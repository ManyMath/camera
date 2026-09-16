import 'dart:async';
import 'dart:js_interop';
import 'dart:typed_data';

import 'package:web/web.dart' as web;

import '../../camera.dart';
import '../../camera_backend.dart';
import '../../camera_device.dart';
import '../../camera_format.dart';
import '../../exceptions.dart';
import '../../frame.dart';

/// Web camera backend using getUserMedia and the mediaDevices API.
///
/// There is no `dart:ffi` on the web; camera access goes through the browser.
/// getUserMedia triggers the permission prompt and unlocks device labels, so
/// [enumerate] requests access first, then lists devices. Frames are pulled by
/// drawing the video element to an offscreen canvas and reading back RGBA
/// pixels per decoded frame.
CameraBackend? createNativeBackend() => WebBackend();

/// Camera backend backed by the browser mediaDevices API.
class WebBackend implements CameraBackend {
  /// Cameras opened through this backend and not yet closed.
  final Set<_WebCamera> _cameras = {};

  @override
  String get name => 'web';

  @override
  bool get isAvailable => true;

  @override
  int get priority => 50;

  @override
  Future<void> dispose() async {
    for (final camera in _cameras.toList()) {
      await camera.close();
    }
  }

  @override
  Future<List<CameraDevice>> enumerate() async {
    final devices = web.window.navigator.mediaDevices;

    // Requesting a stream prompts for permission and unlocks device labels.
    // Stop it immediately: we only needed the grant.
    try {
      final probe = await devices
          .getUserMedia(web.MediaStreamConstraints(video: true.toJS))
          .toDart;
      for (final track in probe.getTracks().toDart) {
        track.stop();
      }
    } catch (e) {
      throw DeviceException('Camera access denied or unavailable: $e');
    }

    final list = await devices.enumerateDevices().toDart;
    final out = <CameraDevice>[];
    for (final info in list.toDart) {
      if (info.kind != 'videoinput') continue;
      out.add(
        CameraDevice(
          id: info.deviceId,
          name: info.label.isEmpty ? 'Camera ${out.length + 1}' : info.label,
          // The browser does not surface facing for desktop webcams.
          facing: CameraFacing.external,
        ),
      );
    }
    return out;
  }

  @override
  Future<Camera> open(CameraDevice device) async {
    final devices = web.window.navigator.mediaDevices;
    final stream = await _openStream(devices, device);
    final camera = _WebCamera(device, stream, _cameras.remove);
    _cameras.add(camera);
    try {
      await camera._start();
    } catch (_) {
      // Stop the tracks we just acquired rather than leaking a live camera.
      await camera.close();
      rethrow;
    }
    return camera;
  }

  /// Open a stream for [device], preferring it but never hard-failing if it
  /// cannot be matched.
  ///
  /// The chosen device is requested via `deviceId: {exact: id}` (a JS object,
  /// not a bare string -- the bare-string form is unreliable through the
  /// interop layer). If that throws (e.g. the id went stale between enumerate
  /// and open, or the device is busy) we fall back to any available camera so
  /// the user still gets a preview rather than a NotFoundError.
  Future<web.MediaStream> _openStream(
    web.MediaDevices devices,
    CameraDevice device,
  ) async {
    final byId = web.MediaStreamConstraints(
      video:
          web.MediaTrackConstraints(
                deviceId:
                    web.ConstrainDOMStringParameters(exact: device.id.toJS)
                        as JSAny,
              )
              as JSAny,
    );
    final anyCamera = web.MediaStreamConstraints(video: true.toJS);
    Object? lastError;
    for (final constraints in [byId, anyCamera]) {
      try {
        return await devices.getUserMedia(constraints).toDart;
      } catch (e) {
        lastError = e;
      }
    }
    throw DeviceException('Failed to open camera: $lastError');
  }
}

class _WebCamera implements Camera {
  _WebCamera(this.device, this._stream, this._onClose)
    : _video = web.HTMLVideoElement(),
      _canvas = web.HTMLCanvasElement();

  final web.MediaStream _stream;
  final void Function(_WebCamera) _onClose;
  final web.HTMLVideoElement _video;
  final web.HTMLCanvasElement _canvas;
  web.CanvasRenderingContext2D? _ctx;

  bool _closed = false;
  StreamController<Frame>? _controller;
  int? _frameRequest;

  @override
  final CameraDevice device;

  @override
  CameraFormat get format => CameraFormat(
    width: _video.videoWidth,
    height: _video.videoHeight,
    pixelFormat: PixelFormat.rgba8888,
    frameRate: 0,
  );

  Future<void> _start() async {
    _video
      ..autoplay = true
      ..muted = true
      ..srcObject = _stream;
    await _video.play().toDart;
    _ctx = _canvas.getContext('2d') as web.CanvasRenderingContext2D?;
  }

  @override
  Future<Frame> captureFrame() => frames().first;

  @override
  Stream<Frame> frames() {
    if (_closed) throw const CaptureException('Camera is closed');
    final existing = _controller;
    if (existing != null) return existing.stream;

    // Cancelling stops the frame loop; the camera stays open until close().
    late final StreamController<Frame> controller;
    controller = StreamController<Frame>(
      onCancel: () => _stopFrames(controller),
    );
    _controller = controller;
    _scheduleFrame();
    return controller.stream;
  }

  void _scheduleFrame() {
    // requestVideoFrameCallback fires once per decoded frame (supported by
    // Chrome, Brave, and Safari 16+); the callback gets (now, metadata).
    _frameRequest = _video.requestVideoFrameCallback(
      ((JSNumber now, JSObject _) => _tick(now.toDartDouble)).toJS,
    );
  }

  /// Ends the frame loop for [controller], if it is still the current one.
  ///
  /// The pending frame callback is cancelled too: left queued, it would see a
  /// controller from a later frames() call and run a second loop beside it.
  void _stopFrames(StreamController<Frame> controller) {
    if (!identical(controller, _controller)) return;
    _controller = null;
    final request = _frameRequest;
    _frameRequest = null;
    if (request != null) _video.cancelVideoFrameCallback(request);
  }

  /// [nowMs] is the callback's `now`, a monotonic DOMHighResTimeStamp in ms.
  void _tick(double nowMs) {
    _frameRequest = null;
    final controller = _controller;
    if (_closed || controller == null || controller.isClosed) return;
    final w = _video.videoWidth;
    final h = _video.videoHeight;
    final ctx = _ctx;
    if (ctx != null && w > 0 && h > 0) {
      _canvas
        ..width = w
        ..height = h;
      ctx.drawImage(_video, 0, 0);
      final image = ctx.getImageData(0, 0, w, h);
      controller.add(
        Frame(
          // toDart yields a Uint8ClampedList aliasing the ImageData buffer;
          // copy so the frame stays valid past the next tick.
          bytes: Uint8List.fromList(image.data.toDart),
          width: w,
          height: h,
          pixelFormat: PixelFormat.rgba8888,
          timestamp: Duration(microseconds: (nowMs * 1000).round()),
        ),
      );
    }
    _scheduleFrame();
  }

  @override
  Future<void> close() async {
    if (_closed) return;
    _closed = true;
    final controller = _controller;
    if (controller != null) _stopFrames(controller);
    for (final track in _stream.getTracks().toDart) {
      track.stop();
    }
    _video.srcObject = null;
    _onClose(this);
    if (controller != null && !controller.isClosed) {
      // Not awaited: the done future only completes once the event reaches a
      // listener, and a stream that was never listened to has none.
      unawaited(controller.close());
    }
  }
}
