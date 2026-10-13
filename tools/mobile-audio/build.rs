// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

use std::{env, fs, path::Path};

fn archives(root: &Path, output: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(root).expect("read PARAKEET_LIB_DIR") {
        let path = entry.expect("read native archive entry").path();
        if path.is_dir() {
            archives(&path, output);
        } else if path.extension().is_some_and(|e| e == "a") {
            output.push(path);
        }
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=PARAKEET_LIB_DIR");
    if env::var_os("CARGO_FEATURE_NATIVE").is_none() {
        return;
    }
    let target = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let Some(root) = env::var_os("PARAKEET_LIB_DIR") else {
        assert!(
            !matches!(target.as_str(), "ios" | "android"),
            "PARAKEET_LIB_DIR is required for the native mobile build"
        );
        return;
    };
    let mut inputs = Vec::new();
    archives(Path::new(&root), &mut inputs);
    inputs.sort();
    assert!(
        inputs
            .iter()
            .any(|p| p.file_name().is_some_and(|n| n == "libparakeet.a")),
        "PARAKEET_LIB_DIR must contain the maintained libparakeet.a build"
    );
    for path in inputs {
        println!("cargo:rerun-if-changed={}", path.display());
        println!(
            "cargo:rustc-link-search=native={}",
            path.parent().unwrap().display()
        );
        let name = path
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .strip_prefix("lib")
            .unwrap();
        println!("cargo:rustc-link-lib=static={name}");
    }
    if target == "ios" {
        for name in ["Metal", "MetalKit", "Foundation", "Accelerate"] {
            println!("cargo:rustc-link-lib=framework={name}");
        }
        println!("cargo:rustc-link-lib=c++");
    } else if target == "android" {
        println!("cargo:rustc-link-lib=static=c++_static");
        println!("cargo:rustc-link-lib=static=c++abi");
        for name in ["log", "dl", "m", "atomic"] {
            println!("cargo:rustc-link-lib={name}");
        }
    } else {
        println!("cargo:rustc-link-lib=stdc++");
    }
}
