//! Open the first camera, grab a few frames, and write the last one to a PPM.
//!
//! Run with: cargo run --example capture
//!
//! A quick end-to-end smoke test of the capture path on the current host: it
//! exercises enumerate -> open -> start_stream -> frame callback -> close, and
//! drops a viewable image (BGRA reinterpreted as RGB) to /tmp so the result can
//! be eyeballed.

use std::ffi::CStr;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use camera_cli::{
    camera_cli_close, camera_cli_enumerate, camera_cli_frame_free, camera_cli_open,
    camera_cli_start_stream, camera_cli_stop_stream, camera_cli_string_free, STREAM_ENDED,
};

static FRAME_COUNT: AtomicI32 = AtomicI32::new(0);
static ENDED: AtomicBool = AtomicBool::new(false);
static LAST_FRAME: Mutex<Option<(Vec<u8>, i32, i32)>> = Mutex::new(None);

extern "C" fn on_frame(
    data: *mut u8,
    length: i32,
    width: i32,
    height: i32,
    pixel_format: i32,
    _timestamp_us: i64,
) {
    if data.is_null() && pixel_format == STREAM_ENDED {
        ENDED.store(true, Ordering::SeqCst);
        return;
    }
    FRAME_COUNT.fetch_add(1, Ordering::SeqCst);
    if !data.is_null() && length > 0 {
        let slice = unsafe { std::slice::from_raw_parts(data, length as usize) };
        *LAST_FRAME.lock().unwrap() = Some((slice.to_vec(), width, height));
    }
    camera_cli_frame_free(data, length);
}

fn first_device_id() -> Option<String> {
    let ptr = camera_cli_enumerate();
    if ptr.is_null() {
        return None;
    }
    let json = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    camera_cli_string_free(ptr);
    // Minimal extraction of the first "id" value; avoids a JSON dependency.
    let key = "\"id\":\"";
    let start = json.find(key)? + key.len();
    let end = json[start..].find('"')? + start;
    Some(json[start..end].to_string())
}

fn main() {
    let Some(id) = first_device_id() else {
        eprintln!("no camera found");
        return;
    };
    println!("opening {id}");
    let c_id = std::ffi::CString::new(id).unwrap();
    let handle = camera_cli_open(c_id.as_ptr());
    if handle <= 0 {
        eprintln!("open failed: {handle}");
        return;
    }

    let rc = camera_cli_start_stream(handle, on_frame);
    if rc != 0 {
        eprintln!("start_stream failed: {rc}");
        camera_cli_close(handle);
        return;
    }

    // Wait up to 5 s for a handful of frames.
    let deadline = Instant::now() + Duration::from_secs(5);
    while FRAME_COUNT.load(Ordering::SeqCst) < 5
        && !ENDED.load(Ordering::SeqCst)
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(50));
    }

    camera_cli_stop_stream(handle);
    camera_cli_close(handle);

    let count = FRAME_COUNT.load(Ordering::SeqCst);
    println!("received {count} frame(s)");
    if ENDED.load(Ordering::SeqCst) {
        eprintln!("capture ended early (device lost or failed to start)");
    }

    if let Some((bytes, w, h)) = LAST_FRAME.lock().unwrap().take() {
        println!("last frame: {w}x{h}, {} bytes", bytes.len());
        let nonzero = bytes.iter().filter(|&&b| b != 0).count();
        println!(
            "non-zero bytes: {nonzero}/{} ({:.0}%)",
            bytes.len(),
            100.0 * nonzero as f64 / bytes.len() as f64
        );
        write_ppm("/tmp/camera_cli_frame.ppm", &bytes, w, h);
    } else if count == 0 {
        eprintln!("no frames captured");
    }
}

/// Write a BGRA buffer as a binary PPM (RGB), for a quick visual check.
fn write_ppm(path: &str, bgra: &[u8], w: i32, h: i32) {
    let (w, h) = (w as usize, h as usize);
    if bgra.len() < w * h * 4 {
        return;
    }
    let mut rgb = Vec::with_capacity(w * h * 3);
    for px in bgra.chunks_exact(4) {
        rgb.push(px[2]); // R
        rgb.push(px[1]); // G
        rgb.push(px[0]); // B
    }
    let Ok(mut f) = std::fs::File::create(path) else {
        return;
    };
    let _ = write!(f, "P6\n{w} {h}\n255\n");
    let _ = f.write_all(&rgb);
    println!("wrote {path}");
}
