# Journal on a dedicated iOS device

This development app runs Journal storage, indexing and search in a standard bundled ExtensionFoundation helper. Keep the device plugged in and the host app in the foreground. It requires iOS 26 and a development signing identity. It does not integrate with the capture app or run a network service.

The helper owns persistent files in its private `Documents/journal-<suite>` directory. The host sends typed operations over XPC and receives results. A supplied host path conveys no file authority. The bridge retains an admitted `JournalRoot` and checks its canonical binding before each operation; bound publication uses the production Journal IO primitives. Indexing and queries use the production crates. Exported synthetic results go to the host's Documents directory for independent checking.

## Build and install

On a Mac with Xcode 26, XcodeGen, Rust's `aarch64-apple-ios` target and a paired device in Developer Mode:

```sh
cd tools/ios-runtime
make install DEVICE=<device-identifier>
```

`make build` uses the selected iPhoneOS SDK and a generic iOS destination. It generates the Xcode project, builds the native library, generates ExtensionFoundation metadata from the compiler's constant values, then signs and verifies the helper and host. These steps are part of the build. Generated projects and build products are ignored. `BUILD_ROOT` defaults to `/var/tmp/journal-ios-runtime-build`.

The development signing defaults in `project.yml` and `finalize.py` must match the local team, identity and unlocked signing keychain. They use development provisioning only. This app has its own bundle identifier, `app.solstone.journal.runtime.dev`. Installation preserves other apps and their data.

`make run DEVICE=<device-identifier>` opens the manual controls. `make check` checks native formatting and all-target Clippy on the host. This standalone development crate is separate from the core workspace and release packages.

## Physical qualification

Use a fresh suite name for each run that creates fixtures. These commands execute and verify real native operations, exported SQLite FTS rows, exact fixture content, process identities and foreground heartbeats:

```sh
python3 qualify.py --device <id> --suite acceptance1 --output /tmp/acceptance.log
python3 qualify.py --device <id> --suite controls1 --scenario controls --output /tmp/controls.log
python3 qualify.py --device <id> --suite mixed1 --scenario mixed --count 2048 --duration 1200 --output /tmp/mixed.log
python3 qualify.py --device <id> --suite hostloss1 --scenario hostloss --output /tmp/hostloss.log
python3 qualify.py --device <id> --suite hostloss1 --scenario reopen --output /tmp/reopen.log
```

Acceptance covers create-only refusal, canonical root substitution, worker death at the production pre-publication barrier, committed data after reply loss, connection cancellation, cooperative work cancellation and idle reuse. Fault operations deliberately abort this development helper. A reconnect creates a new process object; the receipts record the actual PID. Connection cancellation does not promise worker termination or roll back committed data.

The controls repeat unpaced production searches with and without a retained read connection. The mixed run rotates updates across 32 memories in a 2,048-memory journal, atomically updates a checkpoint, scans and queries once per cycle for twenty minutes. This is a bounded foreground qualification, not a claim about unattended endurance or background execution. Capture platform resource counters alongside the console when investigating file-backed dirty-memory diagnostics; file sizes, logical write counters and physical flash writes are different measurements.

The host-loss command kills the observed host PID after indexing while the worker is active. The next command opens the same helper-owned journal from a cold host and verifies its files and index. The verifier fails on missing completion, a reported operation error, wrong content or FTS rows, or observed background heartbeats. Re-run the verifier against preserved console output with `--verify-only` and the same arguments. Keep failed runs alongside successful receipts.

All data generated here is synthetic. The fault interface and test-hook dependency belong only to this development app. Do not embed this bridge in a release app.
