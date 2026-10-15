# Callable backup device check

This development app checks an explicit local callable backup archive on a
physical iPhone. Use a dedicated foreground test device. It creates synthetic
fixtures in its own app container and exits after each phase. Symlink metadata
uses a separate private Caches workspace whose OS-backup exclusion is checked
before authorization and after restart. It never uses the repository as plaintext
scratch storage.

Build the archive from the repository root on an Apple Silicon Mac:

```sh
python3 scripts/backup_bridge_build.py --target ios --work-dir /private/var/tmp/backup-native --cache-dir /private/var/tmp/backup-cache --jobs 8
```

Install XcodeGen, select the iPhoneOS SDK and provide a development signing team.
From this directory, build and install using the archive's completed receipt:

```sh
python3 build.py --native-work /private/var/tmp/backup-native --device DEVICE --team TEAM
```

Use `--keychain PATH` for a signing keychain. `--signing-project PATH` optionally
runs that project's `unlock-signing` Make target before building.

Run the three phases in order, with a new process for each:

```sh
xcrun devicectl device process launch --device DEVICE --console app.solstone.journal.backup.dev
xcrun devicectl device process launch --device DEVICE --console app.solstone.journal.backup.dev -- --resume
xcrun devicectl device process launch --device DEVICE --console app.solstone.journal.backup.dev -- --cancel
```

The first phase initializes an encrypted repository, adds and independently
opens a recovery key, refuses a wrong key, creates original and changed
snapshots, lists them, restores each to a fresh destination and checks integrity.
It verifies file bytes, symlink targets, mode, modification time and an extended
attribute. It also checks that an existing restore destination is refused.

The second phase reopens both keys after process restart and restores again.
The third writes a 512 MiB random fixture, requests cancellation after observing
a repository data write, requires a busy response to an early free, waits for
the worker to finish, checks that no snapshot was published and compares the
repository again after two seconds for late writes.

Collect `Documents/backup-evidence.jsonl` from the app container together with
`app-manifest.json` and the native build receipt. A phase passes only when the process exits with code 0 and its matching
`phase.finished` event says `passed: true`. Match console and durable events
by PID. To repeat the first phase, uninstall this development app
and reinstall it, which removes only its synthetic fixtures.

The archive and this check cover local repositories. They do not establish
production backup admission, hosted transport, key rotation or service cleanup.

The separate [native HTTP device check](../backup-http/README.md) checks signed
storage requests and TLS behavior using a disposable protocol canary.
