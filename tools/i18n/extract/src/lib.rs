// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc
//! Source-accurate discovery and explicit adjudication; no prose heuristics.
use proc_macro2::{TokenStream, TokenTree};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};
use syn::visit::Visit;

pub type Result<T> = std::result::Result<T, String>;
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Literal {
    pub start: usize,
    pub end: usize,
    pub raw: String,
    pub kind: String,
    pub context: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceFile {
    pub sha256: String,
    pub language: String,
    pub literals: Vec<Literal>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    pub version: u32,
    /// Complete discovery roots, relative to the source root. New files are scanned too.
    pub roots: Vec<String>,
    pub files: BTreeMap<String, SourceFile>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    Presentation,
    Protocol,
    Log,
    Path,
    Query,
    Test,
    Invariant,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub file: String,
    pub start: usize,
    pub end: usize,
    pub class: Class,
    pub reason: String,
    pub message_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Adjudication {
    pub version: u32,
    pub inventory: Inventory,
    pub decisions: Vec<Decision>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    pub start: usize,
    pub end: usize,
    pub expected: String,
    pub replacement: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FilePlan {
    pub sha256: String,
    pub edits: Vec<Edit>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub version: u32,
    pub files: BTreeMap<String, FilePlan>,
}

pub fn safe_relative(path: &str) -> Result<()> {
    if path.is_empty()
        || path.contains('\\')
        || Path::new(path)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(format!("unsafe relative source path: {path:?}"));
    }
    Ok(())
}

struct Discovery<'a> {
    source: &'a str,
    literals: BTreeMap<(usize, usize), Literal>,
    context: Vec<String>,
    error: Option<String>,
}
impl Discovery<'_> {
    fn tokens(&mut self, tokens: TokenStream) {
        for token in tokens {
            match token {
                TokenTree::Group(g) => self.tokens(g.stream()),
                TokenTree::Literal(l) => self.record(l),
                _ => (),
            }
        }
    }
    fn record(&mut self, token: proc_macro2::Literal) {
        // syn handles raw strings, byte strings and C strings without guessing quotation syntax.
        let lit = match syn::parse_str::<syn::Lit>(&token.to_string()) {
            Ok(lit) => lit,
            Err(error) => {
                self.error = Some(format!(
                    "unsupported literal token at {:?}: {error}",
                    token.span().byte_range()
                ));
                return;
            }
        };
        let kind = match lit {
            syn::Lit::Str(_) => "string",
            syn::Lit::ByteStr(_) => "byte_string",
            syn::Lit::CStr(_) => "c_string",
            syn::Lit::Verbatim(_) => {
                self.error = Some(
                    "unsupported verbatim literal: update the syntax parser before adjudication"
                        .into(),
                );
                return;
            }
            _ => return,
        };
        let range = token.span().byte_range();
        let Some(raw) = self.source.get(range.clone()) else {
            self.error = Some("parser returned an invalid UTF-8 byte span".into());
            return;
        };
        if raw != token.to_string() {
            self.error = Some(format!(
                "parser span differs from source at {}",
                range.start
            ));
            return;
        }
        self.literals.insert(
            (range.start, range.end),
            Literal {
                start: range.start,
                end: range.end,
                raw: raw.into(),
                kind: kind.into(),
                context: self.context.join("::"),
            },
        );
    }
}
impl<'ast> Visit<'ast> for Discovery<'_> {
    fn visit_lit(&mut self, lit: &'ast syn::Lit) {
        let token = match lit {
            syn::Lit::Str(s) => s.token(),
            syn::Lit::ByteStr(s) => s.token(),
            syn::Lit::CStr(s) => s.token(),
            _ => return,
        };
        self.record(token);
    }
    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        self.context.push(format!(
            "macro:{}",
            node.path
                .segments
                .last()
                .map(|s| s.ident.to_string())
                .unwrap_or_default()
        ));
        self.tokens(node.tokens.clone());
        self.context.pop();
    }
    fn visit_attribute(&mut self, node: &'ast syn::Attribute) {
        self.context.push("attribute".into());
        if let syn::Meta::List(list) = &node.meta {
            self.tokens(list.tokens.clone());
        }
        syn::visit::visit_attribute(self, node);
        self.context.pop();
    }
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.context.push(node.sig.ident.to_string());
        syn::visit::visit_item_fn(self, node);
        self.context.pop();
    }
    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        self.context.push(node.sig.ident.to_string());
        syn::visit::visit_impl_item_fn(self, node);
        self.context.pop();
    }
}
pub fn discover_rust(source: &str) -> Result<SourceFile> {
    let syntax = syn::parse_file(source).map_err(|e| format!("Rust parse failed: {e}"))?;
    let mut discovery = Discovery {
        source,
        literals: BTreeMap::new(),
        context: vec![],
        error: None,
    };
    discovery.visit_file(&syntax);
    if let Some(error) = discovery.error {
        return Err(error);
    }
    Ok(SourceFile {
        sha256: digest(source.as_bytes()),
        language: "rust".into(),
        literals: discovery.literals.into_values().collect(),
    })
}

pub fn validate_inventory(inventory: &Inventory) -> Result<()> {
    if inventory.version != 1 || inventory.roots.is_empty() || inventory.files.is_empty() {
        return Err("empty or unsupported inventory".into());
    }
    let mut roots = BTreeSet::new();
    for root in &inventory.roots {
        safe_relative(root)?;
        if !roots.insert(root) {
            return Err("duplicate discovery root".into());
        }
    }
    for (path, file) in &inventory.files {
        safe_relative(path)?;
        if !inventory
            .roots
            .iter()
            .any(|root| Path::new(path).starts_with(root))
        {
            return Err(format!("file outside declared discovery roots: {path}"));
        }
        if file.sha256.len() != 64
            || !file.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || !matches!(file.language.as_str(), "rust" | "swift")
        {
            return Err(format!("invalid file record: {path}"));
        }
        let mut end = 0;
        for lit in &file.literals {
            if lit.start < end || lit.end <= lit.start || lit.raw.len() != lit.end - lit.start {
                return Err(format!("invalid or overlapping literal spans: {path}"));
            }
            end = lit.end;
        }
    }
    Ok(())
}

/// Every occurrence is explicitly classified. A changed file invalidates its old decisions.
pub fn lint(current: &Inventory, adjudication: &Adjudication) -> Result<usize> {
    validate_inventory(current)?;
    validate_inventory(&adjudication.inventory)?;
    if adjudication.version != 1 || current != &adjudication.inventory {
        return Err("discovery changed: adjudicate new/changed/deleted files and literals".into());
    }
    let population: BTreeSet<_> = current
        .files
        .iter()
        .flat_map(|(path, file)| {
            file.literals
                .iter()
                .map(move |l| (path.as_str(), l.start, l.end))
        })
        .collect();
    if population.is_empty() {
        return Err("discovery found no string literals; check scan roots".into());
    }
    let mut seen = BTreeSet::new();
    for d in &adjudication.decisions {
        let key = (d.file.as_str(), d.start, d.end);
        if !population.contains(&key) || !seen.insert(key) || d.reason.trim().is_empty() {
            return Err(format!("invalid, duplicate or stale adjudication: {key:?}"));
        }
        match (&d.class, &d.message_id) {
            (Class::Presentation, Some(id)) if valid_message_id(id) => (),
            (Class::Presentation, _) => {
                return Err("presentation literals need a dotted stable message ID".into())
            }
            (_, Some(_)) => return Err("only presentation decisions may name a message ID".into()),
            _ => (),
        }
    }
    if seen != population {
        return Err(format!(
            "unadjudicated literals: {} of {}",
            population.len() - seen.len(),
            population.len()
        ));
    }
    Ok(population.len())
}
fn valid_message_id(id: &str) -> bool {
    id.contains('.')
        && id.split('.').all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}
/// Exact catalog membership, rather than a stale population total.
#[derive(Debug, Serialize)]
pub struct Reconciliation {
    pub source_occurrences: usize,
    pub messages: BTreeMap<String, usize>,
    pub unreferenced_catalog_ids: Vec<String>,
}

pub fn reconcile(adjudication: &Adjudication, ids: &BTreeSet<String>) -> Result<Reconciliation> {
    let source_occurrences = lint(&adjudication.inventory, adjudication)?;
    let mut occurrences = BTreeMap::new();
    for d in &adjudication.decisions {
        if let Some(id) = &d.message_id {
            if !ids.contains(id) {
                return Err(format!("presentation message missing from catalog: {id}"));
            }
            *occurrences.entry(id.clone()).or_insert(0) += 1;
        }
    }
    let unreferenced_catalog_ids = ids
        .iter()
        .filter(|id| !occurrences.contains_key(*id))
        .cloned()
        .collect();
    Ok(Reconciliation {
        source_occurrences,
        messages: occurrences,
        unreferenced_catalog_ids,
    })
}

/// Preflight the entire plan in memory before any file is written.
pub fn prepare_apply(
    plan: &Plan,
    sources: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    if plan.version != 1 || plan.files.is_empty() || sources.len() != plan.files.len() {
        return Err("empty or unsupported plan/source population".into());
    }
    let mut output = BTreeMap::new();
    for (path, file) in &plan.files {
        safe_relative(path)?;
        let source = sources
            .get(path)
            .ok_or_else(|| format!("missing source: {path}"))?;
        if digest(source.as_bytes()) != file.sha256 {
            return Err(format!("stale plan: {path}"));
        }
        if file.edits.is_empty() {
            return Err(format!("no edits for {path}"));
        }
        let mut edits = file.edits.clone();
        edits.sort_by_key(|e| e.start);
        let mut end = 0;
        for e in &edits {
            if e.start < end
                || e.end <= e.start
                || source.get(e.start..e.end) != Some(e.expected.as_str())
            {
                return Err(format!(
                    "overlapping/invalid/stale byte span: {path}:{}",
                    e.start
                ));
            }
            end = e.end;
        }
        let mut result = source.clone();
        for e in edits.into_iter().rev() {
            result.replace_range(e.start..e.end, &e.replacement);
        }
        output.insert(path.clone(), result);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn inventory(source: &str) -> Inventory {
        Inventory {
            version: 1,
            roots: vec!["src".into()],
            files: BTreeMap::from([("src/main.rs".into(), discover_rust(source).unwrap())]),
        }
    }
    fn classify(i: Inventory) -> Adjudication {
        let decisions = i
            .files
            .iter()
            .flat_map(|(p, f)| {
                f.literals.iter().map(move |l| Decision {
                    file: p.clone(),
                    start: l.start,
                    end: l.end,
                    class: Class::Invariant,
                    reason: "synthetic control".into(),
                    message_id: None,
                })
            })
            .collect();
        Adjudication {
            version: 1,
            inventory: i,
            decisions,
        }
    }
    #[test]
    fn utf8_macros_bytes_attributes_and_nested_display() {
        let source = "#[allow(reason = \"例\")] fn label() -> &'static str { \"ready\" }\nimpl std::fmt::Display for E { fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { write!(f, \"nested {}\", format_args!(\"detail\")) } }\nfn main() { println!(\"{}\", label()); let _ = br#\"bytes\"#; let _ = r#\"中文\"#; }";
        let f = discover_rust(source).unwrap();
        assert_eq!(f.literals.len(), 7);
        for l in &f.literals {
            assert_eq!(source.get(l.start..l.end), Some(l.raw.as_str()));
        }
        assert!(f
            .literals
            .iter()
            .any(|l| l.raw == "\"detail\"" && l.context.contains("macro:write")));
    }
    #[test]
    fn every_new_literal_is_adjudicated_even_helper_display_and_new_file() {
        let i = inventory(
            "fn main() { println!(\"{}\", label()); } fn label()-> &'static str { \"ready\" }",
        );
        let a = classify(i.clone());
        assert_eq!(lint(&i, &a).unwrap(), 2);
        let mut missing = a.clone();
        missing.decisions.pop();
        assert!(lint(&i, &missing).is_err());
        let mut new_file = i.clone();
        new_file.files.insert(
            "src/new.rs".into(),
            discover_rust("impl Display for E { fn fmt() { write!(f, \"nested detail\") } }")
                .unwrap(),
        );
        assert!(lint(&new_file, &a).is_err());
        let mut changed = i.clone();
        changed.files.get_mut("src/main.rs").unwrap().sha256 = digest(b"changed");
        assert!(lint(&changed, &a).is_err());
        let mut duplicate = a.clone();
        duplicate.decisions.push(a.decisions[0].clone());
        assert!(lint(&i, &duplicate).is_err());
    }
    #[test]
    fn stale_plan_rejects_every_file_before_output() {
        let sources: BTreeMap<String, String> = BTreeMap::from([
            ("src/a.rs".into(), "\"中文\"".into()),
            ("src/z.rs".into(), "\"old\"".into()),
        ]);
        let mut files = BTreeMap::new();
        for (p, s) in &sources {
            files.insert(
                p.clone(),
                FilePlan {
                    sha256: digest(s.as_bytes()),
                    edits: vec![Edit {
                        start: 0,
                        end: s.len(),
                        expected: s.clone(),
                        replacement: "message()".into(),
                    }],
                },
            );
        }
        let mut plan = Plan { version: 1, files };
        assert_eq!(prepare_apply(&plan, &sources).unwrap().len(), 2);
        plan.files.get_mut("src/z.rs").unwrap().sha256 = digest(b"stale");
        assert!(prepare_apply(&plan, &sources).is_err());
        plan.files.get_mut("src/z.rs").unwrap().sha256 = digest(b"\"old\"");
        plan.files.get_mut("src/a.rs").unwrap().edits[0].end = 3;
        assert!(prepare_apply(&plan, &sources).is_err());
    }
    #[test]
    fn reconciliation_reports_missing_and_unreferenced_catalog_identity() {
        let i = inventory("fn label()-> &'static str { \"ready\" }");
        let mut a = classify(i);
        a.decisions[0].class = Class::Presentation;
        a.decisions[0].message_id = Some("fixture.state.ready".into());
        assert!(reconcile(&a, &BTreeSet::new()).is_err());
        let ids = BTreeSet::from(["fixture.state.ready".into(), "fixture.unreferenced".into()]);
        let result = reconcile(&a, &ids).unwrap();
        assert_eq!(result.source_occurrences, 1);
        assert_eq!(result.messages["fixture.state.ready"], 1);
        assert_eq!(
            result.unreferenced_catalog_ids,
            vec!["fixture.unreferenced"]
        );
    }

    #[test]
    fn unsafe_paths_and_parser_errors_fail() {
        for p in ["../x", "/x", "src/../x", "src\\x", ""] {
            assert!(safe_relative(p).is_err());
        }
        assert!(discover_rust("fn broken(").is_err());
        let mut i = inventory("fn main() {}");
        assert!(lint(&i, &classify(i.clone())).is_err());
        i.roots.clear();
        assert!(validate_inventory(&i).is_err());
    }
}
