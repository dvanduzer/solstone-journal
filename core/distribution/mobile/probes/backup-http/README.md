# Native HTTP device check

This separate development app checks the callable archive's Apple HTTP
transport with a random 64 KiB storage canary. It signs a PUT, reads back the
same bytes, checks HEAD metadata, refuses invalid credentials and checks that
a self-signed TLS endpoint is refused. It exits after writing a receipt.

Build the production archive from the repository root on an Apple Silicon Mac:

```sh
python3 scripts/backup_bridge_build.py --target ios --work-dir /private/var/tmp/backup-native --cache-dir /private/var/tmp/backup-cache --jobs 8
```

Prepare a private JSON fixture with `endpoint`, `region`, `bucket`, `prefix`,
`access_key_id`, `secret_access_key`, `session_token` and
`invalid_tls_endpoint`. Use a dedicated disposable bucket and a key restricted
to that bucket. `session_token` may be empty. Set `invalid_tls_endpoint` to a
known self-signed test endpoint. Give the fixture mode `0600`.

From this directory, build and install with explicit signing settings:

```sh
python3 build.py --native-work /private/var/tmp/backup-native --work-dir /private/var/tmp/backup-http-check --fixture /private/var/tmp/storage-fixture.json --device DEVICE --team TEAM --keychain KEYCHAIN --signing-project SIGNING_PROJECT
```

The signing project must provide an `unlock-signing` Make target. The builder
checks its copied source against the passed native receipt, then adds a
separately tagged test entry point. It runs host protocol and TLS controls
before building the phone archive. The production archive keeps its existing
public C header and excludes the test entry point and trust hooks.

The loopback TLS controls use the controlled Go test certificate, accepted
only by the isolated test hook for its exact certificate, address and port.
They exercise upload cancellation, dropped responses, redirect refusal and
authentication refusal. The phone app uses normal platform TLS verification.

Copy the fixture to the installed app and run it in the foreground:

```sh
xcrun devicectl device copy to --device DEVICE --domain-type appDataContainer --domain-identifier app.solstone.journal.backup.http.dev --source /private/var/tmp/storage-fixture.json --destination Documents/http-credential.json
xcrun devicectl device process launch --device DEVICE --console app.solstone.journal.backup.http.dev
```

The app removes its credential file after the check. Collect
`Documents/http-evidence.json`, the console output and `build-receipt.json`.
Require exit 0, matching process identities, `passed: true` and
`credential_file_absent: true`. Memory readings are before/after snapshots,
not peak measurements.

Canaries remain in the disposable bucket for independent readback. Inventory
and remove their versions, unfinished uploads and credentials after testing.
This check establishes protocol behavior; remote repository operations,
recovery, lock reconciliation and normal app admission require separate
qualification. The [local backup device check](../backup/README.md) remains
available independently.
