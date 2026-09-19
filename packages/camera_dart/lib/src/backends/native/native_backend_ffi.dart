import 'dart:async';
import 'dart:convert';
import 'dart:ffi';
import 'dart:io';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

import '../../camera.dart';
import '../../camera_backend.dart';
import '../../camera_device.dart';
import '../../camera_format.dart';
import '../../frame.dart';
import '../../exceptions.dart';

// ignore_for_file: non_constant_identifier_names

/// Locates and opens the `camera_cli` shared library.
///
/// In a Flutter app the native crate is force-linked into the plugin framework
/// loaded into the host process, so there is no standalone library file to
/// open: `DynamicLibrary.process()` resolves the symbols instead. For CLI/dev
/// use the loader falls back to an env override, the bare library name, and the
/// in-repo build outputs.
class NativeLibrary {
  /// Explicit path override, highest priority. Set before first use.
  static String? overridePath;

  static DynamicLibrary? _opened;
  static bool _attempted = false;

  static String get _fileName => switch (Platform.operatingSystem) {
    'windows' => 'camera_cli.dll',
    'macos' || 'ios' => 'libcamera_cli.dylib',
    _ => 'libcamera_cli.so',
  };

  /// Opens the library, caching the result. Returns null if it cannot be
  /// found or loaded: never throws.
  static DynamicLibrary? open() {
    if (_attempted) return _opened;
    _attempted = true;
    for (final candidate in _candidates()) {
      try {
        _opened = DynamicLibrary.open(candidate);
        return _opened;
      } on Object {
        // Try the next candidate.
      }
    }
    // The plugin framework is loaded into the host process and the crate's
    // symbols are -force_load'd into it, so process() resolves them even when
    // open() cannot find a file by name.
    try {
      final p = DynamicLibrary.process();
      // Probe one expected symbol to confirm FFI can find it.
      p.lookup<NativeFunction<_EnumerateNative>>('camera_cli_enumerate');
      _opened = p;
    } on Object {
      // Fall through.
    }
    return _opened;
  }

  static Iterable<String> _candidates() sync* {
    if (overridePath != null) yield overridePath!;
    final fromEnv = Platform.environment['CAMERA_CLI_LIBRARY'];
    if (fromEnv != null && fromEnv.isNotEmpty) yield fromEnv;
    // Resolved via the OS loader path / Flutter app bundle.
    yield _fileName;
    // On macOS/iOS the symbols are -force_load'd into the plugin framework, so
    // the lib-name lookup above fails; the Flutter loader resolves the
    // framework binary by its short path.
    if (Platform.isMacOS || Platform.isIOS) {
      yield 'camera_flutter.framework/camera_flutter';
    }
    // Developer builds of the in-repo crate, searched from the cwd upward.
    var dir = Directory.current.absolute;
    for (var i = 0; i < 6; i++) {
      for (final profile in ['release', 'debug']) {
        yield '${dir.path}/native/camera_cli/target/$profile/$_fileName';
      }
      final parent = dir.parent;
      if (parent.path == dir.path) break;
      dir = parent;
    }
  }
}

// camera_cli_enumerate() -> *char (JSON, caller frees with
// camera_cli_string_free). Null on failure.
typedef _EnumerateNative = Pointer<Utf8> Function();
typedef _EnumerateDart = Pointer<Utf8> Function();

typedef _StringFreeNative = Void Function(Pointer<Utf8> s);
typedef _StringFreeDart = void Function(Pointer<Utf8> s);

// camera_cli_open(device_id) -> handle (> 0 ok, <= 0 error).
typedef _OpenNative = Int32 Function(Pointer<Utf8> deviceId);
typedef _OpenDart = int Function(Pointer<Utf8> deviceId);

// camera_cli_close(handle).
typedef _CloseNative = Void Function(Int32 handle);
typedef _CloseDart = void Function(int handle);

// FrameCallback: invoked per frame on the capture thread. `data` is a heap
// buffer the callee owns and must free with camera_cli_frame_free.
typedef _FrameCallbackNative =
    Void Function(
      Pointer<Uint8> data,
      Int32 length,
      Int32 width,
      Int32 height,
      Int32 pixelFormat,
      Int64 timestampUs,
    );

// camera_cli_start_stream(handle, callback) -> 0 ok, < 0 error.
typedef _StartStreamNative =
    Int32 Function(
      Int32 handle,
      Pointer<NativeFunction<_FrameCallbackNative>> callback,
    );
typedef _StartStreamDart =
    int Function(
      int handle,
      Pointer<NativeFunction<_FrameCallbackNative>> callback,
    );

// camera_cli_stop_stream(handle) -> 0 ok, < 0 error.
typedef _StopStreamNative = Int32 Function(Int32 handle);
typedef _StopStreamDart = int Function(int handle);

// camera_cli_frame_free(data, length): frees a frame buffer.
typedef _FrameFreeNative = Void Function(Pointer<Uint8> data, Int32 length);
typedef _FrameFreeDart = void Function(Pointer<Uint8> data, int length);

// camera_cli_close_all(): stop and release every session, synchronously.
typedef _CloseAllNative = Void Function();
typedef _CloseAllDart = void Function();

/// Lazily-resolved native entry points.
class _Native {
  _Native(DynamicLibrary lib)
    : enumerate = lib
          .lookup<NativeFunction<_EnumerateNative>>('camera_cli_enumerate')
          .asFunction(),
      stringFree = lib
          .lookup<NativeFunction<_StringFreeNative>>('camera_cli_string_free')
          .asFunction(),
      open = lib
          .lookup<NativeFunction<_OpenNative>>('camera_cli_open')
          .asFunction(),
      close = lib
          .lookup<NativeFunction<_CloseNative>>('camera_cli_close')
          .asFunction(),
      startStream = lib
          .lookup<NativeFunction<_StartStreamNative>>('camera_cli_start_stream')
          .asFunction(),
      stopStream = lib
          .lookup<NativeFunction<_StopStreamNative>>('camera_cli_stop_stream')
          .asFunction(),
      frameFree = lib
          .lookup<NativeFunction<_FrameFreeNative>>('camera_cli_frame_free')
          .asFunction(),
      closeAll = lib
          .lookup<NativeFunction<_CloseAllNative>>('camera_cli_close_all')
          .asFunction();

  final _EnumerateDart enumerate;
  final _StringFreeDart stringFree;
  final _OpenDart open;
  final _CloseDart close;
  final _StartStreamDart startStream;
  final _StopStreamDart stopStream;
  final _FrameFreeDart frameFree;
  final _CloseAllDart closeAll;
}

/// Creates the FFI backend. Availability is probed lazily via [isAvailable], so
/// the backend is always registered and a load failure surfaces as an
/// unavailable backend rather than a silently missing one.
CameraBackend? createNativeBackend() => FfiBackend._();

/// Camera backend backed by the native crate over `dart:ffi`.
class FfiBackend implements CameraBackend {
  FfiBackend._();

  _Native? _native;
  bool _available = false;
  bool _probed = false;

  /// Cameras opened through this backend and not yet closed.
  final Set<_FfiCamera> _cameras = {};

  @override
  String get name => 'ffi';

  @override
  bool get isAvailable {
    if (_probed) return _available;
    _probed = true;
    final lib = NativeLibrary.open();
    if (lib == null) return _available = false;
    try {
      _native = _Native(lib);
      _available = true;
    } on Object {
      _available = false;
    }
    return _available;
  }

  @override
  int get priority => 100;

  @override
  Future<void> dispose() async {
    // Close each camera so its frame callback is released and its stream gets
    // done; an open NativeCallable keeps the isolate alive.
    for (final camera in _cameras.toList()) {
      await camera.close();
    }
    // Stop every native session synchronously. Mainly a safety net for shutdown
    // so the capture thread cannot call back into a torn-down isolate.
    _native?.closeAll();
  }

  @override
  Future<List<CameraDevice>> enumerate() async {
    if (!isAvailable) {
      throw const DeviceException(
        'camera_cli native library could not be loaded',
      );
    }
    final native = _native!;
    final ptr = native.enumerate();
    if (ptr == nullptr) {
      throw const DeviceException('Native enumerate failed');
    }
    try {
      final json = ptr.toDartString();
      return _parseDevices(json);
    } finally {
      native.stringFree(ptr);
    }
  }

  @override
  Future<Camera> open(CameraDevice device) async {
    if (!isAvailable) {
      throw const DeviceException(
        'camera_cli native library could not be loaded',
      );
    }
    final native = _native!;
    final idPtr = device.id.toNativeUtf8();
    try {
      final handle = native.open(idPtr);
      if (handle <= 0) {
        throw DeviceException('Native open failed (code $handle)');
      }
      final format = device.formats.isNotEmpty
          ? device.formats.first
          : const CameraFormat(
              width: 0,
              height: 0,
              pixelFormat: PixelFormat.bgra8888,
              frameRate: 0,
            );
      final camera = _FfiCamera(
        native,
        handle,
        device,
        format,
        _cameras.remove,
      );
      _cameras.add(camera);
      return camera;
    } finally {
      malloc.free(idPtr);
    }
  }
}

List<CameraDevice> _parseDevices(String json) {
  final decoded = jsonDecode(json);
  if (decoded is! List) return const [];
  return decoded.map((d) {
    final m = d as Map<String, dynamic>;
    final formats = (m['formats'] as List? ?? const []).map((f) {
      final fm = f as Map<String, dynamic>;
      return CameraFormat(
        width: fm['width'] as int,
        height: fm['height'] as int,
        pixelFormat: _pixelFormatByName(fm['pixel_format'] as String?),
        frameRate: (fm['frame_rate'] as num).toDouble(),
      );
    }).toList();
    return CameraDevice(
      id: m['id'] as String,
      name: m['name'] as String? ?? '',
      facing: _facingByName(m['facing'] as String?),
      formats: formats,
    );
  }).toList();
}

PixelFormat _pixelFormatByName(String? name) {
  switch (name) {
    case 'rgba8888':
      return PixelFormat.rgba8888;
    case 'yuyv422':
      return PixelFormat.yuyv422;
    case 'mjpeg':
      return PixelFormat.mjpeg;
    case 'bgra8888':
    default:
      return PixelFormat.bgra8888;
  }
}

CameraFacing _facingByName(String? name) {
  switch (name) {
    case 'front':
      return CameraFacing.front;
    case 'back':
      return CameraFacing.back;
    case 'external':
    default:
      return CameraFacing.external;
  }
}

class _FfiCamera implements Camera {
  _FfiCamera(
    this._native,
    this._handle,
    this.device,
    this.format,
    this._onClose,
  );

  final _Native _native;
  final int _handle;
  final void Function(_FfiCamera) _onClose;
  bool _closed = false;

  StreamController<Frame>? _controller;
  NativeCallable<_FrameCallbackNative>? _callback;

  @override
  final CameraDevice device;

  @override
  final CameraFormat format;

  @override
  Future<Frame> captureFrame() {
    // Take the first frame from the stream; `first` cancels its subscription
    // when it completes, which tears the stream back down via onCancel.
    return frames().first;
  }

  @override
  Stream<Frame> frames() {
    if (_closed) {
      throw const CaptureException('Camera is closed');
    }
    // One active stream per camera: until it is cancelled, frames() returns
    // the same single-subscription stream.
    final existing = _controller;
    if (existing != null) return existing.stream;

    late final StreamController<Frame> controller;
    controller = StreamController<Frame>(
      onListen: () => _startStream(controller),
      onCancel: () => _stopStream(controller),
    );
    _controller = controller;
    return controller.stream;
  }

  void _startStream(StreamController<Frame> controller) {
    // Closed (or superseded) before anyone listened: nothing to start.
    if (_closed || !identical(controller, _controller)) return;
    // The listener runs on this isolate's event loop; the native side invokes
    // it from the capture thread.
    final callback = NativeCallable<_FrameCallbackNative>.listener(_onFrame);
    _callback = callback;
    final rc = _native.startStream(_handle, callback.nativeFunction);
    if (rc < 0) {
      _teardown();
      controller.addError(CaptureException('Native start_stream failed ($rc)'));
      controller.close();
    }
  }

  void _onFrame(
    Pointer<Uint8> data,
    int length,
    int width,
    int height,
    int pixelFormat,
    int timestampUs,
  ) {
    final controller = _controller;
    if (controller == null || controller.isClosed || controller.isPaused) {
      // No consumer, or a paused one: free the buffer and drop the frame
      // rather than queueing every frame until the listener catches up.
      _native.frameFree(data, length);
      return;
    }
    // Copy out of the native buffer, then free it. A zero-copy path
    // (asTypedList with a native finalizer) needs a single-argument free
    // function; that is a future optimization.
    final bytes = Uint8List.fromList(data.asTypedList(length));
    _native.frameFree(data, length);
    controller.add(
      Frame(
        bytes: bytes,
        width: width,
        height: height,
        pixelFormat: PixelFormat.values[pixelFormat],
        timestamp: Duration(microseconds: timestampUs),
      ),
    );
  }

  void _stopStream(StreamController<Frame> controller) {
    // A stale controller (one whose start failed, then got its done event after
    // frames() had already started a new stream) must not stop the new one.
    if (_closed || !identical(controller, _controller)) return;
    _native.stopStream(_handle);
    _teardown();
  }

  void _teardown() {
    _callback?.close();
    _callback = null;
    _controller = null;
  }

  @override
  Future<void> close() async {
    if (_closed) return;
    _closed = true;
    _native.close(_handle);
    _onClose(this);
    // Grab the controller before _teardown clears it, so listeners get done.
    final controller = _controller;
    _teardown();
    if (controller != null && !controller.isClosed) {
      // Not awaited: the done future only completes once the event reaches a
      // listener, and a stream that was never listened to has none.
      unawaited(controller.close());
    }
  }
}
