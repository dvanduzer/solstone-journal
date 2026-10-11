# Android Journal process experiment

This experimental app runs Journal storage, SQLite indexing and search in a bound Android service. The activity and ordinary service share an app UID and use separate processes. A second service uses an isolated UID to test access to the same synthetic journal path.

The package is `app.solstone.experiment.runtime`. It requests no permissions and creates only synthetic data in its own private files directory. Use a dedicated test device. The driver force-stops this experimental package before each run. Uninstalling it deletes its synthetic data.

## Build and run

Start from this experimental branch. Install Rust 1.97.1 with the `aarch64-linux-android` target, JDK 17, Android SDK platform 36, build-tools 35.0.0 and NDK r30. Set `ANDROID_HOME`, `JAVA_HOME` and `SPIKE_NDK` to those installations. Existing Cargo dependencies must be cached for the offline build.

From this directory:

```sh
python3 build_native.py
python3 build_apk.py --root "$PWD" --sdk "$ANDROID_HOME" --java "$JAVA_HOME"
python3 run_device.py full --install
```

The driver names its test handset explicitly in `run_device.py`; change that serial for your bench. Reserve the device before running. It wakes the screen and dismisses an unsecured keyguard, but does not bypass credentials or change security settings. Keep the activity in the foreground for the run.

The full run checks real indexing and an eight-result query over 512 synthetic records, root identity, symlink refusal, locking, worker failure and recovery, SQLite transaction rollback, interrupted atomic replacement and cooperative batch cancellation. It then makes 120 requests with one second between requests. It records process identity, memory and UI timer gaps. This is a bounded experiment, not an endurance qualification.

Additional scenarios are `verify`, `loss`, `exit` and `finish`. `loss` kills the host during a native batch. `exit` kills it after work. `finish` finishes the activity. Follow each with `verify` to check persisted query results. The service cancels its batch when its last binding is destroyed; Android may retain the process after that component has stopped. Force-stop and relaunch are separate from service reconnection.

`evidence/` contains command exits, app events, installed APK hashes and device snapshots. `build/` contains the APK, its member hashes and debug signing material. These directories and native build outputs are excluded from Git.

## Adaptation and limits

The branch changes Android ancestor traversal in `JournalRoot` to use `O_PATH` while keeping requested-root access, no-follow and identity validation. Without that adaptation the library loads, but opening the journal is denied on the tested handset. The experiment does not relax SELinux, app UID restrictions or Journal authority checks.

Create-only publication still fails at its hard-link step on that handset. Atomic replacement and SQLite indexing work for this slice; this does not qualify every Journal writer. The isolated service can load the native libraries but cannot open the ordinary app's private journal path.

The Rust experiment enables the existing `journal-io` test hooks only to kill its worker immediately before a real atomic rename. The SQLite failure case edits the real index inside an uncommitted transaction and kills the process. It does not simulate a crash inside the production scanner's transaction.

Cancellation is checked between complete native work units, not inside indexing or SQLite calls. No model inference, GPU/NPU, multimedia processing, background service guarantee or automatic persistent job recovery is tested. The app and scripts are experimental instrumentation, not a release application or reusable IPC API.
