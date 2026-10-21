# Changelog

## 0.0.2

- Include native camera sources in the Flutter package and update native build discovery.
- Verify isolated publication archives and fresh consumers before release.

## 0.0.1

- Initial release. Pure-Dart camera access with a pluggable backend registry.
- Public API: enumerate devices, open a camera, capture frames.
- Backends: native (FFI), web (getUserMedia), stub fallback.
