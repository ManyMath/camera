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
sibling overrides. Both modes build a fresh Flutter application. Linux also
runs a separate Dart consumer using the newly bundled native library and
checks backend selection and device enumeration. This verifies native loading
and minimal behavior; it does not prove physical camera capture.

Local path consumption needs `resolution: workspace` removed from extracted
pubspec files, because hosted packages do not participate in the source
workspace. The original candidate archives remain unchanged. Consumer
pubspecs and lockfiles record exactly what was tested.

`--target` accepts Flutter build targets. The default is `linux`; use `apk`
for Android (the generated project needs `android` scaffolding), `ios`,
`macos`, or `windows` on suitable hosts. Only Linux currently has an automated
runtime smoke. Other native builds need corresponding runtime/device checks.

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
