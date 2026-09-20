//! Device descriptors and the small JSON encoder used to hand them to Dart.
//!
//! We hand-roll JSON to keep the crate dependency-free. The shape matches the
//! Dart parser in native_backend_ffi.dart.

#![allow(dead_code)] // Format wire-contract fields are not all emitted yet.

/// Pixel layout of captured frames. Order must match the Dart `PixelFormat`.
///
/// Only `Bgra8888` is produced today (AVFoundation is configured to deliver
/// BGRA); the rest are part of the wire contract with the Dart side for the
/// other platforms/formats that land later.
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub enum PixelFormat {
    Rgba8888 = 0,
    Bgra8888 = 1,
    Yuyv422 = 2,
    Mjpeg = 3,
}

impl PixelFormat {
    fn as_str(self) -> &'static str {
        match self {
            PixelFormat::Rgba8888 => "rgba8888",
            PixelFormat::Bgra8888 => "bgra8888",
            PixelFormat::Yuyv422 => "yuyv422",
            PixelFormat::Mjpeg => "mjpeg",
        }
    }
}

/// A supported capture format.
pub struct Format {
    pub width: i32,
    pub height: i32,
    pub pixel_format: PixelFormat,
    pub frame_rate: f64,
}

/// A camera the host exposes.
pub struct Device {
    pub id: String,
    pub name: String,
    /// One of "front", "back", "external".
    pub facing: String,
    pub formats: Vec<Format>,
}

/// Encode devices as a JSON array string.
pub fn devices_to_json(devices: &[Device]) -> String {
    let mut out = String::from("[");
    for (i, d) in devices.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"id\":");
        push_json_string(&mut out, &d.id);
        out.push_str(",\"name\":");
        push_json_string(&mut out, &d.name);
        out.push_str(",\"facing\":");
        push_json_string(&mut out, &d.facing);
        out.push_str(",\"formats\":[");
        for (j, f) in d.formats.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            // NaN and infinity have no JSON spelling; report them as unknown (0).
            let frame_rate = if f.frame_rate.is_finite() {
                f.frame_rate
            } else {
                0.0
            };
            out.push_str(&format!(
                "{{\"width\":{},\"height\":{},\"pixel_format\":\"{}\",\"frame_rate\":{}}}",
                f.width,
                f.height,
                f.pixel_format.as_str(),
                frame_rate,
            ));
        }
        out.push_str("]}");
    }
    out.push(']');
    out
}

/// Append a JSON-escaped, quoted string.
fn push_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_empty() {
        assert_eq!(devices_to_json(&[]), "[]");
    }

    #[test]
    fn encodes_a_device_with_a_format() {
        let devices = vec![Device {
            id: "cam0".into(),
            name: "Test \"Cam\"".into(),
            facing: "front".into(),
            formats: vec![Format {
                width: 1280,
                height: 720,
                pixel_format: PixelFormat::Bgra8888,
                frame_rate: 30.0,
            }],
        }];
        let json = devices_to_json(&devices);
        assert!(json.contains("\"id\":\"cam0\""));
        assert!(json.contains("\"name\":\"Test \\\"Cam\\\"\""));
        assert!(json.contains("\"facing\":\"front\""));
        assert!(json.contains("\"pixel_format\":\"bgra8888\""));
        assert!(json.contains("\"width\":1280"));
    }

    #[test]
    fn encodes_a_non_finite_frame_rate_as_zero() {
        let devices = vec![Device {
            id: "cam0".into(),
            name: "Cam".into(),
            facing: "external".into(),
            formats: vec![Format {
                width: 640,
                height: 480,
                pixel_format: PixelFormat::Bgra8888,
                frame_rate: f64::NAN,
            }],
        }];
        assert!(devices_to_json(&devices).contains("\"frame_rate\":0}"));
    }
}
