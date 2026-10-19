// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
fn label() -> &'static str { "ready" }
fn main() { println!("{}", label()); let _ = r#"中文İ"#; let _ = b"bytes"; }
struct Detail;
impl std::fmt::Display for Detail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "nested {}", format_args!("detail"))
    }
}
