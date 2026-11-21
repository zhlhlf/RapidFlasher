//! Native port of zhlhlf's shell flashing flow.
//!
//! The original flow is a shell `update-binary` (see zhlhlf/android_flash_scripts)
//! that shells out to `unzip` and `dd` for every image. This module implements the
//! same steps with the primitives RapidFlasher already has: streaming reads from
//! the ZIP, `ThreadedWriter` (buffered + fsync), and `ui_print` progress output.
//!
//! It is triggered when the update script executes a program whose name contains
//! "zhlhlf" (see `matches`).

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use zip::ZipArchive;

use crate::recovery::RecoveryUI;
use crate::threaded_writer::ThreadedWriter;

const PT_DIR: &str = "/dev/block/by-name";
const FW_DIR: &str = "firmware-update";
const WORK_DIR: &str = "/data/tmp";

// Test hooks: point the flow at a scratch directory instead of real partitions.
fn pt_dir() -> String {
    std::env::var("ZHLHLF_PT_DIR").unwrap_or_else(|_| PT_DIR.to_string())
}

fn work_dir() -> String {
    std::env::var("ZHLHLF_WORK_DIR").unwrap_or_else(|_| WORK_DIR.to_string())
}
const WRITE_BUFFER_SIZE: usize = 16 * 1024 * 1024;
const SUPER_GROUP: &str = "qti_dynamic_partitions";

/// True when the update script is trying to run zhlhlf's shell flow.
///
/// The flow is started from `updater-script` only: the bare commands
/// `zhlhlf`, `zhlhlf_flash` and `zhlhlf_flow`, or a `run_program` whose
/// argument basename contains `zhlhlf`. ZIP contents are never inspected to
/// decide this.
pub fn matches(args: &[String]) -> bool {
    args.iter().any(|arg| {
        Path::new(arg)
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.to_lowercase().contains("zhlhlf"))
            .unwrap_or(false)
    })
}

pub fn run(ui: &mut RecoveryUI, archive: &mut ZipArchive<File>) -> Result<()> {
    match run_inner(ui, archive) {
        Ok(()) => Ok(()),
        Err(err) => {
            // Errors used to escape to stderr, which recovery never shows.
            // Report them on the recovery screen before propagating.
            let _ = ui.ui_print(" ");
            let _ = ui.ui_print("-----------------------------------------");
            let _ = ui.ui_print(&format!("FLASH FAILED: {:#}", err));
            let _ = ui.ui_print("partition may be incomplete, flash a full zip again");
            let _ = ui.ui_print("-----------------------------------------");
            Err(err)
        }
    }
}

fn run_inner(ui: &mut RecoveryUI, archive: &mut ZipArchive<File>) -> Result<()> {
    let names = collect_entries(archive)?;
    let fw_images: Vec<String> = names
        .iter()
        .filter(|n| n.starts_with(&format!("{}/", FW_DIR)) && n.ends_with(".img"))
        .cloned()
        .collect();
    let rom_images: Vec<String> = names
        .iter()
        .filter(|n| n.starts_with("images/") && (n.ends_with(".img") || n.ends_with(".zst")))
        .cloned()
        .collect();

    let ddr = detect_ddr()?;
    ui.ui_print(" ")?;
    ui.ui_print(&format!("you device type is: {}", ddr))?;
    ui.ui_print("-----------------------------------------")?;
    ui.ui_print("             Author by zhlhlf")?;
    ui.ui_print("-----------------------------------------")?;

    // A prebuilt super means the zip already holds a complete dynamic-partition
    // set: pin fw and rom to slot a, then switch the device to a. When the zip
    // only carries loose images, the user still picks the slot and the super is
    // built into that slot.
    let prebuilt_super = has_prebuilt_super(&rom_images);
    let super_from_images = !prebuilt_super && has_system_image(&rom_images);
    let rom_flow = prebuilt_super || super_from_images;

    let slot = if prebuilt_super {
        ui.ui_print("super image found: fw and rom will be flashed to slot a only")?;
        'a'
    } else {
        if super_from_images {
            ui.ui_print("no prebuilt super: images will be packed into the slot you choose")?;
        }
        ask_slot(ui, "flash fw  -> a or b?    + is a   ||  - is b")?
    };

    let work = work_dir();
    fs::create_dir_all(&work)?;

    // ---- firmware images -------------------------------------------------
    let has_lp5 = fw_images.iter().any(|e| e == "firmware-update/xbl_lp5.img");
    if has_lp5 {
        ui.ui_print("your device is ddr4/ddr5 fw")?;
        if ddr == "ddr4" {
            flash_zip_entry(ui, archive, "firmware-update/xbl.img", &part_dest("xbl.img", slot))?;
            flash_zip_entry(
                ui,
                archive,
                "firmware-update/xbl_config.img",
                &part_dest("xbl_config.img", slot),
            )?;
        } else {
            flash_zip_entry(
                ui,
                archive,
                "firmware-update/xbl_lp5.img",
                &part_dest("xbl.img", slot),
            )?;
            flash_zip_entry(
                ui,
                archive,
                "firmware-update/xbl_config_lp5.img",
                &part_dest("xbl_config.img", slot),
            )?;
        }
    }

    for entry in &fw_images {
        if has_lp5 && basename(entry).starts_with("xbl") {
            continue;
        }
        flash_zip_entry(ui, archive, entry, &part_dest(basename(entry), slot))?;
    }
    ui.ui_print(" ")?;
    ui.ui_print("fw all write done")?;
    ui.ui_print(" ")?;

    // ---- super / dynamic partitions --------------------------------------
    if rom_images.iter().any(|e| e == "images/super.zst") {
        flash_zip_entry_zstd(ui, archive, "images/super.zst", &format!("{}/super", pt_dir()))?;
    } else if rom_images.iter().any(|e| e == "images/super.img") {
        // The shell flow wrote this one to `super_<slot>`, but `super` itself is
        // not slot-suffixed on A/B devices. Keep it unsuffixed.
        flash_zip_entry(ui, archive, "images/super.img", &format!("{}/super", pt_dir()))?;
    } else if rom_images.iter().any(|e| e == "images/system.img") {
        build_and_flash_super(ui, archive, &rom_images, slot)?;
        ui.ui_print("super write done")?;
    }

    // ---- boot / vbmeta / dtbo --------------------------------------------
    if rom_images.iter().any(|e| e == "images/boot.img") {
        for entry in &rom_images {
            let name = basename(entry);
            if name.contains("boot") || name.contains("vbmeta") || name.contains("dtbo") {
                flash_zip_entry(ui, archive, entry, &part_dest(name, slot))?;
            }
        }
    }

    if rom_flow {
        ensure_active_slot(ui, slot)?;
    }

    maybe_format_userdata(ui)?;

    ui.ui_print(" ")?;
    ui.ui_print("-----------------------------------------")?;
    ui.ui_print("              all done!")?;
    ui.ui_print("-----------------------------------------")?;
    ui.ui_print(" ")?;

    let _ = fs::remove_dir_all(&work);
    Ok(())
}

fn collect_entries(archive: &mut ZipArchive<File>) -> Result<Vec<String>> {
    let mut names = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let entry = archive.by_index(i)?;
        names.push(entry.name().to_string());
    }
    Ok(names)
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// True when the zip ships a ready-made super image.
fn has_prebuilt_super(rom_images: &[String]) -> bool {
    rom_images
        .iter()
        .any(|e| e == "images/super.zst" || e == "images/super.img")
}

/// True when the zip carries loose images that `build_and_flash_super` packs
/// into a super of its own.
fn has_system_image(rom_images: &[String]) -> bool {
    rom_images.iter().any(|e| e == "images/system.img")
}

fn part_dest(name: &str, slot: char) -> String {
    let name = name.strip_suffix(".img").unwrap_or(name);
    format!("{}/{}_{}", pt_dir(), name, slot)
}

/// Make sure the device boots into the slot that was just written.
fn ensure_active_slot(ui: &mut RecoveryUI, slot: char) -> Result<()> {
    let current = crate::func::get_active_slot_suffix().unwrap_or_default();
    let normalized = current.trim_start_matches('_').to_lowercase();

    if current.is_empty() {
        ui.ui_print("current slot: unknown")?;
    } else {
        ui.ui_print(&format!("current slot: {}", current))?;
    }

    if normalized == slot.to_string() {
        ui.ui_print(&format!("active slot is {}, no switch needed", slot))?;
        return Ok(());
    }

    let index = if slot == 'a' { "0" } else { "1" };
    ui.ui_print(&format!("switching active slot to {} ...", slot))?;
    crate::func::set_slot(ui, index)?;

    // ro.boot.slot_suffix only changes after reboot, so ask bootctl instead.
    match bootctl_active_slot() {
        Some(value) if value == index => ui.ui_print(&format!(
            "active slot set to {} (takes effect on reboot)",
            slot
        ))?,
        Some(value) => ui.ui_print(&format!("warning: bootctl reports active slot {}", value))?,
        None => ui.ui_print(&format!(
            "active slot switched to {} (takes effect on reboot)",
            slot
        ))?,
    }

    Ok(())
}

/// `bootctl get-active-boot-slot` prints 0 (slot a) or 1 (slot b).
fn bootctl_active_slot() -> Option<String> {
    let output = Command::new("bootctl")
        .arg("get-active-boot-slot")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn detect_ddr() -> Result<String> {
    if let Ok(text) = fs::read_to_string("/proc/devinfo/ddr_type") {
        let lower = text.to_lowercase();
        if lower.contains("ddr4") {
            return Ok("ddr4".to_string());
        }
        if lower.contains("ddr5") {
            return Ok("ddr5".to_string());
        }
    }

    let prop = Command::new("getprop")
        .arg("ro.boot.ddr_type")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default();

    if prop.contains('0') {
        return Ok("ddr4".to_string());
    }
    if prop.contains('1') {
        return Ok("ddr5".to_string());
    }

    bail!("ddr get fail");
}

fn ask_slot(ui: &mut RecoveryUI, prompt: &str) -> Result<char> {
    ui.ui_print(prompt)?;
    ui.ui_print(" ")?;
    let slot = match wait_volume_key(Duration::from_secs(5)) {
        Some(1) => 'a',
        Some(0) => 'b',
        _ => 'a',
    };
    ui.ui_print(&format!("selected slot: {}", slot))?;
    Ok(slot)
}

/// Returns 1 for VOLUMEUP, 0 for VOLUMEDOWN, None on timeout/unavailable.
fn wait_volume_key(timeout: Duration) -> Option<u8> {
    let deadline = Instant::now() + timeout;

    // `getevent -c 1` exits after the first input event, which may be a key
    // release, a touch event or a SYN_REPORT rather than the press we want.
    // Keep a fresh listener running until the deadline and ignore anything
    // that is not a volume-key press.
    loop {
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        let remaining = deadline - now;

        let mut child = Command::new("getevent")
            .args(["-qlc", "1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(_) => break,
            }
            if started.elapsed() >= remaining {
                let _ = child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let output = match child.wait_with_output() {
            Ok(output) => output,
            Err(_) => return None,
        };
        let text = String::from_utf8_lossy(&output.stdout);

        for line in text.lines() {
            if line.contains("KEY_VOLUMEUP") && line.contains("DOWN") {
                return Some(1);
            }
            if line.contains("KEY_VOLUMEDOWN") && line.contains("DOWN") {
                return Some(0);
            }
        }
        // Non-key event or a key release: listen again until the deadline.
    }
}

fn flash_zip_entry(
    ui: &mut RecoveryUI,
    archive: &mut ZipArchive<File>,
    entry: &str,
    dest: &str,
) -> Result<()> {
    ui.ui_print(&format!("flash {} -> {}", basename(entry), basename(dest)))?;
    let mut source = archive
        .by_name(entry)
        .with_context(|| format!("{} not found in zip", entry))?;
    let mut writer = ThreadedWriter::new(dest.to_string(), WRITE_BUFFER_SIZE);
    io::copy(&mut source, &mut writer)?;
    writer.finish()?;
    Ok(())
}

fn flash_zip_entry_zstd(
    ui: &mut RecoveryUI,
    archive: &mut ZipArchive<File>,
    entry: &str,
    dest: &str,
) -> Result<()> {
    ui.ui_print(&format!("flash {} -> {}", basename(entry), basename(dest)))?;
    let mut source = archive
        .by_name(entry)
        .with_context(|| format!("{} not found in zip", entry))?;
    let mut writer = ThreadedWriter::new(dest.to_string(), WRITE_BUFFER_SIZE);
    zstd::stream::copy_decode(&mut source, &mut writer)?;
    writer.finish()?;
    Ok(())
}

fn flash_local_file(ui: &mut RecoveryUI, source: &str, dest: &str) -> Result<()> {
    ui.ui_print(&format!(
        "flash {} -> {}",
        basename(source),
        basename(dest)
    ))?;
    let mut input = File::open(source).with_context(|| format!("open {}", source))?;
    let mut writer = ThreadedWriter::new(dest.to_string(), WRITE_BUFFER_SIZE);
    io::copy(&mut input, &mut writer)?;
    writer.finish()?;
    Ok(())
}

fn extract_to_file(
    archive: &mut ZipArchive<File>,
    entry: &str,
    dest: &str,
) -> Result<()> {
    if let Some(parent) = Path::new(dest).parent() {
        fs::create_dir_all(parent)?;
    }
    let mut source = archive
        .by_name(entry)
        .with_context(|| format!("{} not found in zip", entry))?;
    let mut writer = ThreadedWriter::new(dest.to_string(), WRITE_BUFFER_SIZE);
    io::copy(&mut source, &mut writer)?;
    writer.finish()?;
    Ok(())
}

fn build_and_flash_super(
    ui: &mut RecoveryUI,
    archive: &mut ZipArchive<File>,
    rom_images: &[String],
    slot: char,
) -> Result<()> {
    check_data_size()?;

    let project_dir = format!("{}/project/images", work_dir());
    fs::create_dir_all(&project_dir)?;

    ui.ui_print("extract images/* ...")?;
    for entry in rom_images {
        let name = basename(entry);
        if name == "super.img" || name == "super.zst" || !name.ends_with(".img") {
            continue;
        }
        extract_to_file(archive, entry, &format!("{}/{}", project_dir, name))?;
    }

    // Partition payloads that live inside super. boot/vbmeta/dtbo are flashed
    // to their own partitions later, exactly like the shell flow does.
    let mut parts: Vec<(String, String, u64)> = Vec::new();
    for entry in rom_images {
        let name = basename(entry);
        if !name.ends_with(".img") || name == "super.img" {
            continue;
        }
        if name.starts_with("boot")
            || name.starts_with("vbmeta")
            || name.starts_with("dtbo")
        {
            continue;
        }
        let part = name.trim_end_matches(".img").to_string();
        let path = format!("{}/{}", project_dir, name);
        let size = fs::metadata(&path)
            .with_context(|| format!("stat {}", path))?
            .len();
        parts.push((part, path, size));
    }

    if parts.is_empty() {
        bail!("no images found to build super");
    }

    let device_size = query_super_size()?;
    let group_size = device_size.saturating_sub(1024 * 1024);
    let lpmake = extract_lpmake(archive)?;
    let output = format!("{}/project/super.img", work_dir());

    let mut args: Vec<String> = vec![
        "--metadata-size".into(),
        "65536".into(),
        "--super-name".into(),
        "super".into(),
        "--device".into(),
        format!("super:{}", device_size),
        "--metadata-slots".into(),
        "3".into(),
        "--virtual-ab".into(),
        "--group".into(),
        format!("{}_a:{}", SUPER_GROUP, group_size),
        "--group".into(),
        format!("{}_b:{}", SUPER_GROUP, group_size),
    ];

    let (active, inactive) = match slot {
        'a' => ("a", "b"),
        _ => ("b", "a"),
    };

    for (name, path, size) in &parts {
        ui.ui_print(&format!(
            "  - {} size {}",
            name, size
        ))?;
        args.push("--partition".into());
        args.push(format!("{}_{}:none:{}:{}_{}", name, active, size, SUPER_GROUP, active));
        args.push("--image".into());
        args.push(format!("{}_{}={}", name, active, path));
        args.push("--partition".into());
        args.push(format!("{}_{}:none:0:{}_{}", name, inactive, SUPER_GROUP, inactive));
    }

    args.push("-F".into());
    args.push("--output".into());
    args.push(output.clone());

    let log_path = format!("{}/project/make_super.txt", work_dir());
    let log = File::create(&log_path)?;
    let log_err = log.try_clone()?;
    let status = Command::new(&lpmake)
        .args(&args)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .status()
        .context("failed to run lpmake")?;

    if !status.success() || !Path::new(&output).exists() {
        let text = fs::read_to_string(&log_path).unwrap_or_default();
        for line in text.lines() {
            let _ = ui.ui_print(line);
        }
        bail!("make super fail");
    }

    ui.ui_print("super built, flashing...")?;
    flash_local_file(ui, &output, &format!("{}/super", pt_dir()))
}

fn check_data_size() -> Result<()> {
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        let path = b"/data\0";
        if libc::statvfs(path.as_ptr() as *const libc::c_char, &mut stat) != 0 {
            bail!("data mount fail");
        }
        let free = stat.f_bavail as u64 * stat.f_frsize as u64;
        if free < 20 * 1024 * 1024 * 1024 {
            bail!("data mount fail: need more than 20G free on /data");
        }
    }
    Ok(())
}

fn query_super_size() -> Result<u64> {
    if let Ok(out) = Command::new("blockdev")
        .args(["--getsize64", "/dev/block/by-name/super"])
        .output()
    {
        if out.status.success() {
            if let Ok(size) = String::from_utf8_lossy(&out.stdout).trim().parse::<u64>() {
                return Ok(size);
            }
        }
    }

    if let Ok(out) = Command::new("sgdisk")
        .args(["/dev/block/sda", "--print"])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() >= 3 && fields.iter().any(|f| *f == "super") {
                if let (Ok(start), Ok(end)) = (fields[1].parse::<u64>(), fields[2].parse::<u64>()) {
                    return Ok((end.saturating_sub(start) + 1) * 4096);
                }
            }
        }
    }

    bail!("cannot determine super partition size")
}

fn extract_lpmake(archive: &mut ZipArchive<File>) -> Result<String> {
    let dest = "/tmp/lpmake";
    if Path::new(dest).exists() {
        return Ok(dest.to_string());
    }

    for entry in ["META-INF/bin/lptools/lpmake", "scripts/lpmake"] {
        if archive.by_name(entry).is_ok() {
            extract_to_file(archive, entry, dest)?;
            let mut perms = fs::metadata(dest)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(dest, perms)?;
            return Ok(dest.to_string());
        }
    }

    bail!("lpmake not found in zip")
}

fn maybe_format_userdata(ui: &mut RecoveryUI) -> Result<()> {
    ui.ui_print(" ")?;
    ui.ui_print("FORMAT DATA (force, keep encryption)?")?;
    ui.ui_print("  + (VOLUMEUP)   = format data (ERASE ALL DATA)")?;
    ui.ui_print("  - (VOLUMEDOWN) = skip")?;
    ui.ui_print("  (6s timeout -> skip)")?;
    ui.ui_print(" ")?;

    if wait_volume_key(Duration::from_secs(6)) != Some(1) {
        ui.ui_print("skip format data")?;
        return Ok(());
    }

    ui.ui_print("FORMAT DATA started ...")?;
    match crate::userdata::format_data(ui) {
        Ok(()) => {}
        Err(err) => ui.ui_print(&format!("format data fail: {err:#}"))?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_zhlhlf() {
        assert!(matches(&[
            "/tmp/scripts/zhlhlf".to_string(),
            "3".to_string()
        ]));
        assert!(matches(&["/tmp/zhlhlf".to_string()]));
        assert!(matches(&["sh".to_string(), "scripts/zhlhlf".to_string()]));
        assert!(!matches(&["/tmp/busybox".to_string(), "sh".to_string()]));
    }

    #[test]
    fn super_detection() {
        assert!(has_prebuilt_super(&["images/super.zst".to_string()]));
        assert!(has_prebuilt_super(&["images/super.img".to_string()]));
        assert!(!has_prebuilt_super(&["images/system.img".to_string()]));

        assert!(has_system_image(&["images/system.img".to_string()]));
        assert!(!has_system_image(&["images/super.zst".to_string()]));
        assert!(!has_system_image(&["images/boot.img".to_string()]));
        assert!(!has_system_image(&[]));
    }

    #[test]
    fn partition_destination_keeps_slot() {
        assert_eq!(part_dest("xbl.img", 'a'), "/dev/block/by-name/xbl_a");
        assert_eq!(part_dest("boot", 'b'), "/dev/block/by-name/boot_b");
        assert_eq!(basename("firmware-update/xbl.img"), "xbl.img");
    }

}
