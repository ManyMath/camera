import 'dart:async';

import 'camera_device.dart';
import 'camera_format.dart';
import 'frame.dart';

/// An open camera.
///
/// Returned by [CameraControl.open]. Holds native resources until [close];
/// capture single frames with [captureFrame] or a continuous feed with
/// [frames]. Backends subclass this.
abstract class Camera {
  /// The device this handle was opened from.
  CameraDevice get device;

  /// The active capture format.
  CameraFormat get format;

  /// Capture a single frame.
  ///
  /// Starts the device if needed, returns the next available frame. Throws
  /// a `CaptureException` on failure.
  Future<Frame> captureFrame();

  /// A continuous stream of frames.
  ///
  /// Starts capture on first listen and stops when the subscription is
  /// cancelled. Frames that arrive while the subscription is paused (as in a
  /// slow `await for` body) are dropped, so a slow listener sees recent
  /// frames rather than a growing backlog.
  Stream<Frame> frames();

  /// Stop capture and release native resources. Idempotent.
  Future<void> close();
}
