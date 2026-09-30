# camera_flutter

Flutter integration for [`camera_dart`](../camera_dart): cross-platform
camera capture with the goal of no system dependencies for users or devs.

This package builds the shared `camera_cli` Rust crate automatically (via 
[cargokit](https://github.com/ManyMath/cargokit)) and bundles it with your app,
then re-exports the full `camera_dart` API.

```dart
import 'package:camera_flutter/camera_flutter.dart';

await CameraControlFlutter.ensureInitialized();
final cameras = await CameraControl.enumerate();
```

## Permissions

Camera capture requires a usage description on Apple platforms
(`NSCameraUsageDescription` in your app's Info.plist) and the `CAMERA`
permission on Android.  The example app sets these up.
