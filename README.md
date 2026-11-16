# RapidFlasher
A high-performance Android update-binary. Written in Rust, this binary is designed to be included in Android ROM Flashable ZIP packages to handle installation logic, partition flashing, and dynamic partition management with modern features like ZSTD decompression and multi-threaded I/O.

***
## Features ##
- Speed: Multi-threaded buffered writing for faster extraction and flashing.
- Compression Support: Native support for ZSTD and GZIP compressed images.
- Sparse Images: Native handling of Android Sparse images (normal and sparsechunk).
- Dynamic Partitions: Built-in logic to resize, unmap, and create logical partitions (super partition) using static lptools binaries.
- Simplified Scripting: parses a shell-like updater-script.
- AVB Control: Ability to disable vbmeta verification on the fly.

***
## Usage ##
To use this binary, you must structure your ZIP file correctly. The Android Recovery expects the binary to be located at `META-INF/com/google/android/update-binary`
````
Flashable.zip
└── META-INF
    └── com
        └── google
            └── android
                ├── update-binary      <-- This compiled binary
                └── updater-script     <-- Your installation instructions
````
or you can just check/use `FLASHABLE_ZIP_TEMPLATE` folder.

### Supported Commands ###
Below is the list of commands you can use in your updater-script.

| Command                     | Arguments                | Description
|-----------------------------|--------------------------|--------------------------------------------------------------------------------------------------------------|
| `ui_print`                  | `<message>`              | Prints a message to the recovery screen.                                                                     |
| `show_progress`             | `<fraction> <secs>`      | Updates the recovery progress bar.                                                                           |
| `verify_device`             | `device1,device2,...`    | Aborts installation if the device model (`ro.product.device` or `ro.build.product`) does not match the list. |
| `verify_md5sum`             | `<file> <md5sumfile>`    | Verifies the MD5 of a file based on a file containing the correct md5sum. Aborts flashing if it doesn't match|
| `package_extract_file`      | `<file> <dest_path>`     | Extracts a single file from the ZIP to the system.                                                           |
| `package_flash_partition`   | `<method> <file> <dest>` | Flashes an image to a partition. See Flash Methods below.                                                    |
| `package_extract_targz`     | `<file> <dest_dir>`      | Extracts a GZIP-compressed tar archive from the ZIP to a directory.                                          |
| `update_dynamic_partitions` | `<op_list_file>`         | Modifies logical partitions based on a config file inside the ZIP.                                           |
| `set_slot`                  | `<slot>` *(0/1)*         | Sets the active boot slot using bootctl.                                                                     |
| `disable_vbmeta`            | *(none)*                 | Disables AVB verification (verity/verification) using avbctl.                                                |
| `run_program`               | `<cmd/file> [args...]`   | Executes a script or binary. Automatically applies proper file perm (`chmod 755`) to file before running.    |

### Flash Methods ###
When using package_flash_partition, the first argument determines how the source file is handled:
- 0 (ZSTD): Decompresses a ZSTD file stream directly to the partition.
- 1 (GZIP): Decompresses a GZIP file stream directly to the partition.
- 2 (Sparse): Flashes an Android Sparse Image.
    Auto-split detection: If the file in the zip ends in `.*`, the binary will automatically find and flash split chunks (e.g., `system.img.001`, `system.img.002`,...).

### Dynamic Partitions Guide ###
To resize or modify logical partitions, create a text file (e.g., dynamic_partitions_op_list) in your ZIP and call it via the script:
```` shell
update_dynamic_partitions "dynamic_partitions_op_list"
````
for dynamic_partitions_op_list format, refer to this [README](op_list.md)

Note: This feature requires lpdump, lpmake, and lptools binaries to be present in META-INF/bin/lptools/ inside the ZIP.

### MD5 Verification ###
The `verify_md5sum` command allows you to verify the integrity of your files directly from the ZIP into memory (zero disk I/O) before flashing anything. This uses a "fail-fast" approach: if a file is corrupted, the installation aborts immediately, preventing a soft-bricked device.

To use this, place a standard text file containing the expected MD5 hash alongside your target file inside the ZIP.
**Usage in `updater-script`:**
It is highly recommended to stack all verification commands at the absolute top of your script, before any partition-altering commands execute.

````shell
verify_md5sum "images/boot.img" "images/boot.img.md5sum"
verify_md5sum "super.img.zst" "super.img.zst.md5sum"

# ... proceed with flashing ...
````

### Example (package_flash_partition package_extract_file package_extract_targz) ###
```` shell
# Flash super image (ZSTD)
package_flash_partition "0" "super.img.zst" "/dev/block/bootdevice/by-name/super"
# Flash super image (GZIP)
package_flash_partition "1" "super.img.gz" "/dev/block/bootdevice/by-name/super"
# Flash super image (Sparse)
package_flash_partition "2" "super.img" "/dev/block/bootdevice/by-name/super"
# Flash super image (Sparsechunk)
package_flash_partition "2" "super.img.*" "/dev/block/bootdevice/by-name/super"

# Flash boot.img to boot partition to current active slot (yea it could do that. usual values are _a/_b)
package_extract_file "boot.img" "/dev/block/bootdevice/by-name/boot${SLOT}"

# Extract tar.gz to a directory
package_extract_targz "oplus.tar.gz" "/data/oplus-partitions"
````

***
## zhlhlf ROM Flow ##
This binary ships a native implementation of the [zhlhlf](https://github.com/zhlhlf/android_flash_scripts) flash flow (the `update-binary`-less shell installer used by OnePlus/Oplus-style ROM packages). It does everything in-process: no `unzip`, no `dd`, no external shell scripts.

### Triggering ###
By default the flow runs **automatically** — no `updater-script` entry needed. After opening the ZIP the binary inspects the entry list; if it looks like a zhlhlf ROM package it takes over immediately and never reads `updater-script`.

A ZIP is detected as a zhlhlf ROM when either of these is true:

| Condition | Example |
|---|---|
| Any entry name contains `zhlhlf` | `scripts/zhlhlf`, `META-INF/com/google/android/updater-script-zhlhlf` |
| Both `firmware-update/*.img` **and** a ROM image under `images/` exist | `firmware-update/xbl.img` + `images/super.zst` |

The `images/` file must be one of `super.zst`, `super.img`, or `system.img`.

Recovery log then starts with:

````
ui_print zhlhlf rom detected, running native flash flow
````

#### Manual / explicit trigger ####
Kept for compatibility; not required for normal use.

- A `run_program` argument whose basename contains `zhlhlf`:
  ````shell
  run_program "/tmp/scripts/zhlhlf" "3" "1" "rom.zip"
  ````
  Note: arguments are whitespace separated. The shell function form `run_program("...")` is **not** parsed and the flow will not start.
- A bare `zhlhlf` or `zhlhlf_flash` command anywhere in `updater-script`.

#### Disabling auto-detection ####
Set `ZHLHLF_AUTO=0` in the recovery environment to restore the normal `updater-script` parser.

### What the flow does ###
1. **DDR type** — reads `/proc/devinfo/ddr_type`, falling back to `getprop ro.boot.ddr_type` (`0` = DDR4, `1` = DDR5).
2. **Slot select** — `getevent -qlc 1` watches the volume keys for 5 seconds; timeout defaults to slot `a`.
3. **Firmware** — every `firmware-update/*.img` is streamed to `/dev/block/by-name/<name>_<slot>`. When `xbl_lp5.img` is present, `xbl`/`xbl_config` are chosen by DDR type and the other variants are skipped.
4. **super** — `images/super.zst` is decompressed on the fly straight into `super`; `images/super.img` is written as-is (no slot suffix).
5. **super from system.img** — if only `images/system.img` exists, `super` is assembled with `lpmake` from inside the ZIP: `--metadata-size 65536`, group `qti_dynamic_partitions`, `--metadata-slots 3`, virtual A/B, inactive slot sized 0, and `boot`/`vbmeta`/`dtbo` excluded (they get their own partitions).
6. **boot / vbmeta / dtbo** — written to their own partitions with the selected slot.
7. **userdata** — optionally formatted with `mke2fs -t ext4`.

Every write goes through the same buffered writer used by the rest of the binary (16 MB buffer) and ends with an `fsync(2)` on that target before returning.

### Build with the zhlhlf flow ###
Same as any other target of this project — see [Build](#build). No extra feature flags.

***
## Build ##

### Local (Android arm64) ###
Requires Rust (edition 2024 ⇒ 1.85+) and an Android NDK.

````shell
rustup target add aarch64-linux-android
export NDK=$ANDROID_NDK_HOME
export CC_aarch64_linux_android="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android21-clang"
export AR_aarch64_linux_android="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ar"
cargo build --release --target aarch64-linux-android
````

The result is `target/aarch64-linux-android/release/update-binary`.

### Packaging ###
`./package.sh [binary] [out_dir] [name]` wraps the binary into a minimal flashable ZIP:

````
update-binary-<sha8>.zip
└── META-INF
    └── com
        └── google
            └── android
                ├── update-binary    <-- compiled binary
                └── updater-script   <-- calls zhlhlf_flow
````

Drop your `firmware-update/` and `images/` payloads next to `META-INF` and the ZIP is ready to flash. The default name is the first 8 characters of the commit SHA.

### CI ###
`.github/workflows/build-arm64.yml` builds `aarch64-linux-android` on every push, pull request and manual dispatch. The flashable ZIP, the raw binary and its `.sha256` are uploaded as the `update-binary-arm64` artifact.

Releases:

| Trigger | Result |
|---|---|
| Manual dispatch (`workflow_dispatch`) | GitHub Release named after the first **8 characters of the commit SHA** (e.g. `78a8275`). Assets: `update-binary-<sha8>.zip` (with `META-INF/com/google/android/{update-binary,updater-script}`), the raw `update-binary`, and `update-binary.sha256`. Re-running the same commit overwrites the assets. |
| Tag push `v*` | GitHub Release on that tag, same assets. |

Builds from ordinary pushes and pull requests only produce an artifact — no release.

## Note ##
You can remove `META-INF/bin/lptools` if you don't plan on using `dynamic_partitions_op_list`