# Exact package payload verification

Run with Python 3.11+, Dart 3.13.2, Flutter, Cargo and the target platform's
normal build tools. The pinned Dart version is deliberate: `--to-archive` is a
hidden pub flag. Updating that pin requires rechecking local-only behavior.
The tool has no publication fallback and never invokes an ordinary publish.

```sh
python3 -m unittest discover -s tool/tests -v
python3 tool/native_payload.py --config tool/payload.json --mode candidate --output /tmp/camera-candidate
python3 tool/native_payload.py --config tool/payload.json --mode hosted --output /tmp/camera-hosted
```

Each output directory must be new. The tool snapshots candidate sources into a
temporary Git checkout, then runs pub validation and archive generation as
**separate** operations. Pub's archive selection, including ignore rules, is
used. The archives are extracted outside the repository. Required sources,
Cargo local dependency closure and platform build references are checked.
Actual archives, SHA256 values and logs are retained in the output directory.
No credentials or signing keys are required.

`candidate` validates the real declared package constraints using Dart
`pub_semver` against the original archived manifests. The candidate solver result is explicit and independent of source overrides. Only after that check does the fresh
Flutter consumer override sibling sources with extracted candidate packages.
`hosted` uses the Flutter archive's declared hosted dependencies without
sibling overrides. Both modes build a fresh Flutter application. Linux and
Windows also run a separate Dart consumer that explicitly opens the newly
bundled native library by path and checks backend selection and device
enumeration; the library path and SHA256 are recorded. This verifies native
loading and minimal behavior; it does not prove physical camera capture.

`--to-archive` writes locally but still runs pub validation, which reads the
hosted server's version listing for the package. It never uploads.

Local path consumption needs `resolution: workspace` removed from extracted
pubspec files, because hosted packages do not participate in the source
workspace. The original candidate archives remain unchanged. Consumer
pubspecs and lockfiles record exactly what was tested.

`--target` accepts Flutter build targets. The default is `linux`; use `apk`
for Android (the generated project needs `android` scaffolding), `ios`,
`macos`, or `windows` on suitable hosts. Linux and Windows have an automated
runtime smoke. Other native builds need corresponding runtime/device checks.

Cargokit builds consumers with `rustup run stable`, so the consumer's native
library uses the host's `stable` toolchain, not `RUSTUP_TOOLCHAIN`.

Fixtures retain regressions for missing native sources, escaping Cargo path
dependencies, incorrect Flutter CMake target names, archive path traversal,
and candidate constraints that overrides would otherwise hide.

The reusable helper is intended to be maintained in the private ecosystem
maintenance tooling repository and copied here with explicit provenance.
Consumer packages have no dependency on that private repository.

## Private CI

`.gitea/workflows/verify.yml` runs the Linux gate on the provisioned
`manymath-maintenance-linux-x64` host runner and checks the server identity
before checkout. It performs no package upload, release creation, signing, or
public workflow execution. Candidate archives/logs remain on the private runner;
the run log records their directory and checksums. Other advertised native
platforms require corresponding runners and runtime checks before release.

The Windows automation explicitly opens and hashes the bundle path but does not
independently inspect the module selected by every backend symbol lookup. The
fleet validation separately inspected the loaded module path; neither check
proves physical capture or permissions behavior.

Android APK consumers use the checked-in target map and the installed stable
Rust toolchain; its actual compiler identity is recorded. Set ANDROID_SDK_ROOT
to an SDK with platform36, build-tools36.0.0 and NDK28.2.13676358 already installed
(Glow also needs CMake3.22.1). Run the usual gate with --target apk. The gate
fails before building when tools/targets are absent, disables SDK/JDK downloads
in the fresh consumer, and checks Gradle's actual selected NDK. Child builds
receive the selected Rust toolchain without changing user settings. Gradle and
Android homes default to task-local output directories; explicitly supplied
task-owned homes may be reused. Evidence binds the APK and every expected
native ABI to SHA256. This is build/packaging validation, not device behavior.

The private `android-payload.yml` lane runs on the dedicated maintenance branch or manual dispatch. It uses isolated temporary dependency/build homes and retains archive evidence and the APK, then removes its own intermediate build/cache directories. Linux CI remains the required default lane; Android is an additional build qualification, not a device test.
