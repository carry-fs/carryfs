# How to format a disk

Formatting a partition as CarryFS erases everything on it. Copy off the files
you want to keep first.

Before you start, [install CarryFS](INSTALLATION.md). The partition must be
between 256 MiB and 32 TiB.

To format a CarryFS partition as something else, see
[Erase CarryFS before using another filesystem](#erase-carryfs-before-using-another-filesystem).

## Format a partition as CarryFS

### Linux

1. Find the partition and unmount it.

   ```sh
   lsblk -o NAME,SIZE,FSTYPE,LABEL,MOUNTPOINTS
   udisksctl unmount -b /dev/sdXN
   ```

2. Format it. `-L NAME` sets a label and is optional.

   ```sh
   sudo mkfs -t carryfs -L NAME /dev/sdXN
   ```

If the partition held data before, such as another filesystem or an earlier
CarryFS volume, `mkfs` stops and reports that the device holds data. This keeps
a mistyped device name from erasing the wrong partition. Check the name with
`lsblk`, then add `-f` to erase the old data and format anyway.

```sh
sudo mkfs -t carryfs -f -L NAME /dev/sdXN
```

`mkfs` refuses a mounted partition. It also refuses a whole disk that holds
partitions, because formatting the disk would erase its partition table. Format
one of the partitions instead. To put CarryFS on the whole disk, first remove
the table with `sudo wipefs -a /dev/sdX`.

### FreeBSD and NetBSD

Format an unmounted partition with `newfs_carryfs`. On FreeBSD that is a GEOM
provider such as `/dev/da0p1`. On NetBSD it is a raw partition node such as
the GPT wedge `/dev/rdk2`. `-L NAME` sets a label and is optional.

```sh
sudo newfs_carryfs -L NAME /dev/da0p1
```

As on Linux, `newfs_carryfs` stops if the partition holds data, and `-f`
erases it and formats anyway. It refuses mounted partitions and whole disks
that hold partitions, even with `-f`.

### macOS

In Disk Utility, select the partition, click **Erase**, and choose CarryFS as
the format.

To use the terminal instead, find the partition identifier with
`diskutil list`, then erase it.

```sh
diskutil eraseVolume CarryFS NAME diskNsM
```

### Windows

`carryfs-windows mkfs` formats only a blank partition. It refuses a partition
that Windows has formatted as NTFS, exFAT, or FAT. Run every command below in
PowerShell as Administrator.

1. Make the partition blank. In Disk Management, delete the volume. Then create
   a new simple volume and choose **Do not format this volume**.

2. Find the disk and partition numbers.

   ```powershell
   Get-Partition | Format-Table DiskNumber, PartitionNumber, DriveLetter, Size
   ```

3. Format the partition. This example uses disk 1, partition 2.

   ```powershell
   & "$env:ProgramFiles\CarryFS\carryfs-windows.exe" mkfs \\.\Harddisk1Partition2
   ```

4. Reconnect the disk to mount it.

If the partition held data before, `mkfs` stops and reports that it is not
blank. To erase the old data and format anyway, add `force`.

```powershell
& "$env:ProgramFiles\CarryFS\carryfs-windows.exe" mkfs \\.\Harddisk1Partition2 force
```

`mkfs` also refuses a partition that is in use. If the CarryFS service mounted
the partition, stop the service with `sc stop CarryFS` and run `mkfs` again.

Windows has no label option yet.

## Erase CarryFS before using another filesystem

Other formatting tools don't recognize CarryFS and leave parts of it
behind. A Windows format, for example, leaves the second root record (root B)
in place. If you later create a partition at the same offset, that record
brings the old volume back. Erase CarryFS first, then format with any tool.

The erase zeroes the 2 MiB reserved band at each end of the partition. It
refuses a partition that holds no CarryFS volume, so a mistyped path cannot
damage another filesystem.

### Linux

```sh
sudo mkfs.carryfs --wipe /dev/sdXN
```

The same checks as `mkfs` apply. The partition must be unmounted, and it can't
be a whole disk that holds partitions.

Without the wipe, `wipefs` can't see CarryFS, and GNOME Disks fails with
"Error synchronizing after initial wipe".

### FreeBSD and NetBSD

```sh
sudo newfs_carryfs --wipe /dev/da0p1
```

The same checks as formatting apply.

### macOS

Erase the partition in Disk Utility and choose the new format. The erase also
clears CarryFS.

### Windows

Find the disk and partition numbers as in the format steps above, then wipe
the partition.

```powershell
& "$env:ProgramFiles\CarryFS\carryfs-windows.exe" wipe \\.\Harddisk1Partition2
```

`wipe` prints `wiped CarryFS from ...` when it finishes. Format the partition
with Disk Management or any other tool.

`wipe` also refuses a partition that Windows mounts as NTFS, exFAT, or FAT, and
a partition that is in use. If the CarryFS service mounted the partition, run
`sc stop CarryFS` first.
