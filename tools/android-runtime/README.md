# Android journal development runtime

This app runs the journal's storage, indexing and search libraries in an app-owned Android bound service. The activity and worker share an app UID and use separate processes. It is a development app for a dedicated, plugged-in device kept in the foreground.

The package is `app.solstone.development.journal`. It requests no permissions, disables Android backup and uses synthetic data in its own private directory. It has a separate debug signing identity. Uninstalling it deletes its fixtures. Reserve your test device before running the scripts.

## Build and install

Install the repository's pinned Rust toolchain and `aarch64-linux-android` target, JDK 17, Android SDK platform 36, build-tools 35.0.0 and NDK r30. The [Android cross-build instructions](../../docs/ANDROID_CROSS_BUILD.md) provide the pinned NDK acquisition recipe. Set `ANDROID_HOME`, `JAVA_HOME` and `JOURNAL_NDK` to those installations. Cache the Cargo dependencies before the offline build.

From this directory:

```sh
python3 build_native.py
python3 build_apk.py --root "$PWD" --sdk "$ANDROID_HOME" --java "$JAVA_HOME"
python3 run_device.py full --install
python3 run_device.py mixed
```

`run_device.py` targets the development handset recorded in its source. Pass `--serial SERIAL` or set `ANDROID_SERIAL` for another bench. It verifies the installed APK hash, wakes the screen and dismisses an unsecured keyguard. It leaves security settings unchanged. The app keeps the screen awake while its activity is visible.

## Repeatable checks

`full` exercises 512 synthetic activity records using real `JournalRoot`, atomic writers, the scanner, owner-bound search and SQLite. It requires confirmed create-only publication, existing-destination refusal, a single complete winner among 16 competing publishers, symlink refusal and lock release. It kills and reconnects the worker during an open SQLite transaction and immediately before and after atomic publication. Cancellation is checked between complete native work units; the service's control handler runs separately from its work executor. The activity remains alive during worker failures.

`mixed` runs 360 requests over 30 minutes plus operation time, with five seconds between requests. Its 4,096 records span 16 activity files. Queries alternate with rescans and source replacements. Memory, file descriptors, journal bytes and the activity's 50 ms timer are measured. This is bounded foreground qualification on one fixture and handset; it does not establish unattended operation or a supported device floor.

Additional scenarios are `verify`, `loss`, `exit` and `finish`. `loss` kills the host during work and records the worker's lost reply. Follow each lifecycle scenario with `verify` to reopen and search committed data from cold app processes. Recovery explicitly reconnects and reopens authoritative files. A lost reply leaves the commit outcome unknown until disk state is checked; the app does not retry writes blindly. No persistent job ledger is introduced.

After the runs, force-stop only this package and copy its `files` directory using `adb exec-out run-as app.solstone.development.journal tar -cf - files`. Capture binary stdout directly to a file. Verify that snapshot using a separate host SQLite implementation:

```sh
python3 verify_evidence.py --files files.tar \
  --full evidence/full-<run>/events.txt \
  --mixed evidence/mixed-<run>/events.txt --output verification.json
```

`evidence/` contains actual command exits, app events, APK hashes and device snapshots. `build/` contains the APK, member hashes and debug key. These and native build outputs are excluded from Git.

## Storage and limits

Android ancestor witnesses use `O_PATH` with directory, no-follow and identity checks. The admitted journal directory retains readable access for synchronization. Bound exclusive file publication uses descriptor-relative `renameat2(RENAME_NOREPLACE)` after staging and file sync; unsupported kernels or filesystems return an error. There is no overwrite-capable fallback. Other Unix platforms retain hard-link publication. Directory sync and final inode/byte observations retain their existing result states.

The native harness enables existing Journal IO test hooks for precise crash boundaries. The SQLite interruption is an explicit uncommitted transaction in the real index, rather than a crash inside the scanner. Complete unpublished stages may remain after death; automatic orphan deletion is not promised. Process-kill checks do not establish power-loss durability.

The ordinary service shares the app's file authority. The isolated-UID control service cannot open those private files. Models, media, GPU work, network serving, background residency, every other Journal writer and store distribution remain outside this development app.
