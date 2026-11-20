.DEFAULT_GOAL := help

.PHONY: help check

help: ## Show available commands
	@printf '%s\n' 'check  Run host checks, including mounted tests on FreeBSD and NetBSD'

check: ## Validate the current host adapter and core
	@case "$$(uname -s)" in \
		NetBSD) sh scripts/check-netbsd.sh ;; \
		FreeBSD) sh scripts/check-freebsd.sh ;; \
		*) bash scripts/check.sh ;; \
	esac
