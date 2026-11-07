# Native Linux driver

The experimental native driver supports CarryFS reads and writes. Writable
mounts validate structure and block ownership before root repair, allocation,
or reclamation. Publications flush data and metadata, root A, then root B
before acknowledging success. Read-only mounts never write to the device.

Build with a Rust-enabled kernel, its matching headers and Rust libraries,
and the exact compiler used to build that kernel:

```sh
bash scripts/kernel-build.sh
```

Load and mount explicitly in a disposable VM:

```sh
sudo insmod linux/carryfs.ko
sudo mount -i -t carryfs DEVICE DIRECTORY
```

Ordinary mounts continue to use FUSE. The native driver requires 4 KiB pages
and refuses `sync` and `dirsync` mounts. Files report mode 644 and directories
report mode 755. Access times, ownership changes, hard links, and execute bits
are not stored.

With the module loaded and the userspace CLI built, run the native checks in
the VM:

```sh
python3 linux/tests/check.py --entries 1000 --fsx-operations 500 --seed 17
```

The checks exercise namespace changes, file I/O, concurrent writeback,
reclamation, corruption rejection, and byte comparisons after remount.
