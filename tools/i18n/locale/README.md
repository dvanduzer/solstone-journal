# test locale activation

These helpers belong in test harnesses. They do not declare product language support.

On glibc hosts:

```sh
tools/i18n/locale/run-glibc de_DE.UTF-8 de_DE Oktober -- COMMAND ARGUMENTS
tools/i18n/locale/verify-glibc
```

The runner builds a private locale with `localedef`, then checks libc's resolved locale and October's translated month name before starting the command with the same environment. It requires a C compiler and installed locale definitions. Nothing is installed system-wide. The command's exit status is preserved. Rust standard formatting ignores the process locale; this harness proves libc activation and the environment inherited by subprocesses. Inspect a production subprocess's environment where it matters.

On a Mac with Xcode:

```sh
tools/i18n/locale/verify-apple
```

The verification script compiles a disposable test executable with embedded test-only localization declarations and constructs a scratch resource bundle. It checks Turkish, German and Arabic test cases, rejects a wrong process locale, and rejects a bundle without the requested declaration. Temporary files are removed on exit.

For Apple test targets, call `AppleLocaleCanary.verify` inside the actual test process before accepting the run. Set the test language and region through the test runner (`AppleLanguages` and `AppleLocale` for a launched process); host `LANG` alone is insufficient. Include the requested language in the test executable and test bundle's declared localizations. `test-localizations.plist` and `apple-test-main.swift` are scaffolding for these synthetic checks and must stay out of application packaging. For SwiftUI rendering tests, also set the view's locale environment.

The command-line Foundation proof does not qualify iOS simulator or SwiftUI rendering. Those require the actual test target to invoke the canary during integration. These source files are AGPL-3.0-only; the system compiler and glibc locale tools are build prerequisites.
