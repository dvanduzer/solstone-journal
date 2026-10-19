# catalog and source tools

These standalone build tools prepare localization resources and review source literals. They are outside the application Cargo workspace and install payload. The fixtures are synthetic; the tools do not declare application language support. [Dependency notices](licenses/NOTICES.md).

## catalog compiler

Prepare the compiler's pinned Cargo dependencies during an explicit setup step. Build and run it offline afterward:

```sh
cargo fetch --manifest-path tools/i18n/Cargo.toml --locked
cargo build --manifest-path tools/i18n/Cargo.toml --locked --offline
cargo test --manifest-path tools/i18n/Cargo.toml --locked --offline
cargo clippy --manifest-path tools/i18n/Cargo.toml --locked --offline --all-targets -- -D warnings
cargo fmt --manifest-path tools/i18n/Cargo.toml --check
tools/i18n/target/debug/i18n-catalog compile tools/i18n/fixtures/synth --channel production
tools/i18n/target/debug/i18n-catalog coverage tools/i18n/fixtures/synth --channel production
```

Each catalog contains `catalog.json`, `source/<source_locale>.ftl`, `meta/<dotted-id>.json`, `translations/<locale>.ftl`, `history.json` and `source-lock.json`. The [synthetic catalog](fixtures/synth) is an executable example. [Sidecar schema](schema/metadata.v1.json), [IR schema](schema/ir.v1.json) and compiler admission checks define the format together.

Author message identities as stable dotted IDs. These are compiler input: stock Fluent readers cannot read them directly. Runtime resource keys are the persisted `m` prefix followed by the lowercase UTF-8 hex encoding of the identity. Keep history in version control; retired identities remain tombstones. Argument contracts, namespace owners and protected-token values cannot change under an existing identity. Create a new identity for those changes.

The portable profile admits plain text, named typed arguments, one top-level cardinal select, or an enum select with an explicit default. Functions, terms, message references, nested selects and rich markup refuse admission. Scalar contracts distinguish strings, booleans, integers and cardinal quantities. Integers stay within JavaScript's exact-integer range; cardinal quantities also stay within signed 32-bit range. Numeric style is explicit. `max_length` is a layout hint.

Put commands, flags, accepted input, URLs, mark words and meaningful whitespace in `protected_slots`. Refer to them with the same named-variable syntax as arguments, but keep their names disjoint from runtime argument names. The compiler injects their exact sidecar bytes; callers cannot supply them. Translations preserve each branch's slot multiplicity and may reorder slots. Pseudolocalization changes prose while preserving these slots and runtime argument identities.

Every sidecar names its logical `surfaces` and compiled `bundles` separately. Coverage checks required message × locale × bundle cells, plus branch and slot populations. The source locale uses canonical source directly; another locale requires an explicit current catalog. English fallback cannot satisfy its coverage.

The [locale registry](locales.json) is compiled into this tool. It owns namespaces, bundle locale membership, locale purpose, tier and channel state. Its current entries are synthetic. Production excludes draft, test and pseudo locales even when a contradictory release flag says released. A consuming repository pins the reviewed compiler source and registry bytes together; adding its real namespaces, bundles or supported languages is an explicit integration change.

For a new catalog, bootstrap history explicitly with `history: false` in `catalog.json`, run `history <catalog>`, then set `history: true`. Compiled output requires history; a catalog claiming existing history cannot recreate a missing registry. Use `retire <catalog> --id <id> [--replacement <id>]` to keep a permanent retirement record.

After changing source prose, run `lock <catalog>`. Copy each changed message's compiler-produced `source` hash from `source-lock.json` into its sidecar `source_revision`; do not hash FTL text yourself. Review and update each translation, then run `lock <catalog> --acknowledge <locale>`. A source-only lock refresh preserves old locale acknowledgments, so unchanged translations stay stale. Withdraw obsolete content approvals; a supplied approval binds its exact locale, source revision and translation revision. Payment, legal, statutory and consent entries reject `auto_merge: true`.

`emit <catalog> --channel <channel> --out <file>` writes deterministic validated IR. `pseudo <catalog> --out <absent-directory>` creates en-XA/ar-XB authoring catalogs and reports their coverage denominator. These catalogs go through the compiler before any runtime reader.

`export <catalog> --out <absent-directory>` creates a shared catalog bundle with a deterministic `import-lock.json`. `import <bundle> --dest <absent-directory>` verifies the exact manifest and member digests before publishing a staged tree and retaining the lock. Commit the imported bytes and lock with the consuming repository's source pin. Import verifies transport integrity; run normal catalog compilation afterward to validate semantics and current coverage. Local build and CI routes need no network or localization service. The tools use Rust and Swift, so a consuming repository need not introduce Python.

## source review

The [extractor](extract/README.md) uses Rust `syn` and an installed SwiftSyntax parser. Prepare dependencies explicitly, then run its locked, offline checks:

```sh
make -C tools/i18n/extract prepare
make -C tools/i18n/extract ci
make -C tools/i18n/extract verify
cargo build --manifest-path tools/i18n/extract/Cargo.toml --locked --offline
```

Use the `.` discovery root to cover new top-level source directories. A narrower directory root covers new files only within that directory. On a Mac, build the Swift helper and pass its absolute path when the tree contains Swift:

```sh
make -C tools/i18n/extract swift-parser
make -C tools/i18n/extract verify-swift
tools/i18n/extract/target/debug/i18n-extract discover "$PWD" /var/tmp/inventory.json . \
  --swift-parser "$PWD/tools/i18n/extract/swift-discover"
```

Every discovered occurrence needs an explicit classification and reason. Presentation occurrences also name a stable dotted message ID. Keep the complete inventory with those decisions and run `lint` against the current source. It refuses new files, changed files and unreviewed literals, including strings returned by helpers or nested in formatting macros. Reviewers still decide which expressions are presentation and how typed message calls should replace them.

Use `reconcile` to compare presentation IDs with the catalog's ID set. Generate a plan from reviewed replacement expressions, then apply it explicitly. A stale file or byte span refuses the entire plan before source writes. Publication is not a crash-atomic transaction across files; use an exclusive source checkout during apply. Command timings are written separately from deterministic artifacts.

## locale tests

The [locale harnesses](locale/README.md) reject a run unless its requested locale canary succeeds. On glibc, the runner builds private locales and starts the command with the verified environment. Rust standard formatting ignores that environment; check the localization adapter's rendered output separately.

Apple tests must call the canary inside the actual test process and declare test localizations in the test target. The disposable Foundation check covers the harness; it does not qualify simulator or SwiftUI rendering. Test declarations and fixtures must stay out of application packaging.
