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

## Test

The host tests cover USB transport, partition discovery, and file operations
against a simulated USB drive:

```sh
cargo test -p carryfs-android
```

The instrumented tests run the documents provider on an emulator or device.
They need a desktop-made image at the app's `files/test.img` that holds
`/hello.txt` with the text `hello from a desktop`:

```sh
printf 'hello from a desktop\n' > hello.txt
carryfs mkfs test.img 268435456
carryfs put test.img hello.txt /hello.txt
./gradlew installDebug installDebugAndroidTest
adb push test.img /data/local/tmp/test.img
adb shell run-as com.carryfs.android sh -c "'mkdir -p files && cp /data/local/tmp/test.img files/'"
adb shell am instrument -w com.carryfs.android.test/androidx.test.runner.AndroidJUnitRunner
```

To measure copy speed on a real drive, plug it in, grant USB access in the
app, eject it, and run the benchmark. It writes, reads, and deletes a 256 MiB
file:

```sh
adb shell am instrument -r -w -e usb true -e class com.carryfs.android.UsbBenchmark com.carryfs.android.test/androidx.test.runner.AndroidJUnitRunner
```

Debug builds show a **Mount test image** button that mounts the same image
without a USB drive.
