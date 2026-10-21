//! C ABI for the camera_cli crate.
//!
//! This is the surface the Dart FFI backend binds to. Keep it small and
//! stable: integer handles, JSON for device lists, no Rust types across the
//! boundary.
//!
//! Frames are pushed, not polled: the caller registers a `FrameCallback` with
//! `camera_cli_start_stream`, the platform capture delivers each frame to
//! that callback (on the platform's capture thread), and the caller frees each
//! frame buffer with `camera_cli_frame_free`. This avoids any blocking call
//! across the FFI boundary.

// The exported functions take raw pointers the caller is responsible for; the
// derefs here are expected at an FFI boundary rather than a smell.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, MutexGuard};

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod avfoundation;

#[cfg(target_os = "windows")]
mod media_foundation;

#[cfg(target_os = "android")]
mod android;

#[cfg(target_os = "linux")]
mod linux;

mod device;
// `Format` is unused on Windows, which does not list formats yet (frames still
// carry their own dimensions).
#[allow(unused_imports)]
pub(crate) use device::{Device, Format, PixelFormat};

/// A frame produced by a platform capture session.
pub(crate) struct Frame {
    pub data: Vec<u8>,
    pub width: i32,
    pub height: i32,
    pub pixel_format: PixelFormat,
    pub timestamp_us: i64,
}

/// Callback invoked once per captured frame.
///
/// `data` points at a heap buffer of `length` bytes that the callee now owns
/// and must free with `camera_cli_frame_free(data, length)`. Called on the
/// platform capture thread, so it must be cheap and thread-safe. It must not
/// call `camera_cli_stop_stream`, `camera_cli_close` or `camera_cli_close_all`:
/// those wait for the capture thread, which is the one running the callback.
///
/// If capture ends without a stop request (the device went away, or capture
/// could not start after `camera_cli_start_stream` returned), the callback is
/// invoked once more with null `data`, zero length/size and `pixel_format`
/// [`STREAM_ENDED`]; no frame follows until the stream is started again.
pub type FrameCallback = extern "C" fn(
    data: *mut u8,
    length: i32,
    width: i32,
    height: i32,
    pixel_format: i32,
    timestamp_us: i64,
);

/// `pixel_format` of the callback that reports the stream ending on its own.
pub const STREAM_ENDED: i32 = -1;

/// A wrapper making a `FrameCallback` Send so it can move to the capture thread.
#[derive(Clone, Copy)]
pub(crate) struct FrameSink(pub FrameCallback);

// SAFETY: a plain C function pointer is safe to send/share across threads.
unsafe impl Send for FrameSink {}
unsafe impl Sync for FrameSink {}

impl FrameSink {
    /// Hand a frame to the callback, transferring ownership of the buffer.
    pub fn emit(&self, frame: Frame) {
        let mut data = frame.data.into_boxed_slice();
        let ptr = data.as_mut_ptr();
        let len = data.len() as i32;
        std::mem::forget(data);
        (self.0)(
            ptr,
            len,
            frame.width,
            frame.height,
            frame.pixel_format as i32,
            frame.timestamp_us,
        );
    }

    /// Report that capture ended without a stop request. Call at most once per
    /// start, from the thread that would otherwise have emitted the next frame.
    #[allow(dead_code)] // Not every backend can detect a lost device yet.
    pub fn end(&self) {
        (self.0)(std::ptr::null_mut(), 0, 0, 0, STREAM_ENDED, 0);
    }
}

/// Trait every platform capture session implements.
pub(crate) trait FrameSource: Send {
    /// Start delivering frames to `sink`. Idempotent: replaces any prior sink.
    fn start(&mut self, sink: FrameSink);
    /// Stop delivering frames.
    fn stop(&mut self);
}

/// Active capture sessions, keyed by handle.
static SESSIONS: Mutex<Option<HashMap<i32, Box<dyn FrameSource>>>> = Mutex::new(None);

/// Next handle to hand out. Positive on success, <= 0 reserved for errors.
static NEXT_HANDLE: AtomicI32 = AtomicI32::new(1);

/// Lock the session table, tolerating a poisoned mutex rather than panicking
/// across the FFI boundary.
fn sessions() -> MutexGuard<'static, Option<HashMap<i32, Box<dyn FrameSource>>>> {
    SESSIONS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Run `f`, returning `err` if it panics (a panic across `extern "C"` is UB).
fn guard<T>(err: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(err)
}

/// Enumerate cameras and their formats, as a JSON array string.
///
/// Caller owns the returned pointer and must free it with
/// `camera_cli_string_free`. Returns null on failure.
#[unsafe(no_mangle)]
pub extern "C" fn camera_cli_enumerate() -> *mut c_char {
    guard(std::ptr::null_mut(), || {
        let devices = platform_enumerate();
        let json = device::devices_to_json(&devices);
        match CString::new(json) {
            Ok(s) => s.into_raw(),
            Err(_) => std::ptr::null_mut(),
        }
    })
}

/// Free a string returned by this library.
#[unsafe(no_mangle)]
pub extern "C" fn camera_cli_string_free(s: *mut c_char) {
    if s.is_null() {
        return;
    }
    guard((), || unsafe {
        drop(CString::from_raw(s));
    });
}

/// Open a device by id. Returns a handle > 0, or <= 0 on error.
#[unsafe(no_mangle)]
pub extern "C" fn camera_cli_open(device_id: *const c_char) -> c_int {
    guard(-100, || {
        let id = unsafe {
            if device_id.is_null() {
                return -1;
            }
            match CStr::from_ptr(device_id).to_str() {
                Ok(s) => s.to_owned(),
                Err(_) => return -2,
            }
        };
        let source = match platform_open(&id) {
            Ok(s) => s,
            Err(_) => return -3,
        };
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::SeqCst);
        sessions()
            .get_or_insert_with(HashMap::new)
            .insert(handle, source);
        handle
    })
}

/// Start streaming frames from `handle` to `callback`. Returns 0 on success,
/// < 0 on error. The callback is invoked on the capture thread until
/// `camera_cli_stop_stream` or `camera_cli_close`.
#[unsafe(no_mangle)]
pub extern "C" fn camera_cli_start_stream(handle: c_int, callback: FrameCallback) -> c_int {
    guard(-100, || {
        let mut guard = sessions();
        let map = match guard.as_mut() {
            Some(m) => m,
            None => return -2,
        };
        match map.get_mut(&handle) {
            Some(source) => {
                source.start(FrameSink(callback));
                0
            }
            None => -3,
        }
    })
}

/// Stop streaming frames from `handle`. Returns 0 on success, < 0 on error.
#[unsafe(no_mangle)]
pub extern "C" fn camera_cli_stop_stream(handle: c_int) -> c_int {
    guard(-100, || {
        let mut guard = sessions();
        let map = match guard.as_mut() {
            Some(m) => m,
            None => return -2,
        };
        match map.get_mut(&handle) {
            Some(source) => {
                source.stop();
                0
            }
            None => -3,
        }
    })
}

/// Free a frame buffer handed to a `FrameCallback`.
#[unsafe(no_mangle)]
pub extern "C" fn camera_cli_frame_free(data: *mut u8, length: i32) {
    if data.is_null() || length <= 0 {
        return;
    }
    guard((), || unsafe {
        let len = length as usize;
        drop(Vec::from_raw_parts(data, len, len));
    });
}

/// Close a session and release its resources.
#[unsafe(no_mangle)]
pub extern "C" fn camera_cli_close(handle: c_int) {
    guard((), || {
        // Remove under the lock, then stop outside it so a slow stop never holds
        // the table lock.
        let source = sessions().as_mut().and_then(|m| m.remove(&handle));
        if let Some(mut source) = source {
            source.stop();
        }
    });
}

/// Stop and release every open session.
///
/// Synchronous: after this returns no frame callback can be invoked again. Call
/// it from an app-termination hook that runs before the Dart VM is destroyed
/// (e.g. macOS applicationWillTerminate), so the capture thread cannot invoke a
/// deleted callback during shutdown.
#[unsafe(no_mangle)]
pub extern "C" fn camera_cli_close_all() {
    guard((), || {
        // Take the whole table out under the lock, then stop each outside it.
        let map = sessions().take();
        if let Some(mut map) = map {
            for (_handle, mut source) in map.drain() {
                source.stop();
            }
        }
    });
}

// Platform dispatch. Each host compiles in its own implementation; hosts with
// no implementation yet fall back to an empty device list and open errors.

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn platform_enumerate() -> Vec<Device> {
    avfoundation::enumerate()
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn platform_open(id: &str) -> Result<Box<dyn FrameSource>, ()> {
    avfoundation::open(id)
}

#[cfg(target_os = "windows")]
fn platform_enumerate() -> Vec<Device> {
    media_foundation::enumerate()
}

#[cfg(target_os = "windows")]
fn platform_open(id: &str) -> Result<Box<dyn FrameSource>, ()> {
    media_foundation::open(id)
}

#[cfg(target_os = "android")]
fn platform_enumerate() -> Vec<Device> {
    android::enumerate()
}

#[cfg(target_os = "android")]
fn platform_open(id: &str) -> Result<Box<dyn FrameSource>, ()> {
    android::open(id)
}

#[cfg(target_os = "linux")]
fn platform_enumerate() -> Vec<Device> {
    linux::enumerate()
}

#[cfg(target_os = "linux")]
fn platform_open(id: &str) -> Result<Box<dyn FrameSource>, ()> {
    linux::open(id)
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "windows",
    target_os = "android",
    target_os = "linux"
)))]
fn platform_enumerate() -> Vec<Device> {
    Vec::new()
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "windows",
    target_os = "android",
    target_os = "linux"
)))]
fn platform_open(_id: &str) -> Result<Box<dyn FrameSource>, ()> {
    Err(())
}
