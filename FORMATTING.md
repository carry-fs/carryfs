# Formatting a disk

Formatting a partition as CarryFS erases everything on it. Copy off anything you
want to keep first.

CarryFS needs a partition of at least 256 MiB and at most 32 TiB. Install
CarryFS before you start, as [INSTALLATION.md](INSTALLATION.md) describes.

## Linux

Find the partition with `lsblk`, unmount it, and format it.

```sh
lsblk -o NAME,SIZE,FSTYPE,LABEL,MOUNTPOINTS
udisksctl unmount -b /dev/sdXN
sudo mkfs -t carryfs -L NAME /dev/sdXN
```

`-L NAME` is optional. `mkfs` refuses a partition that is still mounted. It
also refuses a whole disk that holds partitions, because formatting it would
erase the partition table. Format one of the partitions instead, or remove the
table with `sudo wipefs -a /dev/sdX` if you want CarryFS on the whole disk.

## macOS

In Disk Utility, select the partition, click Erase, and choose CarryFS as the
format.

From the terminal, find the partition's identifier with `diskutil list`, then
erase it.

```sh
diskutil eraseVolume CarryFS NAME diskNsM
```

## Windows

`carryfs-windows mkfs` formats only a blank partition. It refuses one that
Windows has formatted with NTFS, exFAT, or FAT. To get a blank partition, open
Disk Management, delete the volume, and create a new simple volume with "Do not
format this volume".

Find the disk and partition numbers in PowerShell.

```powershell
Get-Partition | Format-Table DiskNumber, PartitionNumber, DriveLetter, Size
```

Then format it from PowerShell as Administrator. This example uses disk 1,
partition 2.

```powershell
& "$env:ProgramFiles\CarryFS\carryfs-windows.exe" mkfs \\.\Harddisk1Partition2
```

If the partition held data before, `mkfs` stops and says it is not blank. Add
`force` to erase it.

```powershell
& "$env:ProgramFiles\CarryFS\carryfs-windows.exe" mkfs \\.\Harddisk1Partition2 force
```

Windows has no label option yet. Reconnect the disk to mount it.

## Formatting a CarryFS disk as something else

On Linux, wipe CarryFS from the partition first, then format it with any tool.

```sh
sudo mkfs.carryfs --wipe /dev/sdXN
```

`wipefs` and GNOME Disks don't recognize CarryFS. Without the wipe, GNOME Disks
fails with "Error synchronizing after initial wipe". `--wipe` refuses a
partition that doesn't hold CarryFS, so it can't erase another filesystem by
mistake.

On macOS, erase the partition in Disk Utility and choose the new format.

On Windows, eject the CarryFS volume, then format the partition in Disk
Management.
