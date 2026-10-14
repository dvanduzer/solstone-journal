# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) 2026 sol pbc
SHELL := /bin/bash
BUILD_ROOT ?= /var/tmp/journal-ios-runtime-build
DEVICE ?=
DESTINATION ?= generic/platform=iOS
RUST_LIB := $(BUILD_ROOT)/rust/aarch64-apple-ios/debug/libsolstone_ios_runtime.a
APPRAISAL_LIB := $(BUILD_ROOT)/appraisal-rust/aarch64-apple-ios/debug/libsolstone_ios_appraisal_helper.a
APP := $(BUILD_ROOT)/derived/Build/Products/Debug-iphoneos/JournalRuntimeDev.app
.PHONY: build rust appraisal-rust payload project finalize install run check
rust:
	cargo build --locked --manifest-path native/Cargo.toml $(if $(AUDIO_MODELS_DIR),--features audio,) --target aarch64-apple-ios --target-dir $(BUILD_ROOT)/rust
appraisal-rust:
	cargo build --locked --manifest-path appraisal-helper/Cargo.toml --target aarch64-apple-ios --target-dir $(BUILD_ROOT)/appraisal-rust
payload:
	AUDIO_MODELS_DIR='$(AUDIO_MODELS_DIR)' python3 package_payload.py
project: payload
	xcodegen generate
build: rust appraisal-rust project
	xcodebuild -project JournalRuntimeDev.xcodeproj -scheme JournalRuntimeDev -configuration Debug -destination '$(DESTINATION)' -derivedDataPath $(BUILD_ROOT)/derived -allowProvisioningUpdates RUST_LIB=$(RUST_LIB) APPRAISAL_LIB=$(APPRAISAL_LIB) build
	python3 finalize.py $(BUILD_ROOT)
install: build
	@test -n "$(DEVICE)" || (echo "Set DEVICE to a connected device identifier"; exit 2)
	xcrun devicectl device install app --device $(DEVICE) $(APP)
run:
	@test -n "$(DEVICE)" || (echo "Set DEVICE to a connected device identifier"; exit 2)
	xcrun devicectl device process launch --device $(DEVICE) --console --terminate-existing app.solstone.journal.runtime.dev $(ARGS)
check:
	cargo fmt --manifest-path native/Cargo.toml -- --check
	cargo clippy --locked --manifest-path native/Cargo.toml --all-targets --no-deps -- -D warnings
	cargo fmt --manifest-path appraisal-helper/Cargo.toml -- --check
	cargo clippy --locked --manifest-path appraisal-helper/Cargo.toml --all-targets --no-deps -- -D warnings
