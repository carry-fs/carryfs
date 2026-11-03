# Out-of-tree build of the CarryFS kernel module against the running kernel.
.DEFAULT_GOAL := help

KDIR  ?= /lib/modules/$(shell uname -r)/build
# Rust modules must use the compiler that built the kernel's Rust crates;
# Ubuntu ships it as rustc-1.93 for its 7.0 kernels.
RUSTC ?= rustc-1.93

# Largest stack frame allowed per function, in bytes. A kernel stack is 16
# KiB for the whole call chain: a 15.9 KiB carryfs_fill_super frame
# overflowed it on mount and crashed the host (2026-11-03). C code has
# -Wframe-larger-than; nothing checks Rust code, so `build` scans the module
# (x86-64 `sub $N,%rsp`) and deletes it if a frame is larger.
MAX_FRAME ?= 4096

.PHONY: help build test clean

help: ## Show available commands
	@awk 'BEGIN {FS = ":.*## "} /^[a-zA-Z0-9_.-]+:.*## / {printf "  %-20s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

build: ## Build carryfs.ko for the running kernel
	$(MAKE) -C $(KDIR) M=$(CURDIR) RUSTC=$(RUSTC) modules
	@objdump -d --no-show-raw-insn carryfs.ko | awk -v max=$(MAX_FRAME) ' \
		/^[0-9a-f]+ <.*>:$$/ { fn = $$2 } \
		/sub +\$$0x[0-9a-f]+,%rsp/ { \
			h = $$0; sub(/.*\$$0x/, "", h); sub(/,.*/, "", h); n = 0; \
			for (i = 1; i <= length(h); i++) n = n * 16 + index("0123456789abcdef", substr(h, i, 1)) - 1; \
			if (n > max) { printf "stack frame of %d bytes, over MAX_FRAME=%d: %s\n", n, max, fn; bad = 1 } \
		} \
		END { exit bad }' || { rm -f carryfs.ko; echo "deleted carryfs.ko: it would overflow the kernel stack"; exit 1; }

test: ## Run the end-to-end check as root; needs the module loaded and the carryfs CLI built
	python3 tests/check.py

clean: ## Remove build artifacts
	$(MAKE) -C $(KDIR) M=$(CURDIR) RUSTC=$(RUSTC) clean
