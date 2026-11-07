# Native Linux driver

This layer supports read-only CarryFS mounts. File reads verify whole 64 KiB
checksum units. Directory lookups use the shared filename profile.
Writable mounts are rejected.

Build with a Rust-enabled kernel, its matching headers and Rust libraries,
and the exact compiler used to build that kernel:

```sh
bash scripts/kernel-build.sh
```

In a disposable VM, load the module and mount an existing image read-only:

```sh
sudo insmod linux/carryfs.ko
sudo mount -i -t carryfs -o ro DEVICE DIRECTORY
```

The driver requires 4 KiB pages. The default mount helper continues to use FUSE.
The distribution build workflow checks Ubuntu, Fedora, and Arch kernels.

With the module loaded and both userspace binaries built, run the read-only
check inside the VM:

```sh
python3 linux/tests/check.py
```

The check populates a volume through FUSE, verifies native reads and sparse
holes, refuses writes, and checks that the image remains unchanged.
