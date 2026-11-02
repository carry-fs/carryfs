# Out-of-tree build of the CarryFS kernel module against the running kernel.
.DEFAULT_GOAL := help

KDIR  ?= /lib/modules/$(shell uname -r)/build
# Rust modules must use the compiler that built the kernel's Rust crates;
# Ubuntu ships it as rustc-1.93 for its 7.0 kernels.
RUSTC ?= rustc-1.93

.PHONY: help build test clean

help: ## Show available commands
	@awk 'BEGIN {FS = ":.*## "} /^[a-zA-Z0-9_.-]+:.*## / {printf "  %-20s %s\n", $$1, $$2}' $(MAKEFILE_LIST)

build: ## Build carryfs.ko for the running kernel
	$(MAKE) -C $(KDIR) M=$(CURDIR) RUSTC=$(RUSTC) modules

test: ## Run the end-to-end check as root; needs the module loaded and the carryfs CLI built
	python3 tests/check.py

clean: ## Remove build artifacts
	$(MAKE) -C $(KDIR) M=$(CURDIR) RUSTC=$(RUSTC) clean
