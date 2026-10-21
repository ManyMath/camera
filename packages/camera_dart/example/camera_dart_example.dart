// Enumerate the host's cameras, capture one frame, and write it to a PPM.
//
// Pure-Dart CLI usage of camera_dart over the FFI backend. Build the native
// library first (from the repo root):
//
//   cargo build --release --manifest-path packages/camera_flutter/rust/Cargo.toml
//
// then run:
//
//   dart run packages/camera_dart/example/camera_dart_example.dart
//
// It prints the device list, opens the first camera, and saves its first frame
// to /tmp/camera_dart_frame.ppm (a portable format any image viewer reads).

import 'dart:io';
import 'dart:typed_data';

import 'package:camera_dart/camera_dart.dart';

Future<void> main(List<String> args) async {
  final cameras = await CameraControl.enumerate();
  if (cameras.isEmpty) {
    stderr.writeln('No cameras found.');
    return;
  }

  stdout.writeln('Cameras:');
  for (final cam in cameras) {
    stdout.writeln('  ${cam.id}  "${cam.name}"  (${cam.facing.name})');
    for (final f in cam.formats.take(3)) {
      stdout.writeln(
        '      ${f.width}x${f.height} '
        '${f.pixelFormat.name} @ ${f.frameRate.toStringAsFixed(0)}fps',
      );
    }
  }

  final camera = await CameraControl.open(cameras.first);
  stdout.writeln('\nOpened ${camera.device.name}; capturing a frame...');
  try {
    final frame = await camera.captureFrame();
    stdout.writeln(
      'Got ${frame.width}x${frame.height} '
      '${frame.pixelFormat.name}, ${frame.bytes.length} bytes.',
    );
    const path = '/tmp/camera_dart_frame.ppm';
    _writePpm(path, frame);
    stdout.writeln('Wrote $path');
  } finally {
    await camera.close();
  }
}

/// Write a frame to a binary PPM, converting BGRA/RGBA to RGB.
void _writePpm(String path, Frame frame) {
  final w = frame.width, h = frame.height;
  final src = frame.bytes;
  final rgb = BytesBuilder();
  final swap = frame.pixelFormat == PixelFormat.bgra8888;
  for (var i = 0; i + 3 < src.length; i += 4) {
    if (swap) {
      rgb.addByte(src[i + 2]); // R
      rgb.addByte(src[i + 1]); // G
      rgb.addByte(src[i]); // B
    } else {
      rgb.addByte(src[i]); // R
      rgb.addByte(src[i + 1]); // G
      rgb.addByte(src[i + 2]); // B
    }
  }
  File(path)
    ..writeAsBytesSync('P6\n$w $h\n255\n'.codeUnits, mode: FileMode.write)
    ..writeAsBytesSync(rgb.takeBytes(), mode: FileMode.append);
}
