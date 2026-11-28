# CarryFS for Android

The app shows CarryFS volumes on a USB drive in the Files app and in every
app's file picker. Android can't mount a CarryFS partition, so the app reads
the drive itself:

- `native/` is the Rust adapter. It drives the USB drive in user space
  through usbfs (Bulk-Only Transport and SCSI), finds CarryFS volumes in GPT
  or MBR partitions or on the whole disk, and runs the storage core.
- `app/` is the Kotlin app. A `DocumentsProvider` presents each volume as a
  storage root, and a foreground service keeps the drive mounted.

Android 8 (API 26) or later and an arm64 device with USB host (OTG) support
are required.

## Limits

- Only apps that use the system file picker or the Files app see the files.
  There is no file system path, and the media library doesn't index the
  drive.
- Symbolic links are not shown. Hard links show as separate files.
- Eject from the app or its notification before unplugging. Changes not yet
  published are lost when the drive is removed. Published data stays intact.
- When the app takes the drive, Android unmounts any FAT or exFAT partition on
  it until the drive is plugged in again.
- Android can't read CarryFS, so it may show "Issue with USB drive. Tap to
  fix". Swipe the notification away. Its fix formats the drive, which erases
  the CarryFS volume.
- Drives that support only UAS, and card readers with the card on a LUN
  other than 0, are not supported.
- The app doesn't format, check, or repair volumes yet. Use a desktop.
- Document grants last for the current mount. After reconnecting a drive or
  restarting the app process, select the file again in the system picker.

## Build

Install the NDK `30.0.16248370` with the SDK manager. Then add the Rust
target and cargo-ndk to the toolchain that `cargo` resolves to:

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk
```

Then build the APK. The Gradle build compiles the Rust library first.

```sh
./gradlew assembleDebug
```

For an x86_64 emulator, add the `x86_64-linux-android` Rust target and build
with `-Pcarryfs.abi=x86_64`.

## Test

The host tests cover USB transport, partition discovery, and file operations
against a simulated USB drive:

```sh
cargo test -p carryfs-android
```

The instrumented tests run the documents provider on an emulator or device.
They use a desktop-made image at the app's `files/test.img` with a regular file
and a directory containing two hard links to the same file:

```sh
printf 'hello from a desktop\n' > hello.txt
carryfs mkfs test.img 268435456
carryfs put test.img hello.txt /hello.txt
mkdir links
cp hello.txt links/original.txt
ln links/original.txt links/alias.txt
carryfs put test.img links /links
./gradlew installDebug installDebugAndroidTest
adb push test.img /data/local/tmp/test.img
adb shell run-as com.carryfs.android sh -c "'mkdir -p files && cp /data/local/tmp/test.img files/'"
adb shell am instrument -w com.carryfs.android.test/androidx.test.runner.AndroidJUnitRunner
```

To run the document workload for CarryFS alone, plug in the USB drive,
grant access in the app, eject it, and run:

```sh
adb shell am instrument -r -w -e usb true -e rounds 10 \
  -e class com.carryfs.android.UsbBenchmark com.carryfs.android.test/androidx.test.runner.AndroidJUnitRunner
```

For a run on the phone's internal storage, create a 1 GiB CarryFS image
and copy it to the debug app:

```sh
carryfs mkfs benchmark.img 1073741824
adb push benchmark.img /data/local/tmp/benchmark.img
adb shell run-as com.carryfs.android sh -c "'mkdir -p files && cp /data/local/tmp/benchmark.img files/benchmark.img'"
adb shell am instrument -r -w -e image true -e rounds 10 \
  -e class com.carryfs.android.UsbBenchmark com.carryfs.android.test/androidx.test.runner.AndroidJUnitRunner
```

Both commands run the same workload as the separate APK and print each
sample. Image runs measure the provider and core against internal storage.
Add `-e smallOnly true` to either command to run only small-file creation,
listing, and removal.

Debug builds show a **Mount test image** button that mounts `files/test.img`
without a USB drive.

## Compare USB filesystems

The benchmark APK uses the system file picker to exercise CarryFS and Android's
native USB storage provider from the same client. It needs Wireless debugging
for ADB, an unlocked phone, and a local folder grant for each volume. CarryFS
also needs the app's usual USB permission. Root access is unnecessary.

Use the same SSD, USB adapter and cable for both runs. Existing writable
volumes work if each has at least 1 GiB free. If you need to prepare a test
partition, format it on a desktop after backing up the disk. Use exFAT as the
baseline when the phone supports it, or FAT32 on devices that expose only
FAT32. Keep the phone idle and record the format, partition size, existing data
and free space. A USB 2.0 hub can help troubleshoot adapters that time out when
switching from UAS to Bulk-Only Transport. Use the same hub for both sides if
you do.

From `android/`, install the CarryFS and benchmark APKs before mounting the drive.

```sh
./gradlew :app:installDebug :benchmark:installDebug
```

Run the native baseline while Android owns the drive. If CarryFS opens
automatically, eject in CarryFS. Android then mounts the drive's own volumes
again within a few seconds. Check `adb shell sm list-volumes public` for the
mounted volume's UUID and verify its filesystem on the desktop or in the phone's
mount diagnostics. Replace `1234-ABCD` with that UUID, choose a folder on that
volume in the picker, and grant access.

```sh
adb shell am start -S -n com.carryfs.benchmark/.BenchmarkActivity \
  --es filesystem exfat --es volume 1234-ABCD --ei rounds 10
```

Use `--es filesystem fat32` for a FAT32 partition.

When the baseline completes, eject the volume through Android, mount the
drive from CarryFS, and run the same number of rounds. Select the CarryFS
volume in the picker.

```sh
adb shell am start -S -n com.carryfs.benchmark/.BenchmarkActivity \
  --es filesystem carryfs --ei rounds 10
```

Each round writes and syncs a 512 MiB file in 128 KiB requests, reads it back
and verifies every byte, then creates, lists and removes 1,000 files of 4 KiB.
The benchmark creates and removes only its own directories. If interrupted,
it can leave a `carryfs-bench-*` directory behind. The read immediately
follows the write and can hit cache. It is reported twice: as the time spent
in reads, and as wall time including the byte checks. Compare the wall time.
The small-file timings use ordinary document operations without explicit
file or directory syncs.

The app shows the result filename when all rounds pass. Retrieve it from the
debug APK's private storage. Replace the filename with the one shown or use
`adb logcat -d -s CarryFsBench` to find it.

```sh
adb exec-out run-as com.carryfs.benchmark cat files/results-exfat-RUN-ID.tsv > exfat.tsv
```

Keep the per-round TSVs with the phone model, Android build, disk, USB
connection, CarryFS revision and run order. Report medians and ranges. The
workloads measure Android's document provider paths, including both providers
and their underlying drivers, so keep the results alongside the POSIX desktop
suite.
