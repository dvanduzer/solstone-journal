// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
use i18n_extract::*;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

fn read<T: DeserializeOwned>(p: &str) -> Result<T> {
    serde_json::from_slice(&fs::read(p).map_err(|e| format!("{p}: {e}"))?)
        .map_err(|e| format!("{p}: {e}"))
}
fn write<T: Serialize>(p: &str, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    fs::write(p, bytes).map_err(|e| format!("{p}: {e}"))
}
fn source_path(root: &Path, relative: &str) -> Result<PathBuf> {
    safe_relative(relative)?;
    let mut target = root.to_path_buf();
    for part in Path::new(relative).components() {
        target.push(part);
        if fs::symlink_metadata(&target)
            .map_err(|e| format!("{}: {e}", target.display()))?
            .file_type()
            .is_symlink()
        {
            return Err(format!("symlink source refused: {}", target.display()));
        }
    }
    Ok(target)
}
fn walk(root: &Path, path: &str, files: &mut BTreeSet<String>) -> Result<()> {
    let full = if path == "." {
        root.to_path_buf()
    } else {
        source_path(root, path)?
    };
    if full.is_file() {
        if matches!(
            full.extension().and_then(|v| v.to_str()),
            Some("rs" | "swift")
        ) {
            files.insert(path.into());
        }
    } else if full.is_dir() {
        for entry in fs::read_dir(&full).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "non-UTF8 source filename")?;
            if matches!(name.as_str(), ".git" | "target" | ".build" | "node_modules") {
                continue;
            }
            let relative = if path == "." {
                name
            } else {
                format!("{path}/{name}")
            };
            walk(root, &relative, files)?;
        }
    } else {
        return Err(format!("not a regular source or directory: {path}"));
    }
    Ok(())
}
fn discover(root: &Path, roots: &[String], swift_parser: Option<&str>) -> Result<Inventory> {
    let mut paths = BTreeSet::new();
    for path in roots {
        walk(root, path, &mut paths)?;
    }
    let mut files = BTreeMap::new();
    for path in paths {
        let full = source_path(root, &path)?;
        let source = fs::read_to_string(&full).map_err(|e| format!("{path}: {e}"))?;
        let file = if path.ends_with(".rs") {
            discover_rust(&source).map_err(|e| format!("{path}: {e}"))?
        } else {
            let parser = swift_parser.ok_or("Swift source found: pass --swift-parser pointing to the SwiftSyntax discovery executable")?;
            let output = Command::new(parser)
                .arg(&full)
                .output()
                .map_err(|e| format!("SwiftSyntax parser: {e}"))?;
            if !output.status.success() {
                return Err(format!(
                    "SwiftSyntax parser failed for {path}: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            let literals: Vec<Literal> = serde_json::from_slice(&output.stdout)
                .map_err(|e| format!("SwiftSyntax parser output for {path}: {e}"))?;
            for literal in &literals {
                if source.get(literal.start..literal.end) != Some(&literal.raw) {
                    return Err(format!("Swift parser byte span mismatch: {path}"));
                }
            }
            SourceFile {
                sha256: digest(source.as_bytes()),
                language: "swift".into(),
                literals,
            }
        };
        files.insert(path, file);
    }
    let result = Inventory {
        version: 1,
        roots: roots
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        files,
    };
    validate_inventory(&result)?;
    Ok(result)
}
fn build_plan(a: &Adjudication, replacements: &BTreeMap<String, String>) -> Result<Plan> {
    lint(&a.inventory, a)?;
    let mut files = BTreeMap::new();
    let mut used = BTreeSet::new();
    for d in &a.decisions {
        if d.class != Class::Presentation {
            continue;
        }
        let key = format!("{}:{}:{}", d.file, d.start, d.end);
        let replacement = replacements
            .get(&key)
            .ok_or_else(|| format!("missing replacement: {key}"))?;
        let source = &a.inventory.files[&d.file];
        let literal = source
            .literals
            .iter()
            .find(|l| l.start == d.start && l.end == d.end)
            .ok_or("decision has no literal")?;
        let file = files.entry(d.file.clone()).or_insert_with(|| FilePlan {
            sha256: source.sha256.clone(),
            edits: vec![],
        });
        file.edits.push(Edit {
            start: d.start,
            end: d.end,
            expected: literal.raw.clone(),
            replacement: replacement.clone(),
        });
        used.insert(key);
    }
    if used.len() != replacements.len() || files.is_empty() {
        return Err(
            "replacement set must exactly match the nonempty presentation population".into(),
        );
    }
    Ok(Plan { version: 1, files })
}
struct ApplyLock(PathBuf);
impl Drop for ApplyLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn apply(root: &Path, plan: &Plan) -> Result<usize> {
    let lock_path = root.join(".i18n-apply.lock");
    let mut lock = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("cannot acquire {}: {e}", lock_path.display()))?;
    let _guard = ApplyLock(lock_path);
    writeln!(lock, "{}", std::process::id()).map_err(|e| e.to_string())?;
    let mut sources = BTreeMap::new();
    for path in plan.files.keys() {
        sources.insert(
            path.clone(),
            fs::read_to_string(source_path(root, path)?).map_err(|e| e.to_string())?,
        );
    }
    let output = prepare_apply(plan, &sources)?;
    // Staging does not touch a source. A second full preflight precedes the first rename.
    let mut staged = Vec::new();
    let stage_result = (|| -> Result<()> {
        for (path, text) in &output {
            let full = source_path(root, path)?;
            let stage = full.with_file_name(format!(
                ".i18n-stage-{}-{}",
                std::process::id(),
                staged.len()
            ));
            let mut f = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&stage)
                .map_err(|e| e.to_string())?;
            staged.push((full.clone(), stage.clone()));
            f.set_permissions(
                fs::metadata(&full)
                    .map_err(|e| e.to_string())?
                    .permissions(),
            )
            .map_err(|e| e.to_string())?;
            f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
            f.sync_all().map_err(|e| e.to_string())?;
        }
        for (path, source) in &sources {
            if fs::read_to_string(source_path(root, path)?).map_err(|e| e.to_string())? != *source {
                return Err(format!("source changed during staging: {path}"));
            }
        }
        Ok(())
    })();
    if let Err(error) = stage_result {
        for (_, stage) in &staged {
            let _ = fs::remove_file(stage);
        }
        return Err(error);
    }
    let mut published = Vec::new();
    for (full, stage) in &staged {
        if let Err(error) = fs::rename(stage, full) {
            let mut rollback_errors = Vec::new();
            for prior in &published {
                if let Err(e) = fs::write(root.join(prior), &sources[prior]) {
                    rollback_errors.push(e.to_string());
                }
            }
            for (_, pending) in &staged {
                let _ = fs::remove_file(pending);
            }
            return Err(format!(
                "publication failed: {error}; rollback errors: {rollback_errors:?}"
            ));
        }
        published.push(
            full.strip_prefix(root)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .to_string(),
        );
    }
    Ok(output.len())
}
fn run(args: &[String]) -> Result<serde_json::Value> {
    match args.first().map(String::as_str) {
        Some("discover") if args.len()>=4 => {
            let root=fs::canonicalize(&args[1]).map_err(|e|e.to_string())?;
            let position=args.iter().position(|a| a=="--swift-parser");
            let end=position.unwrap_or(args.len());
            if end<4 || position.is_some_and(|p|p+2!=args.len()) { return Err("discover ROOT OUTPUT SCAN_ROOT... [--swift-parser EXECUTABLE]".into()); }
            let roots=args[3..end].to_vec();
            let i=discover(&root,&roots,position.map(|p|args[p+1].as_str()))?; write(&args[2],&i)?;
            Ok(serde_json::json!({"files":i.files.len(),"literal_occurrences":i.files.values().map(|f|f.literals.len()).sum::<usize>()}))
        }
        Some("lint") if args.len()==3 || args.len()==5 => {
            let a:Adjudication=read(&args[2])?;
            let root=fs::canonicalize(&args[1]).map_err(|e|e.to_string())?;
            let swift=if args.len()==5 && args[3]=="--swift-parser" {Some(args[4].as_str())} else if args.len()==3 {None} else {return Err("invalid lint options".into())};
            let current=discover(&root,&a.inventory.roots,swift)?;
            Ok(serde_json::json!({"adjudicated_occurrences":lint(&current,&a)?}))
        }
        Some("plan") if args.len()==4 => { let plan=build_plan(&read(&args[1])?,&read(&args[2])?)?;write(&args[3],&plan)?;Ok(serde_json::json!({"planned_files":plan.files.len()})) }
        Some("apply") if args.len()==4 && args[3]=="--apply" => { let root=fs::canonicalize(&args[1]).map_err(|e|e.to_string())?;Ok(serde_json::json!({"applied_files":apply(&root,&read(&args[2])?)?})) }
        Some("reconcile") if args.len()==3 => { Ok(serde_json::to_value(reconcile(&read(&args[1])?,&read(&args[2])?)?).map_err(|e|e.to_string())?) }
        _ => Err("usage: i18n-extract discover ROOT OUTPUT SCAN_ROOT... [--swift-parser EXECUTABLE] | lint ROOT ADJUDICATION [--swift-parser EXECUTABLE] | plan ADJUDICATION REPLACEMENTS OUTPUT | apply ROOT PLAN --apply | reconcile ADJUDICATION CATALOG_IDS_JSON\nAll literals require revision-bound per-occurrence decisions; no file or prose heuristic exemptions. Use scan root '.' to include new top-level source directories. Dependency acquisition is separate; run Cargo with --locked --offline. Phase elapsed nanoseconds are reported on stderr. Multi-file apply preflights all files before writing; it does not promise crash-atomic transactions across multiple files. Do not edit sources concurrently with apply.".into()),
    }
}
fn main() -> ExitCode {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let started = Instant::now();
    if args.first().is_some_and(|a| a == "--help") {
        if let Err(usage) = run(&[]) {
            println!("{usage}");
        }
        return ExitCode::SUCCESS;
    }
    let result = run(&args);
    eprintln!(
        "{}",
        serde_json::json!({"phase":args.first(),"elapsed_ns":started.elapsed().as_nanos(),"success":result.is_ok()})
    );
    match result {
        Ok(value) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(all(test, feature = "full-tests"))]
mod filesystem_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "i18n-extract-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            fs::create_dir(root.join("src")).unwrap();
            Self(root)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
    #[test]
    fn stale_last_file_writes_nothing_and_valid_plan_preserves_utf8() {
        let scratch = Scratch::new();
        let root = &scratch.0;
        let mut files = BTreeMap::new();
        for (path, source) in [
            ("src/a.rs", "fn main(){let x=\"中文İ\";}"),
            ("src/z.rs", "fn main(){let y=\"old\";}"),
        ] {
            fs::write(root.join(path), source).unwrap();
            let literal = &discover_rust(source).unwrap().literals[0];
            files.insert(
                path.into(),
                FilePlan {
                    sha256: digest(source.as_bytes()),
                    edits: vec![Edit {
                        start: literal.start,
                        end: literal.end,
                        expected: literal.raw.clone(),
                        replacement: "message()".into(),
                    }],
                },
            );
        }
        let plan = Plan { version: 1, files };
        let a = fs::read(root.join("src/a.rs")).unwrap();
        fs::write(root.join("src/z.rs"), "fn main(){let y=\"changed\";}").unwrap();
        let z = fs::read(root.join("src/z.rs")).unwrap();
        assert!(apply(root, &plan).unwrap_err().contains("stale"));
        assert_eq!(fs::read(root.join("src/a.rs")).unwrap(), a);
        assert_eq!(fs::read(root.join("src/z.rs")).unwrap(), z);
        fs::write(root.join("src/z.rs"), "fn main(){let y=\"old\";}").unwrap();
        assert_eq!(apply(root, &plan).unwrap(), 2);
        assert!(fs::read_to_string(root.join("src/a.rs"))
            .unwrap()
            .contains("message()"));
        assert!(!root.join(".i18n-apply.lock").exists());
    }
    #[test]
    fn discover_new_files_and_refuse_empty_or_invalid_roots() {
        let scratch = Scratch::new();
        let roots = vec!["src".into()];
        assert!(discover(&scratch.0, &roots, None).is_err());
        fs::write(
            scratch.0.join("src/a.rs"),
            "fn label()->&'static str {\"ready\"}",
        )
        .unwrap();
        let before = discover(&scratch.0, &roots, None).unwrap();
        assert_eq!(before.files.len(), 1);
        fs::write(
            scratch.0.join("src/new.rs"),
            "fn display(){println!(\"detail\");}",
        )
        .unwrap();
        let after = discover(&scratch.0, &roots, None).unwrap();
        assert_eq!(after.files.len(), 2);
        assert_ne!(before, after);
    }
    #[test]
    fn discovery_root_order_does_not_change_inventory() {
        let scratch = Scratch::new();
        fs::create_dir(scratch.0.join("tests")).unwrap();
        fs::write(scratch.0.join("src/a.rs"), "fn a(){let _=c\"token\";}").unwrap();
        fs::write(scratch.0.join("tests/a.rs"), "fn a(){let _=\"fixture\";}").unwrap();
        let a = discover(&scratch.0, &["src".into(), "tests".into()], None).unwrap();
        let b = discover(&scratch.0, &["tests".into(), "src".into()], None).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.files["src/a.rs"].literals[0].kind, "c_string");
    }

    #[test]
    fn whole_repository_scan_catches_new_top_level_sources() {
        let scratch = Scratch::new();
        fs::write(scratch.0.join("src/a.rs"), "fn a(){let _=\"ready\";}").unwrap();
        let before = discover(&scratch.0, &[".".into()], None).unwrap();
        assert!(before.files.contains_key("src/a.rs"));
        fs::create_dir(scratch.0.join("new_surface")).unwrap();
        fs::write(
            scratch.0.join("new_surface/label.rs"),
            "fn label(){println!(\"new label\");}",
        )
        .unwrap();
        let after = discover(&scratch.0, &[".".into()], None).unwrap();
        assert!(after.files.contains_key("new_surface/label.rs"));
        let literal = &before.files["src/a.rs"].literals[0];
        let a = Adjudication {
            version: 1,
            inventory: before.clone(),
            decisions: vec![Decision {
                file: "src/a.rs".into(),
                start: literal.start,
                end: literal.end,
                class: Class::Presentation,
                reason: "synthetic helper label".into(),
                message_id: Some("fixture.state.ready".into()),
            }],
        };
        assert_eq!(lint(&before, &a).unwrap(), 1);
        assert!(lint(&after, &a).unwrap_err().contains("discovery changed"));
        assert!(safe_relative(".").is_err()); // Editable file paths still cannot address the root.
    }

    #[cfg(unix)]
    #[test]
    fn symlink_sources_refuse_without_modifying_target() {
        let scratch = Scratch::new();
        fs::write(scratch.0.join("outside.rs"), "fn x(){let y=\"a\";}").unwrap();
        std::os::unix::fs::symlink(scratch.0.join("outside.rs"), scratch.0.join("src/a.rs"))
            .unwrap();
        assert!(discover(&scratch.0, &["src".into()], None)
            .unwrap_err()
            .contains("symlink"));
    }
}
