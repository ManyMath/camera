# camera

Camera capture for Dart and Flutter across platforms. Native builds
require Rust in addition to Dart or Flutter. Precompiled binaries are planned
to remove that build requirement.

This is a [melos](https://melos.invertase.dev) monorepo:

| Package | What it is |
|---|---|
| [`packages/camera_dart`](packages/camera_dart) | Pure Dart (no Flutter); works in CLI tools. Reaches native camera over `dart:ffi`. |
| [`packages/camera_flutter`](packages/camera_flutter) | Flutter plugin. Builds the native library automatically (via cargokit) and re-exports the `camera_dart` API. |
| [`packages/camera_flutter/rust`](packages/camera_flutter/rust) | Rust crate. |

## Status

| Host | Status |
|---|---|
| Android | Working |
| iOS | Working |
| macOS | Working |
| Ubuntu 24.04 | Working |
| Web | Working |
| Windows 11 | Working |

Web tested on macOS in Chrome and Safari, Ubuntu 24.04 in Chrome and Firefox, and Windows 11 in Chrome and Edge.

## Development

Driven by [melos](https://melos.invertase.dev) (config lives in the root
`pubspec.yaml`). After `dart pub get` (or `flutter pub get`):

```sh
dart run melos run native:build   # build the Rust shared library
dart run melos run analyze        # analyze camera_dart + camera_flutter
dart run melos run format         # check Dart formatting (format:fix to apply)
dart run melos run test           # Dart + Flutter tests
dart run melos run native:test    # Rust crate tests
```

## Package candidates and verification

The native crate lives in `packages/camera_flutter/rust` so its complete source
ships in the Flutter publication archive. See [the payload gate](tool/README.md)
for actual pub archive checks, fresh consumer builds and native loading checks.

The next corrective candidates are `camera_dart` 0.0.2 and `camera_flutter`
0.0.2. The Flutter package supports the verified Dart versions 0.0.1 and 0.0.2
(`^0.0.1`), allowing both existing hosted consumers and coordinated
candidate testing. Publish Dart first in the owner's later public phase.
Private validation creates archives only; it does not publish packages.
