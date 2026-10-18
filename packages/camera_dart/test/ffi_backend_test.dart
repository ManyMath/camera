// Exercises the FFI backend against fake_camera_cli.c, compiled with the
// host C compiler. Skipped where there is no `cc`.
@TestOn('linux || mac-os')
library;

import 'dart:async';
import 'dart:ffi';
import 'dart:io';
import 'dart:isolate';

import 'package:camera_dart/camera_dart.dart';
import 'package:camera_dart/src/backends/native/native_backend_ffi.dart';
import 'package:test/test.dart';

/// Builds the fake library, returning its path, or null without a compiler.
String? _buildFake() {
  final lib = Isolate.resolvePackageUriSync(
    Uri.parse('package:camera_dart/camera_dart.dart'),
  );
  if (lib == null) return null;
  final source = lib.resolve('../test/fake_camera_cli.c').toFilePath();
  final out =
      '${Directory.systemTemp.createTempSync('camera_dart').path}'
      '/libfake_camera_cli${Platform.isMacOS ? '.dylib' : '.so'}';
  try {
    final r = Process.runSync('cc', [
      '-shared',
      '-fPIC',
      '-o',
      out,
      source,
      '-lpthread',
    ]);
    return r.exitCode == 0 ? out : null;
  } on ProcessException {
    return null;
  }
}

void main() {
  final path = _buildFake();
  if (path == null) {
    test('FFI backend', () {}, skip: 'no C compiler to build the fake library');
    return;
  }
  NativeLibrary.overridePath = path;
  final fake = DynamicLibrary.open(path);
  final startCount = fake.lookupFunction<Int32 Function(), int Function()>(
    'fake_start_count',
  );
  final failNextStart = fake.lookupFunction<Void Function(), void Function()>(
    'fake_fail_next_start',
  );
  final endAfter = fake
      .lookupFunction<Void Function(Int32), void Function(int)>(
        'fake_end_after',
      );

  late CameraDevice device;

  setUpAll(() async {
    final devices = await CameraControl.enumerate();
    expect(CameraBackendRegistry.instance.selected.name, 'ffi');
    device = devices.first;
  });

  tearDown(() => endAfter(-1));

  test('enumerate parses the native device list', () async {
    final devices = await CameraControl.enumerate();
    expect(devices.map((d) => d.id), ['fake0', 'fake1']);
    expect(devices[1].facing, CameraFacing.front);
    expect(devices.first.formats.single.toString(), '2x2 bgra8888 @30fps');
  });

  test('captureFrame can be called repeatedly', () async {
    final camera = await CameraControl.open(device);
    final a = await camera.captureFrame();
    final b = await camera.captureFrame();
    expect(a.bytes, hasLength(16));
    expect(b.timestamp, greaterThan(Duration.zero));
    await camera.close();
  });

  test('capture starts when the stream is listened to', () async {
    final camera = await CameraControl.open(device);
    final before = startCount();
    final frames = camera.frames();
    await Future<void>.delayed(const Duration(milliseconds: 20));
    expect(startCount(), before);
    await frames.first;
    expect(startCount(), before + 1);
    await camera.close();
  });

  test('close ends an active frame stream', () async {
    final camera = await CameraControl.open(device);
    final done = Completer<void>();
    camera.frames().listen((_) {}, onDone: done.complete);
    await Future<void>.delayed(const Duration(milliseconds: 20));
    await camera.close();
    await done.future.timeout(const Duration(seconds: 2));
  });

  test('a failed start does not stop the stream that replaced it', () async {
    final camera = await CameraControl.open(device);
    failNextStart();
    final failed = Completer<Object>();
    camera.frames().listen((_) {}, onError: failed.complete, onDone: () {});
    var frames = 0;
    camera.frames().listen((_) => frames++);
    expect(await failed.future, isA<CaptureException>());
    await Future<void>.delayed(const Duration(milliseconds: 20));
    final after = frames;
    await Future<void>.delayed(const Duration(milliseconds: 100));
    expect(frames, greaterThan(after));
    await camera.close();
  });

  test('frames are dropped, not queued, while paused', () async {
    final camera = await CameraControl.open(device);
    var frames = 0;
    final sub = camera.frames().listen((_) => frames++);
    await Future<void>.delayed(const Duration(milliseconds: 30));
    sub.pause();
    final atPause = frames;
    await Future<void>.delayed(const Duration(milliseconds: 200));
    sub.resume();
    await Future<void>.delayed(Duration.zero);
    expect(frames - atPause, lessThanOrEqualTo(2));
    await sub.cancel();
    await camera.close();
  });

  test('a stream that ends natively fails with CaptureException', () async {
    final camera = await CameraControl.open(device);
    endAfter(0);
    await expectLater(camera.captureFrame(), throwsA(isA<CaptureException>()));
    endAfter(3);
    final events = <String>[];
    final ended = Completer<void>();
    camera.frames().listen(
      (_) => events.add('frame'),
      onError: (Object e) => events.add('error'),
      onDone: () {
        events.add('done');
        ended.complete();
      },
    );
    await ended.future.timeout(const Duration(seconds: 2));
    expect(events, ['frame', 'frame', 'frame', 'error', 'done']);
    await camera.close();
  });

  test('dispose closes cameras that are still streaming', () async {
    final camera = await CameraControl.open(device);
    final done = Completer<void>();
    camera.frames().listen((_) {}, onDone: done.complete);
    await Future<void>.delayed(const Duration(milliseconds: 20));
    await CameraControl.dispose();
    await done.future.timeout(const Duration(seconds: 2));
    expect(camera.frames, throwsA(isA<CaptureException>()));
  });
}
