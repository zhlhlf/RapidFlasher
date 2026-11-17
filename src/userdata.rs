//! TWRP-style `/data` formatting.
//!
//! The important parts are not the `mke2fs` call itself but what TWRP does
//! around it (`bootable/recovery/partition.cpp`, `Wipe_Encryption` /
//! `Wipe_EXTFS`):
//!
//! 1. unmount `/data` and every bind mount below it (emulated storage),
//! 2. open the real block device (device-mapper node when one exists),
//! 3. keep the last 16 KiB free when an FDE crypto footer is present,
//! 4. `mke2fs` only the filesystem area, then zero the footer,
//! 5. restore SELinux labels with `e2fsdroid`.
//!
//! `mke2fs -F` only skips mke2fs' own "is this really a filesystem device"
//! checks. It does *not* allow formatting a mounted filesystem: for that
//! e2fsprogs requires `-F` twice (`-F -F`). We only use that as a last
//! resort after trying to unmount, because writing under a live mount leaves
//! a stale filesystem in the kernel until reboot.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::recovery::RecoveryUI;

/// AOSP FDE crypto footer occupies the last 16 KiB of the userdata partition.
const CRYPT_FOOTER_OFFSET: u64 = 0x4000;
/// `struct crypt_mnt_ftr.magic`, little endian (`CRYPT_MAGIC` in vold).
const CRYPT_MAGIC: u32 = 0xF0D1_B4C0;
/// `_IOR(0x12, 114, size_t)` on a 64-bit kernel.
const BLKGETSIZE64: libc::c_ulong = 0x8008_1272;
const MNT_DETACH: libc::c_int = 2;
const EXT4_SUPER_MAGIC: u16 = 0xEF53;
/// ext4 superblock starts at 1024, `s_magic` at offset 0x38 in it.
const EXT4_MAGIC_OFFSET: u64 = 1024 + 0x38;

/// Prefer the device-mapper node: on metadata-encrypted userdata the
/// encryption layer must stay in place, so formatting the physical
/// partition underneath would write plaintext.
const USERDATA_CANDIDATES: [&str; 3] = [
    "/dev/block/mapper/userdata",
    "/dev/block/by-name/userdata",
    "/dev/block/bootdevice/by-name/userdata",
];

/// Mount points that keep `/data` busy. External SD cards live under
/// `/storage/<uuid>` and are deliberately not matched.
fn is_data_mount(mount_point: &str) -> bool {
    mount_point == "/data"
        || mount_point.starts_with("/data/")
        || mount_point == "/metadata"
        || mount_point == "/sdcard"
        || mount_point.starts_with("/storage/emulated")
        || mount_point.starts_with("/storage/self")
        || mount_point.starts_with("/mnt/runtime/")
        || mount_point.starts_with("/mnt/user/")
        || mount_point.starts_with("/mnt/installer/")
        || mount_point.starts_with("/mnt/androidwritable/")
}

fn mounts_to_unmount(proc_mounts: &str) -> Vec<String> {
    let mut mounts: Vec<String> = proc_mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _source = fields.next()?;
            let mount_point = fields.next()?;
            is_data_mount(mount_point).then(|| mount_point.to_string())
        })
        .collect();
    // Deepest first: /data/media before /data.
    mounts.sort_by_key(|m| std::cmp::Reverse(m.matches('/').count()));
    mounts.dedup();
    mounts
}

fn unmount_data(ui: &mut RecoveryUI) -> Result<()> {
    for attempt in 0..3 {
        let mounts = mounts_to_unmount(&fs::read_to_string("/proc/mounts").unwrap_or_default());
        if mounts.is_empty() {
            return Ok(());
        }

        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }

        for mount_point in mounts {
            let c_path = match std::ffi::CString::new(mount_point.clone()) {
                Ok(path) => path,
                Err(_) => continue,
            };
            let rc = unsafe { libc::umount2(c_path.as_ptr(), 0) };
            if rc == 0 {
                continue;
            }

            let err = io::Error::last_os_error();
            let rc = unsafe { libc::umount2(c_path.as_ptr(), MNT_DETACH) };
            if rc == 0 {
                ui.ui_print(&format!("lazy-unmounted {} ({})", mount_point, err))?;
            } else {
                ui.ui_print(&format!(
                    "cannot unmount {}: {}",
                    mount_point,
                    io::Error::last_os_error()
                ))?;
            }
        }
    }

    let left = mounts_to_unmount(&fs::read_to_string("/proc/mounts").unwrap_or_default());
    if !left.is_empty() {
        bail!("userdata is still mounted: {}", left.join(", "));
    }
    Ok(())
}

fn open_userdata(ui: &mut RecoveryUI) -> Result<(File, PathBuf)> {
    let mut errors = Vec::new();

    for candidate in USERDATA_CANDIDATES {
        match OpenOptions::new().read(true).write(true).open(candidate) {
            Ok(file) => {
                let is_block = file
                    .metadata()
                    .map(|meta| meta.file_type().is_block_device())
                    .unwrap_or(false);
                if !is_block {
                    errors.push(format!("{}: not a block device", candidate));
                    continue;
                }
                ui.ui_print(&format!("userdata device: {}", candidate))?;
                return Ok((file, PathBuf::from(candidate)));
            }
            Err(err) => errors.push(format!("{}: {}", candidate, err)),
        }
    }

    bail!("cannot open userdata block device ({})", errors.join("; "))
}

fn block_size(file: &File) -> Result<u64> {
    let mut size: u64 = 0;
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), BLKGETSIZE64 as _, &mut size) };
    if rc == 0 && size > 0 {
        return Ok(size);
    }

    if let Ok(output) = Command::new("blockdev")
        .args(["--getsize64", "/proc/self/fd/0"])
        .stdin(Stdio::from(file.try_clone()?))
        .output()
    {
        if output.status.success() {
            if let Ok(size) = String::from_utf8_lossy(&output.stdout).trim().parse::<u64>() {
                if size > 0 {
                    return Ok(size);
                }
            }
        }
    }

    bail!("cannot read userdata size: {}", io::Error::last_os_error())
}

fn has_crypt_footer(file: &mut File, size: u64) -> Result<bool> {
    if size <= CRYPT_FOOTER_OFFSET {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(size - CRYPT_FOOTER_OFFSET))?;
    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_err() {
        return Ok(false);
    }
    Ok(u32::from_le_bytes(magic) == CRYPT_MAGIC)
}

fn wipe_crypt_footer(file: &mut File, size: u64) -> Result<()> {
    file.seek(SeekFrom::Start(size - CRYPT_FOOTER_OFFSET))?;
    file.write_all(&[0u8; CRYPT_FOOTER_OFFSET as usize])?;
    file.sync_all()?;
    Ok(())
}

fn find_tool(names: &[&str]) -> Option<String> {
    for name in names {
        if name.contains('/') {
            if Path::new(name).exists() {
                return Some((*name).to_string());
            }
            continue;
        }
        if let Ok(path) = std::env::var("PATH") {
            for dir in path.split(':') {
                let candidate = Path::new(dir).join(name);
                if candidate.exists() {
                    return Some(candidate.to_string_lossy().into_owned());
                }
            }
        }
    }
    None
}

fn run_mke2fs(
    mke2fs: &str,
    device: &Path,
    blocks: u64,
    force_twice: bool,
    with_features: bool,
) -> io::Result<std::process::Output> {
    let mut command = Command::new(mke2fs);
    command.args(["-t", "ext4", "-b", "4096", "-I", "512", "-F"]);
    if force_twice {
        // e2fsprogs only allows formatting a mounted/in-use device with -F twice.
        command.arg("-F");
    }
    if with_features {
        command.args(["-O", "metadata_csum,64bit,extent"]);
    }
    command.arg(device).arg(blocks.to_string());
    command.stdin(Stdio::null()).output()
}

fn print_tail(ui: &mut RecoveryUI, output: &std::process::Output) -> Result<()> {
    let text = String::from_utf8_lossy(&output.stderr);
    for line in text.lines().rev().take(6).collect::<Vec<_>>().into_iter().rev() {
        ui.ui_print(&format!("  mke2fs: {}", line))?;
    }
    Ok(())
}

fn mkfs(ui: &mut RecoveryUI, device: &Path, blocks: u64) -> Result<()> {
    let mke2fs = find_tool(&["mke2fs"])
        .context("mke2fs not found (looked in PATH, /system/bin, /sbin)")?;

    // TWRP uses metadata_csum/64bit/extent on modern devices, but older
    // e2fsprogs builds do not know those feature names. Try rich first,
    // then plain, then force even if the kernel still considers it busy.
    for (force_twice, with_features) in [
        (false, true),
        (false, false),
        (true, true),
        (true, false),
    ] {
        if force_twice {
            ui.ui_print("mke2fs refused, retrying with -F -F (force in-use device)")?;
        }
        let output = run_mke2fs(&mke2fs, device, blocks, force_twice, with_features)
            .with_context(|| format!("failed to run {}", mke2fs))?;
        if output.status.success() {
            return Ok(());
        }
        print_tail(ui, &output)?;
    }

    bail!("mke2fs failed for {}", device.display())
}

fn verify_ext4(file: &mut File) -> Result<bool> {
    file.seek(SeekFrom::Start(EXT4_MAGIC_OFFSET))?;
    let mut magic = [0u8; 2];
    if file.read_exact(&mut magic).is_err() {
        return Ok(false);
    }
    Ok(u16::from_le_bytes(magic) == EXT4_SUPER_MAGIC)
}

fn restore_contexts(ui: &mut RecoveryUI, device: &Path) -> Result<()> {
    let Some(e2fsdroid) = find_tool(&["e2fsdroid"]) else {
        ui.ui_print("e2fsdroid not found, skip SELinux restore")?;
        return Ok(());
    };

    let contexts = [
        "/file_contexts",
        "/system/etc/selinux/plat_file_contexts",
        "/system/etc/selinux/plat_sepolicy.cil",
    ];
    let Some(file_contexts) = contexts.iter().find(|path| Path::new(path).exists()) else {
        ui.ui_print("file_contexts not found, skip SELinux restore")?;
        return Ok(());
    };

    let output = Command::new(e2fsdroid)
        .args(["-e", "-S", file_contexts, "-a", "/data"])
        .arg(device)
        .output()
        .context("failed to run e2fsdroid")?;

    if output.status.success() {
        ui.ui_print("SELinux contexts restored")?;
    } else {
        // Not fatal: init's restorecon recreates the labels on first boot.
        ui.ui_print("warning: e2fsdroid failed (boot will restore contexts)")?;
        print_tail(ui, &output)?;
    }
    Ok(())
}

/// Format `/data` the way TWRP's "Format Data" does.
pub fn format_data(ui: &mut RecoveryUI) -> Result<()> {
    ui.ui_print("unmounting /data and emulated storage ...")?;
    unmount_data(ui)?;

    let (mut device, device_path) = open_userdata(ui)?;
    let size = block_size(&device)?;
    let footer = has_crypt_footer(&mut device, size)?;

    let fs_size = if footer {
        size - CRYPT_FOOTER_OFFSET
    } else {
        size
    };
    let blocks = fs_size / 4096;
    if blocks < 16 * 1024 {
        bail!("userdata too small: {} bytes", size);
    }

    ui.ui_print(&format!(
        "userdata size: {} MiB, crypto footer: {}, format: {} blocks",
        size / 1024 / 1024,
        if footer { "yes" } else { "no" },
        blocks
    ))?;

    ui.ui_print("formatting userdata (ext4) ...")?;
    mkfs(ui, &device_path, blocks)?;

    if footer {
        ui.ui_print("zeroing FDE crypto footer ...")?;
        wipe_crypt_footer(&mut device, size)?;
    }

    if !verify_ext4(&mut device)? {
        bail!("ext4 superblock missing after formatting {}", device_path.display());
    }
    ui.ui_print("ext4 superblock verified")?;

    restore_contexts(ui, &device_path)?;
    ui.ui_print("format userdata ok")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmounts_deepest_data_mounts_first() {
        let table = "\
/dev/block/dm-0 /data ext4 rw 0 0
/dev/block/dm-0 /data/media ext4 rw 0 0
/dev/block/mmcblk1p1 /storage/1234-5678 vfat rw 0 0
tmpfs /storage/emulated tmpfs rw 0 0
";
        assert_eq!(
            mounts_to_unmount(table),
            vec![
                "/data/media".to_string(),
                "/storage/emulated".to_string(),
                "/data".to_string(),
            ]
        );
    }

    #[test]
    fn external_sd_is_not_userdata() {
        assert!(!is_data_mount("/storage/1234-5678"));
        assert!(is_data_mount("/storage/emulated/0"));
        assert!(is_data_mount("/mnt/user/0/primary"));
        assert!(is_data_mount("/data/vendor"));
    }

    #[test]
    fn detects_and_zeroes_footer() {
        let path = std::env::temp_dir().join(format!(
            "rapidflasher-userdata-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let size = 0x10000u64;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.set_len(size).unwrap();
        file.seek(SeekFrom::Start(size - CRYPT_FOOTER_OFFSET)).unwrap();
        file.write_all(&CRYPT_MAGIC.to_le_bytes()).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();

        assert!(has_crypt_footer(&mut file, size).unwrap());
        wipe_crypt_footer(&mut file, size).unwrap();
        assert!(!has_crypt_footer(&mut file, size).unwrap());
        assert_eq!(fs::metadata(&path).unwrap().len(), size);
        let _ = fs::remove_file(path);
    }
}
