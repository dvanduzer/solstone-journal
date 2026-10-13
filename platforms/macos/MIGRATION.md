# Where this app came from

Until October 2026 the journal app for mac was built from [solstone-macos](https://github.com/solpbc/solstone-macos), next to the solstone app. Its Swift source, runtime adapter, tests, icon, AX contract and packaging moved here so the app and the runtime it carries are developed and released from one repository.

- **Imported from:** solpbc/solstone-macos at commit `502376c33dc734c01136c8edd6b20c6191c0077a`, byte for byte. [MIGRATION-manifest.tsv](MIGRATION-manifest.tsv) lists every file's old path, new path, mode, Git blob and SHA-256.
- **History before the move** stays in solstone-macos. `git log` there on the old path (for example `Sources/journal/`) shows it.
- **Releases up to 2.0.44 (build 83)** were built from solstone-macos. Their `journal-v<version>-build-<build>` tags, GitHub releases and DMGs stay in that repository. Later app releases are tagged here.
- **Nothing installed changed.** Bundle ID, update feed and signing key, watchdog helper, defaults, the chosen journal folder and the handoff file are the same. Updating from a release built in solstone-macos is an ordinary update.

## Path map

| solstone-macos | here (`platforms/macos/`) |
|---|---|
| `Sources/journal/` | `Sources/journal/` |
| `Sources/JournalRuntime/` | `Sources/JournalRuntime/` |
| `Sources/journal-icon-gen/` | `Sources/journal-icon-gen/` |
| `Sources/SolstoneCore/SunArcEngine.swift`, `SunArcZonePoints.swift` | `Sources/journal/` (only the app draws the sun arc) |
| `Sources/SolstoneCore/SolVersionParser.swift` | `Sources/JournalRuntime/` |
| `Tests/journalTests/`, `Tests/JournalRuntimeTests/` | the same paths |
| `Tests/JournalRuntimeTestSupport/` (runtime helpers) | `Tests/JournalRuntimeTestSupport/` |
| `Tests/JournalMarkKitTests/JournalIconGeneratedAssetTests.swift` | `Tests/journalTests/` |
| `assets/icon-journal.svg`, `journal-ax-contract.json`, `CHANGELOG-journal.md` | the same names |
| `scripts/journal_native_provenance.py` and its two tests | `scripts/` |
| the Journal targets of `Makefile` | [Makefile](Makefile) |

## Split, not moved

These stayed shared in solstone-macos, and the app consumes them at the pinned commit:

- `SolstoneCore`, `JournalMarkKit` (the mark, its font and the `/init` and identity clients both apps use) and `UpdateKit`.
- The watchdog, now the `WatchdogKit` library behind one `run()` entry. Each app has a one-line `solstone-watchdog` executable; this app's target is `JournalWatchdogMain`.
- Generic test helpers, now the `MacTestSupport` library: URL-protocol and log recorders, the pixel sampler, and the appearance and log-subsystem source scans.
- The release helpers for signing, appcasts, GitHub releases, changelogs and the CI lock.

Tests that mixed both apps were split along the same line: the app's own assertions (its committed icon, watchdog LaunchAgent, quit path, appearance and log-subsystem scans) live here, the shared ones stay there. The solstone app's native integration test now starts this package's `journal-integration-fixture` instead of importing the runtime adapter. The obsolete setup-event check against the retired Python setup was replaced by a fixture the native setup crate keeps current.
