//! Media Foundation capture backend for Windows.
//!
//! Uses the `windows` crate, which binds to the Media Foundation system DLLs
//! (mfplat/mfreadwrite/ole32) present on every Windows install -- no system
//! dependency for consumers to install.
//!
//! Enumeration is via `MFEnumDeviceSources`. Capture runs an `IMFSourceReader`
//! synchronous read loop on a dedicated thread, converting whatever the camera
//! delivers to 32-bit RGB (BGRA in memory) via the reader's built-in video
//! processor, and pushes each frame to the registered `FrameSink`. All COM work
//! happens on threads this module owns so the apartment model is controlled.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};
use std::thread::JoinHandle;

use windows::core::{Result, PCWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaSource, IMFSourceReader, MFCreateAttributes, MFCreateDeviceSource,
    MFCreateMediaType, MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFMediaType_Video,
    MFStartup, MFVideoFormat_RGB32, MFSTARTUP_FULL, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED,
};

use crate::{Device, Frame, FrameSink, FrameSource, PixelFormat};

/// MF_SOURCE_READER_FIRST_VIDEO_STREAM: read from the first video stream.
const FIRST_VIDEO_STREAM: u32 = 0xFFFF_FFFC;
/// MF_SOURCE_READERF_ENDOFSTREAM: the read returned because the stream ended.
const READERF_ENDOFSTREAM: u32 = 0x0002;
/// MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED: the output media type changed.
const READERF_CURRENTMEDIATYPECHANGED: u32 = 0x0020;

/// Start Media Foundation once for the whole process. MFStartup is global and
/// ref-counted; we never call MFShutdown -- the library lives for the process.
fn ensure_startup() {
    static START: Once = Once::new();
    START.call_once(|| unsafe {
        let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    });
}

/// Run `f` on a fresh thread inside a multithreaded COM apartment.
///
/// Enumeration and capture both need COM, and the calling (Dart isolate) thread
/// may already sit in an apartment we do not control. Owning the thread lets us
/// pick the MTA and tear COM down cleanly.
fn on_com_thread<T, F>(f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    std::thread::spawn(move || {
        unsafe {
            // S_FALSE (already initialized) is fine; only a hard failure matters,
            // and even then MF objects are free-threaded.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        ensure_startup();
        let out = f();
        unsafe { CoUninitialize() };
        out
    })
    .join()
    .ok()
}

/// List the video capture devices the host exposes.
pub fn enumerate() -> Vec<Device> {
    on_com_thread(|| enumerate_inner().unwrap_or_default()).unwrap_or_default()
}

fn enumerate_inner() -> Result<Vec<Device>> {
    let mut out = Vec::new();
    unsafe {
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 1)?;
        let attributes = attributes.unwrap();
        attributes.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )?;

        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        MFEnumDeviceSources(&attributes, &mut activates, &mut count)?;

        for i in 0..count as usize {
            // Take ownership of each activate so it is released on drop.
            let activate = std::ptr::read(activates.add(i));
            if let Some(activate) = activate {
                let name = get_attr_string(&activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)
                    .unwrap_or_else(|| "Camera".to_string());
                // The symbolic link is a stable per-device id we can re-open.
                let id = get_attr_string(
                    &activate,
                    &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
                );
                if let Some(id) = id {
                    out.push(Device {
                        id,
                        name,
                        // Media Foundation does not surface a facing for USB/
                        // built-in webcams; report external.
                        facing: "external".to_string(),
                        // Formats are not probed here: that would
                        // require activating (powering on) every camera just to
                        // list them. Capture negotiates RGB32 at open time and
                        // each Frame carries its own dimensions.
                        formats: Vec::new(),
                    });
                }
            }
        }
        if !activates.is_null() {
            CoTaskMemFree(Some(activates as *const _));
        }
    }
    Ok(out)
}

/// Read a string device attribute, returning None if it is absent.
unsafe fn get_attr_string(
    activate: &IMFActivate,
    key: &windows::core::GUID,
) -> Option<String> {
    let mut ptr = windows::core::PWSTR::null();
    let mut len = 0u32;
    activate.GetAllocatedString(key, &mut ptr, &mut len).ok()?;
    if ptr.is_null() {
        return None;
    }
    let s = ptr.to_string().ok();
    CoTaskMemFree(Some(ptr.0 as *const _));
    s
}

/// Open a device by its symbolic-link id.
///
/// Validates the id against the live device list; the capture pipeline itself is
/// built on the worker thread when streaming starts (so no COM object has to
/// cross threads).
pub fn open(id: &str) -> std::result::Result<Box<dyn FrameSource>, ()> {
    let exists = enumerate().iter().any(|d| d.id == id);
    if !exists {
        return Err(());
    }
    Ok(Box::new(MfSession {
        symbolic_link: id.to_string(),
        stop: Arc::new(AtomicBool::new(false)),
        thread: None,
    }))
}

/// A capture session: a worker thread running an IMFSourceReader read loop.
struct MfSession {
    symbolic_link: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MfSession {
    /// Stop the worker thread and join it, so no emit is in flight afterwards.
    fn stop_thread(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl FrameSource for MfSession {
    fn start(&mut self, sink: FrameSink) {
        // Replace any prior stream.
        self.stop_thread();
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = stop.clone();
        let link = self.symbolic_link.clone();
        self.thread = Some(std::thread::spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            ensure_startup();
            // Errors here just mean no frames are delivered; the consumer sees an
            // empty stream rather than a crash.
            let _ = capture_loop(&link, sink, &stop);
            unsafe { CoUninitialize() };
        }));
    }

    fn stop(&mut self) {
        self.stop_thread();
    }
}

impl Drop for MfSession {
    fn drop(&mut self) {
        self.stop_thread();
    }
}

/// Build the source reader for `link` and pump frames until `stop` is set.
fn capture_loop(link: &str, sink: FrameSink, stop: &AtomicBool) -> Result<()> {
    unsafe {
        let source = create_source(link)?;
        let reader = create_reader(&source)?;

        // Request 32-bit RGB (BGRA byte order). The reader inserts a video
        // processor to convert from the camera's native format as needed.
        let media_type = MFCreateMediaType()?;
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        reader.SetCurrentMediaType(FIRST_VIDEO_STREAM, None, &media_type)?;
        let (mut width, mut height, mut stride) = current_geometry(&reader)?;

        loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let mut stream_flags: u32 = 0;
            let mut timestamp: i64 = 0;
            let mut sample = None;
            reader.ReadSample(
                FIRST_VIDEO_STREAM,
                0,
                None,
                Some(&mut stream_flags),
                Some(&mut timestamp),
                Some(&mut sample),
            )?;

            if stream_flags & READERF_ENDOFSTREAM != 0 {
                break;
            }
            // The output type was renegotiated (e.g. the camera or its decoder
            // changed resolution); re-read it before handling this sample.
            if stream_flags & READERF_CURRENTMEDIATYPECHANGED != 0 {
                (width, height, stride) = current_geometry(&reader)?;
            }
            let Some(sample) = sample else { continue };
            if let Some(frame) = extract_frame(&sample, width, height, stride, timestamp) {
                sink.emit(frame);
            }
        }
    }
    Ok(())
}

/// Width, height and signed default stride of the reader's current output type.
unsafe fn current_geometry(reader: &IMFSourceReader) -> Result<(u32, u32, i32)> {
    let current = reader.GetCurrentMediaType(FIRST_VIDEO_STREAM)?;
    let frame_size = current.GetUINT64(&MF_MT_FRAME_SIZE)?;
    let width = (frame_size >> 32) as u32;
    let height = (frame_size & 0xFFFF_FFFF) as u32;
    // A negative stride means the rows are stored bottom-up; we flip them so
    // the delivered buffer is always top-down BGRA.
    let stride = current
        .GetUINT32(&MF_MT_DEFAULT_STRIDE)
        .map(|s| s as i32)
        .unwrap_or((width as i32) * 4);
    Ok((width, height, stride))
}

/// Create the IMFMediaSource for a device symbolic link.
unsafe fn create_source(link: &str) -> Result<IMFMediaSource> {
    let mut attributes = None;
    MFCreateAttributes(&mut attributes, 2)?;
    let attributes = attributes.unwrap();
    attributes.SetGUID(
        &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
        &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    )?;
    let wide: Vec<u16> = link.encode_utf16().chain(std::iter::once(0)).collect();
    attributes.SetString(
        &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
        PCWSTR(wide.as_ptr()),
    )?;
    MFCreateDeviceSource(&attributes)
}

/// Create the source reader with video processing (format conversion) enabled.
unsafe fn create_reader(source: &IMFMediaSource) -> Result<IMFSourceReader> {
    let mut attributes = None;
    MFCreateAttributes(&mut attributes, 1)?;
    let attributes = attributes.unwrap();
    attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
    MFCreateSourceReaderFromMediaSource(source, &attributes)
}

/// Copy a frame out of a sample into a tightly-packed top-down BGRA buffer.
unsafe fn extract_frame(
    sample: &windows::Win32::Media::MediaFoundation::IMFSample,
    width: u32,
    height: u32,
    stride: i32,
    timestamp_100ns: i64,
) -> Option<Frame> {
    let buffer = sample.ConvertToContiguousBuffer().ok()?;
    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut max_len: u32 = 0;
    let mut cur_len: u32 = 0;
    buffer
        .Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len))
        .ok()?;

    let w = width as usize;
    let h = height as usize;
    let row_len = w * 4;
    let src_stride = stride.unsigned_abs() as usize;
    let bottom_up = stride < 0;

    // The last row only needs `row_len` bytes, not a whole stride. A stride
    // shorter than a row or a short buffer means the geometry does not match
    // this sample, so drop it rather than emit a blank frame.
    let need = src_stride * h.saturating_sub(1) + row_len;
    if ptr.is_null() || row_len == 0 || h == 0 || src_stride < row_len || (cur_len as usize) < need
    {
        let _ = buffer.Unlock();
        return None;
    }

    let mut data = vec![0u8; row_len * h];
    for row in 0..h {
        let src_row = if bottom_up { h - 1 - row } else { row };
        let src = ptr.add(src_row * src_stride);
        std::ptr::copy_nonoverlapping(src, data.as_mut_ptr().add(row * row_len), row_len);
    }
    let _ = buffer.Unlock();

    // MFVideoFormat_RGB32 is BGRX: the 4th byte is unused padding (X), delivered
    // as 0. Decoded as BGRA that reads as a fully transparent pixel, so the
    // preview shows through to the background. Force every pixel opaque.
    for px in data.chunks_exact_mut(4) {
        px[3] = 255;
    }

    Some(Frame {
        data,
        width: width as i32,
        height: height as i32,
        pixel_format: PixelFormat::Bgra8888,
        // ReadSample timestamps are in 100-ns units.
        timestamp_us: timestamp_100ns / 10,
    })
}
