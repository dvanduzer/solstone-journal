# source extraction tools

Rust (`syn`) and Swift (`SwiftSyntax`) discovery records string literals with UTF-8 byte spans, exact source bytes, file hashes and syntax context. Rust discovery walks macro token trees; Swift discovery includes macro arguments and keeps interpolated strings as whole expressions. The tools inspect source without expanding macros or proving presentation dataflow.

Prepare dependencies separately, then build and check offline:

```sh
make -C tools/i18n/extract prepare
make -C tools/i18n/extract ci
make -C tools/i18n/extract verify
```

`prepare` may use the network. The other commands use the committed Cargo lockfile with `--locked --offline`. `verify` runs filesystem fault controls separately from routine units. Dependencies must already be available in the Cargo cache, or supplied through Cargo's vendoring configuration.

For the synthetic Rust fixture:

```sh
cargo run --manifest-path tools/i18n/extract/Cargo.toml --locked --offline -- \
  discover "$PWD" /var/tmp/i18n-inventory.json tools/i18n/extract/fixtures/discovery.rs
```

On a Mac with Xcode, `make -C tools/i18n/extract swift-parser` builds the SwiftSyntax helper from installed toolchain modules. Pass its absolute path using `--swift-parser` when discovery roots contain Swift. A missing parser or a syntax error fails discovery.

Each adjudication file contains `version: 1`, the complete discovery result under `inventory`, and a `decisions` array. Every decision names `file`, `start`, `end`, `class`, `reason` and `message_id`. Classes are `presentation`, `protocol`, `log`, `path`, `query`, `test` and `invariant`. A presentation decision requires a dotted stable message ID; other decisions use `null`. Reasons must be nonempty. `lint ROOT ADJUDICATION` discovers those roots again and refuses any changed file or unreviewed occurrence, including new files. There are no prose heuristics or file-level allowlists.

`plan ADJUDICATION REPLACEMENTS OUTPUT` makes a mechanical edit plan. Replacements are a JSON object keyed by `file:start:end`, containing exactly the presentation occurrences. Each value is the replacement source expression. `apply ROOT PLAN --apply` checks every affected file hash and byte span before writing any source. It stages replacements, rechecks all sources, then renames them into place. A stale plan writes no source files. Multi-file publication is not a crash-atomic transaction; do not edit sources concurrently with apply.

`reconcile ADJUDICATION CATALOG_IDS_JSON` checks presentation IDs against a JSON array of catalog IDs. It reports occurrence counts per message and catalog IDs without source references. Missing presentation IDs fail. Each command reports its elapsed phase time on stderr; deterministic inventory and plan files contain no timing data.

JavaScript and Kotlin discovery, macro expansion, symbol-reference rewriting and semantic test migration remain integration work. Apply only reviewed replacements after the caller has established typed message seams.

The extractor is AGPL-3.0-only. Its Rust dependencies declare MIT/Apache-2.0 compatible licenses in Cargo metadata; the installed SwiftSyntax modules are Apache-2.0 with the Runtime Library Exception. No dependency here is linked into a product binary.
