SHELL := /bin/bash
SWIFTC ?= xcrun swiftc
SWIFT_HOST_LIB = $(dir $(shell xcrun --find swiftc))/../lib/swift/host

.PHONY: prepare ci verify swift-parser verify-swift
prepare:
	cargo fetch --locked

verify:
	TMPDIR=$${TMPDIR:-/var/tmp} cargo test --locked --offline --features full-tests

ci:
	cargo fmt --check
	cargo clippy --locked --offline --all-targets -- -D warnings
	cargo test --locked --offline

swift-parser:
	$(SWIFTC) -I "$(SWIFT_HOST_LIB)" -L "$(SWIFT_HOST_LIB)" -Xlinker -rpath -Xlinker "$(SWIFT_HOST_LIB)" swift-discover.swift -o swift-discover

verify-swift: swift-parser
	./swift-discover --self-test
