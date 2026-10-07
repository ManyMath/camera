import 'dart:async';
import 'dart:ui' as ui;

import 'package:camera_flutter/camera_flutter.dart';
import 'package:flutter/material.dart';

void main() {
  runApp(const ExampleApp());
}

class ExampleApp extends StatelessWidget {
  const ExampleApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'camera_flutter example',
      theme: ThemeData(colorSchemeSeed: Colors.indigo, useMaterial3: true),
      home: const HomePage(),
    );
  }
}

class HomePage extends StatefulWidget {
  const HomePage({super.key});

  @override
  State<HomePage> createState() => _HomePageState();
}

class _HomePageState extends State<HomePage> {
  String _backend = '';
  List<CameraDevice> _devices = const [];
  String? _error;

  Camera? _camera;
  StreamSubscription<Frame>? _sub;
  ui.Image? _preview;
  bool _streaming = false;
  bool _decoding = false;
  late final AppLifecycleListener _lifecycle;

  @override
  void initState() {
    super.initState();
    // Stop capture before the app exits so the native capture thread cannot
    // deliver a frame into a torn-down isolate. onExitRequested awaits the
    // async stop before allowing exit; on Apple platforms the Runner's
    // AppDelegate calls camera_cli_close_all from applicationWillTerminate as
    // the synchronous belt-and-suspenders for hard quits.
    _lifecycle = AppLifecycleListener(
      onExitRequested: () async {
        await _stop();
        return ui.AppExitResponse.exit;
      },
    );
    _load();
  }

  Future<void> _load() async {
    try {
      final backend = await CameraControlFlutter.ensureInitialized();
      final devices = await CameraControl.enumerate();
      setState(() {
        _backend = backend;
        _devices = devices;
        _error = null;
      });
    } catch (e) {
      setState(() => _error = '$e');
    }
  }

  Future<void> _start(CameraDevice device) async {
    await _stop();
    try {
      final camera = await CameraControl.open(device);
      _camera = camera;
      _streaming = true;
      _sub = camera.frames().listen(
        _onFrame,
        onError: (Object e) => setState(() => _error = '$e'),
      );
      setState(() {});
    } catch (e) {
      setState(() => _error = '$e');
    }
  }

  void _onFrame(Frame frame) {
    // Drop frames while a decode is in flight so work never piles up behind a
    // slow paint: always show the freshest frame, never a backlog.
    if (_decoding || !_streaming) return;
    _decoding = true;
    // Decode the pixels directly in their native layout (no channel swap): the
    // decoder consumes BGRA as-is.
    final format = frame.pixelFormat == PixelFormat.rgba8888
        ? ui.PixelFormat.rgba8888
        : ui.PixelFormat.bgra8888;
    ui.decodeImageFromPixels(frame.bytes, frame.width, frame.height, format, (
      image,
    ) {
      _decoding = false;
      if (!mounted || !_streaming) {
        image.dispose();
        return;
      }
      setState(() {
        _preview?.dispose();
        _preview = image;
      });
    });
  }

  Future<void> _stop() async {
    _streaming = false;
    _decoding = false;
    await _sub?.cancel();
    _sub = null;
    await _camera?.close();
    _camera = null;
    _preview?.dispose();
    _preview = null;
    if (mounted) setState(() {});
  }

  @override
  void dispose() {
    _lifecycle.dispose();
    _stop();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text('camera_flutter example'),
        actions: [
          IconButton(onPressed: _load, icon: const Icon(Icons.refresh)),
        ],
      ),
      body: Column(
        children: [
          Padding(
            padding: const EdgeInsets.all(12),
            child: Text('Backend: $_backend'),
          ),
          if (_error != null)
            Padding(
              padding: const EdgeInsets.all(12),
              child: Text(_error!, style: const TextStyle(color: Colors.red)),
            ),
          Expanded(
            child: _preview != null
                ? Center(
                    child: FittedBox(
                      fit: BoxFit.contain,
                      child: RawImage(image: _preview),
                    ),
                  )
                : ListView(
                    children: [
                      for (final d in _devices)
                        ListTile(
                          title: Text(d.name),
                          subtitle: Text(
                            '${d.id}\n${d.formats.length} formats, '
                            '${d.facing.name}',
                          ),
                          isThreeLine: true,
                          onTap: () => _start(d),
                        ),
                      if (_devices.isEmpty)
                        const ListTile(
                          title: Text('No cameras found'),
                          subtitle: Text('Grant camera access, then refresh.'),
                        ),
                    ],
                  ),
          ),
          if (_camera != null)
            Padding(
              padding: const EdgeInsets.all(12),
              child: FilledButton.icon(
                onPressed: _stop,
                icon: const Icon(Icons.stop),
                label: const Text('Stop'),
              ),
            ),
        ],
      ),
    );
  }
}
