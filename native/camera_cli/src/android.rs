//! Camera2 capture backend for Android, via the NDK Camera2 + ImageReader APIs.
//!
//! Links against libcamera2ndk / libmediandk, which ship with Android (API 24+,
//! our minSdk) -- no system dependency for consumers to install. We declare the
//! handful of NDK entry points we use directly rather than pulling in a bindings
//! crate, mirroring how the macOS backend talks to AVFoundation.
//!
//! enumerate() lists camera ids. open() opens the device, wires an
//! AImageReader (YUV_420_888) to a repeating preview request, and on each
//! delivered image converts YUV -> BGRA and pushes it to the FrameSink.
//!
//! Note: capture requires the CAMERA runtime permission, which can only be
//! granted from the Java/Kotlin layer (the NDK has no permission-request API).
//! ACameraManager_openCamera fails until the host app has been granted it.

#![allow(non_camel_case_types, non_upper_case_globals)]

use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::{Device, Frame, FrameSink, FrameSource, PixelFormat};

// --- Opaque NDK handle types -------------------------------------------------

#[repr(C)]
struct ACameraManager {
    _private: [u8; 0],
}
#[repr(C)]
struct ACameraDevice {
    _private: [u8; 0],
}
#[repr(C)]
struct ACameraMetadata {
    _private: [u8; 0],
}
#[repr(C)]
struct ACameraCaptureSession {
    _private: [u8; 0],
}
#[repr(C)]
struct ACaptureRequest {
    _private: [u8; 0],
}
#[repr(C)]
struct ACameraOutputTarget {
    _private: [u8; 0],
}
#[repr(C)]
struct ACaptureSessionOutput {
    _private: [u8; 0],
}
#[repr(C)]
struct ACaptureSessionOutputContainer {
    _private: [u8; 0],
}
#[repr(C)]
struct AImageReader {
    _private: [u8; 0],
}
#[repr(C)]
struct AImage {
    _private: [u8; 0],
}
#[repr(C)]
struct ANativeWindow {
    _private: [u8; 0],
}

#[repr(C)]
struct ACameraIdList {
    num_cameras: c_int,
    camera_ids: *mut *const c_char,
}

// camera_status_t / media_status_t: 0 == OK.
type camera_status_t = c_int;
type media_status_t = c_int;

const ACAMERA_OK: camera_status_t = 0;
const AMEDIA_OK: media_status_t = 0;

const AIMAGE_FORMAT_YUV_420_888: c_int = 0x23;
const TEMPLATE_PREVIEW: c_int = 1;

// --- Callback structs --------------------------------------------------------

#[repr(C)]
struct ACameraDevice_StateCallbacks {
    context: *mut c_void,
    on_disconnected: extern "C" fn(*mut c_void, *mut ACameraDevice),
    on_error: extern "C" fn(*mut c_void, *mut ACameraDevice, c_int),
}

#[repr(C)]
struct ACameraCaptureSession_stateCallbacks {
    context: *mut c_void,
    on_closed: extern "C" fn(*mut c_void, *mut ACameraCaptureSession),
    on_ready: extern "C" fn(*mut c_void, *mut ACameraCaptureSession),
    on_active: extern "C" fn(*mut c_void, *mut ACameraCaptureSession),
}

#[repr(C)]
struct AImageReader_ImageListener {
    context: *mut c_void,
    on_image_available: extern "C" fn(*mut c_void, *mut AImageReader),
}

extern "C" fn on_disconnected(_ctx: *mut c_void, _dev: *mut ACameraDevice) {}
extern "C" fn on_error(_ctx: *mut c_void, _dev: *mut ACameraDevice, _err: c_int) {}
extern "C" fn on_session_noop(_ctx: *mut c_void, _s: *mut ACameraCaptureSession) {}

// --- NDK entry points --------------------------------------------------------

#[link(name = "camera2ndk")]
extern "C" {
    fn ACameraManager_create() -> *mut ACameraManager;
    fn ACameraManager_delete(manager: *mut ACameraManager);
    fn ACameraManager_getCameraIdList(
        manager: *mut ACameraManager,
        list: *mut *mut ACameraIdList,
    ) -> camera_status_t;
    fn ACameraManager_deleteCameraIdList(list: *mut ACameraIdList);
    fn ACameraManager_getCameraCharacteristics(
        manager: *mut ACameraManager,
        camera_id: *const c_char,
        characteristics: *mut *mut ACameraMetadata,
    ) -> camera_status_t;
    fn ACameraManager_openCamera(
        manager: *mut ACameraManager,
        camera_id: *const c_char,
        callbacks: *mut ACameraDevice_StateCallbacks,
        device: *mut *mut ACameraDevice,
    ) -> camera_status_t;
    fn ACameraMetadata_free(metadata: *mut ACameraMetadata);
    fn ACameraMetadata_getConstEntry(
        metadata: *const ACameraMetadata,
        tag: u32,
        entry: *mut ACameraMetadata_const_entry,
    ) -> camera_status_t;

    fn ACameraDevice_close(device: *mut ACameraDevice) -> camera_status_t;
    fn ACameraDevice_createCaptureRequest(
        device: *mut ACameraDevice,
        template_id: c_int,
        request: *mut *mut ACaptureRequest,
    ) -> camera_status_t;
    fn ACameraDevice_createCaptureSession(
        device: *mut ACameraDevice,
        outputs: *const ACaptureSessionOutputContainer,
        callbacks: *const ACameraCaptureSession_stateCallbacks,
        session: *mut *mut ACameraCaptureSession,
    ) -> camera_status_t;

    fn ACaptureRequest_free(request: *mut ACaptureRequest);
    fn ACaptureRequest_addTarget(
        request: *mut ACaptureRequest,
        target: *const ACameraOutputTarget,
    ) -> camera_status_t;

    fn ACameraOutputTarget_create(
        window: *mut ANativeWindow,
        target: *mut *mut ACameraOutputTarget,
    ) -> camera_status_t;
    fn ACameraOutputTarget_free(target: *mut ACameraOutputTarget);

    fn ACaptureSessionOutput_create(
        window: *mut ANativeWindow,
        output: *mut *mut ACaptureSessionOutput,
    ) -> camera_status_t;
    fn ACaptureSessionOutput_free(output: *mut ACaptureSessionOutput);

    fn ACaptureSessionOutputContainer_create(
        container: *mut *mut ACaptureSessionOutputContainer,
    ) -> camera_status_t;
    fn ACaptureSessionOutputContainer_free(container: *mut ACaptureSessionOutputContainer);
    fn ACaptureSessionOutputContainer_add(
        container: *mut ACaptureSessionOutputContainer,
        output: *const ACaptureSessionOutput,
    ) -> camera_status_t;

    fn ACameraCaptureSession_setRepeatingRequest(
        session: *mut ACameraCaptureSession,
        callbacks: *mut c_void,
        num_requests: c_int,
        requests: *mut *mut ACaptureRequest,
        capture_sequence_id: *mut c_int,
    ) -> camera_status_t;
    fn ACameraCaptureSession_stopRepeating(session: *mut ACameraCaptureSession) -> camera_status_t;
    fn ACameraCaptureSession_close(session: *mut ACameraCaptureSession);
}

#[link(name = "mediandk")]
extern "C" {
    fn AImageReader_new(
        width: c_int,
        height: c_int,
        format: c_int,
        max_images: c_int,
        reader: *mut *mut AImageReader,
    ) -> media_status_t;
    fn AImageReader_delete(reader: *mut AImageReader);
    fn AImageReader_getWindow(
        reader: *mut AImageReader,
        window: *mut *mut ANativeWindow,
    ) -> media_status_t;
    fn AImageReader_setImageListener(
        reader: *mut AImageReader,
        listener: *mut AImageReader_ImageListener,
    ) -> media_status_t;
    fn AImageReader_acquireLatestImage(
        reader: *mut AImageReader,
        image: *mut *mut AImage,
    ) -> media_status_t;

    fn AImage_delete(image: *mut AImage);
    fn AImage_getWidth(image: *const AImage, width: *mut c_int) -> media_status_t;
    fn AImage_getHeight(image: *const AImage, height: *mut c_int) -> media_status_t;
    fn AImage_getTimestamp(image: *const AImage, timestamp_ns: *mut i64) -> media_status_t;
    fn AImage_getNumberOfPlanes(image: *const AImage, num_planes: *mut c_int) -> media_status_t;
    fn AImage_getPlaneData(
        image: *const AImage,
        plane_idx: c_int,
        data: *mut *mut u8,
        data_length: *mut c_int,
    ) -> media_status_t;
    fn AImage_getPlanePixelStride(
        image: *const AImage,
        plane_idx: c_int,
        pixel_stride: *mut c_int,
    ) -> media_status_t;
    fn AImage_getPlaneRowStride(
        image: *const AImage,
        plane_idx: c_int,
        row_stride: *mut c_int,
    ) -> media_status_t;
}

// ACameraMetadata_const_entry layout. The data field is a union of pointers; we
// only read u8 / i32 entries here, so a pointer-sized payload is sufficient.
#[repr(C)]
struct ACameraMetadata_const_entry {
    tag: u32,
    type_: u8,
    count: u32,
    data: *const c_void,
}

// ACAMERA_LENS_FACING = (ACAMERA_SECTION_LENS << 16) + 5, SECTION_LENS = 8.
const ACAMERA_LENS_FACING: u32 = (8u32 << 16) + 5;
const ACAMERA_LENS_FACING_FRONT: u8 = 0;
const ACAMERA_LENS_FACING_BACK: u8 = 1;

// Preview/capture resolution. 640x480 YUV_420_888 is universally supported
// (including the emulator's virtual camera); negotiating the device's best size
// is a future refinement.
const CAPTURE_WIDTH: c_int = 640;
const CAPTURE_HEIGHT: c_int = 480;

/// List the cameras the device exposes.
pub fn enumerate() -> Vec<Device> {
    unsafe {
        let manager = ACameraManager_create();
        if manager.is_null() {
            return Vec::new();
        }
        let mut list: *mut ACameraIdList = std::ptr::null_mut();
        if ACameraManager_getCameraIdList(manager, &mut list) != ACAMERA_OK || list.is_null() {
            ACameraManager_delete(manager);
            return Vec::new();
        }
        let mut out = Vec::new();
        let count = (*list).num_cameras.max(0) as usize;
        for i in 0..count {
            let id_ptr = *(*list).camera_ids.add(i);
            if id_ptr.is_null() {
                continue;
            }
            let id = CStr::from_ptr(id_ptr).to_string_lossy().into_owned();
            let facing = lens_facing(manager, id_ptr);
            out.push(Device {
                id: id.clone(),
                name: format!("Camera {id}"),
                facing,
                // Frame dimensions are carried per-frame; we deliver BGRA at the
                // fixed capture size.
                formats: Vec::new(),
            });
        }
        ACameraManager_deleteCameraIdList(list);
        ACameraManager_delete(manager);
        out
    }
}

/// Read a camera's lens facing, defaulting to "external" if unavailable.
unsafe fn lens_facing(manager: *mut ACameraManager, id: *const c_char) -> String {
    let mut meta: *mut ACameraMetadata = std::ptr::null_mut();
    if ACameraManager_getCameraCharacteristics(manager, id, &mut meta) != ACAMERA_OK
        || meta.is_null()
    {
        return "external".to_string();
    }
    let mut entry = ACameraMetadata_const_entry {
        tag: 0,
        type_: 0,
        count: 0,
        data: std::ptr::null(),
    };
    let facing = if ACameraMetadata_getConstEntry(meta, ACAMERA_LENS_FACING, &mut entry)
        == ACAMERA_OK
        && entry.count > 0
        && !entry.data.is_null()
    {
        match *(entry.data as *const u8) {
            ACAMERA_LENS_FACING_FRONT => "front",
            ACAMERA_LENS_FACING_BACK => "back",
            _ => "external",
        }
    } else {
        "external"
    }
    .to_string();
    ACameraMetadata_free(meta);
    facing
}

/// Shared frame sink. The image callback holds this lock for its whole body,
/// so taking it in stop()/drop() waits out any callback in progress.
type SharedSink = Arc<Mutex<Option<FrameSink>>>;

/// A running Camera2 capture session.
struct CameraSession {
    manager: *mut ACameraManager,
    device: *mut ACameraDevice,
    session: *mut ACameraCaptureSession,
    request: *mut ACaptureRequest,
    target: *mut ACameraOutputTarget,
    output: *mut ACaptureSessionOutput,
    container: *mut ACaptureSessionOutputContainer,
    reader: *mut AImageReader,
    // Boxed so the address handed to the NDK as the listener stays stable.
    listener: *mut AImageReader_ImageListener,
    // The listener's context (a boxed ListenerContext); reclaimed on drop.
    listener_context: *mut ListenerContext,
    sink: SharedSink,
    started: bool,
    // Set (under the sink lock) just before the reader is deleted.
    closing: Arc<AtomicBool>,
}

// SAFETY: the NDK handles are created on open() and only touched again on
// stop()/drop(), both serialized by the crate's session table. Frame delivery
// runs on the NDK's callback thread and only touches the thread-safe SharedSink.
unsafe impl Send for CameraSession {}

impl FrameSource for CameraSession {
    fn start(&mut self, sink: FrameSink) {
        *self.sink.lock().unwrap() = Some(sink);
        if self.started {
            return;
        }
        self.started = true;
        unsafe {
            let mut seq_id: c_int = 0;
            ACameraCaptureSession_setRepeatingRequest(
                self.session,
                std::ptr::null_mut(),
                1,
                &mut self.request,
                &mut seq_id,
            );
        }
    }

    fn stop(&mut self) {
        unsafe {
            if !self.session.is_null() {
                ACameraCaptureSession_stopRepeating(self.session);
            }
        }
        // Clear the sink under the lock: this waits for a callback that is
        // mid-emit, and every later callback sees None and drops its frame.
        *self.sink.lock().unwrap() = None;
        self.started = false;
    }
}

impl Drop for CameraSession {
    fn drop(&mut self) {
        self.stop();
        unsafe {
            if !self.session.is_null() {
                ACameraCaptureSession_close(self.session);
            }
            // Closing the device waits for in-flight captures, so no more
            // buffers are queued to the reader after this.
            if !self.device.is_null() {
                ACameraDevice_close(self.device);
            }
            if !self.manager.is_null() {
                ACameraManager_delete(self.manager);
            }
            if !self.request.is_null() {
                ACaptureRequest_free(self.request);
            }
            if !self.target.is_null() {
                ACameraOutputTarget_free(self.target);
            }
            if !self.container.is_null() {
                ACaptureSessionOutputContainer_free(self.container);
            }
            if !self.output.is_null() {
                ACaptureSessionOutput_free(self.output);
            }
            // AImageReader_delete holds the reader's lock while it joins the
            // callback thread, and acquiring or deleting an image needs that
            // lock too. Set `closing` under the sink lock (waiting out any
            // running callback) so callbacks still queued return without
            // touching the reader.
            {
                let _guard = self.sink.lock().unwrap();
                self.closing.store(true, Ordering::SeqCst);
            }
            // Once this returns the callback thread has exited, so the
            // listener and its context are safe to free.
            if !self.reader.is_null() {
                AImageReader_delete(self.reader);
            }
            if !self.listener.is_null() {
                drop(Box::from_raw(self.listener));
            }
            if !self.listener_context.is_null() {
                drop(Box::from_raw(self.listener_context));
            }
        }
    }
}

/// Context handed to the image listener: the shared sink plus the teardown flag.
struct ListenerContext {
    sink: SharedSink,
    closing: Arc<AtomicBool>,
}

extern "C" fn on_image_available(ctx: *mut c_void, reader: *mut AImageReader) {
    if ctx.is_null() {
        return;
    }
    // The context is a ListenerContext owned by the session; borrow it.
    let context = unsafe { &*(ctx as *const ListenerContext) };
    // Hold the lock for the whole callback so stop() and drop() never overlap
    // a callback that is emitting or using the reader.
    let guard = context.sink.lock().unwrap();
    if context.closing.load(Ordering::SeqCst) {
        // The reader is being deleted; acquiring from it now would deadlock.
        return;
    }
    unsafe {
        let mut image: *mut AImage = std::ptr::null_mut();
        if AImageReader_acquireLatestImage(reader, &mut image) != AMEDIA_OK || image.is_null() {
            return;
        }
        // While stopped (no sink) the image is released without converting.
        if let Some(sink) = guard.as_ref() {
            if let Some(frame) = image_to_bgra(image) {
                sink.emit(frame);
            }
        }
        AImage_delete(image);
    }
}

/// Convert a YUV_420_888 AImage into a tightly-packed top-down BGRA Frame.
unsafe fn image_to_bgra(image: *mut AImage) -> Option<Frame> {
    let mut width: c_int = 0;
    let mut height: c_int = 0;
    if AImage_getWidth(image, &mut width) != AMEDIA_OK
        || AImage_getHeight(image, &mut height) != AMEDIA_OK
        || width <= 0
        || height <= 0
    {
        return None;
    }
    let mut num_planes: c_int = 0;
    if AImage_getNumberOfPlanes(image, &mut num_planes) != AMEDIA_OK || num_planes < 3 {
        return None;
    }

    let y = plane(image, 0)?;
    let u = plane(image, 1)?;
    let v = plane(image, 2)?;

    let w = width as usize;
    let h = height as usize;
    let mut out = vec![0u8; w * h * 4];

    for row in 0..h {
        let y_row = row * y.row_stride;
        let uv_row = (row / 2) * u.row_stride;
        for col in 0..w {
            let yi = y_row + col * y.pixel_stride;
            let uvi = uv_row + (col / 2) * u.pixel_stride;
            let yy = *y.data.add(yi) as i32;
            let uu = *u.data.add(uvi) as i32 - 128;
            let vv = *v.data.add(uvi) as i32 - 128;

            let c = yy - 16;
            let r = (298 * c + 409 * vv + 128) >> 8;
            let g = (298 * c - 100 * uu - 208 * vv + 128) >> 8;
            let b = (298 * c + 516 * uu + 128) >> 8;

            let o = (row * w + col) * 4;
            // BGRA byte order to match PixelFormat::Bgra8888.
            *out.get_unchecked_mut(o) = clamp_u8(b);
            *out.get_unchecked_mut(o + 1) = clamp_u8(g);
            *out.get_unchecked_mut(o + 2) = clamp_u8(r);
            *out.get_unchecked_mut(o + 3) = 255;
        }
    }

    let mut ts_ns: i64 = 0;
    let _ = AImage_getTimestamp(image, &mut ts_ns);

    Some(Frame {
        data: out,
        width,
        height,
        pixel_format: PixelFormat::Bgra8888,
        timestamp_us: ts_ns / 1000,
    })
}

struct Plane {
    data: *const u8,
    pixel_stride: usize,
    row_stride: usize,
}

unsafe fn plane(image: *mut AImage, idx: c_int) -> Option<Plane> {
    let mut data: *mut u8 = std::ptr::null_mut();
    let mut len: c_int = 0;
    let mut pixel_stride: c_int = 0;
    let mut row_stride: c_int = 0;
    if AImage_getPlaneData(image, idx, &mut data, &mut len) != AMEDIA_OK
        || AImage_getPlanePixelStride(image, idx, &mut pixel_stride) != AMEDIA_OK
        || AImage_getPlaneRowStride(image, idx, &mut row_stride) != AMEDIA_OK
        || data.is_null()
    {
        return None;
    }
    Some(Plane {
        data,
        pixel_stride: pixel_stride.max(1) as usize,
        row_stride: row_stride.max(1) as usize,
    })
}

#[inline]
fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Open a camera by id and wire up the capture pipeline (idle until start()).
pub fn open(id: &str) -> Result<Box<dyn FrameSource>, ()> {
    let c_id = std::ffi::CString::new(id).map_err(|_| ())?;
    unsafe {
        let manager = ACameraManager_create();
        if manager.is_null() {
            return Err(());
        }

        // Build a guard that frees whatever we have allocated if we bail early.
        let mut sess = CameraSession {
            manager,
            device: std::ptr::null_mut(),
            session: std::ptr::null_mut(),
            request: std::ptr::null_mut(),
            target: std::ptr::null_mut(),
            output: std::ptr::null_mut(),
            container: std::ptr::null_mut(),
            reader: std::ptr::null_mut(),
            listener: std::ptr::null_mut(),
            listener_context: std::ptr::null_mut(),
            sink: Arc::new(Mutex::new(None)),
            started: false,
            closing: Arc::new(AtomicBool::new(false)),
        };

        let mut device_cbs = ACameraDevice_StateCallbacks {
            context: std::ptr::null_mut(),
            on_disconnected,
            on_error,
        };
        if ACameraManager_openCamera(manager, c_id.as_ptr(), &mut device_cbs, &mut sess.device)
            != ACAMERA_OK
            || sess.device.is_null()
        {
            return Err(());
        }

        // ImageReader for YUV frames.
        if AImageReader_new(
            CAPTURE_WIDTH,
            CAPTURE_HEIGHT,
            AIMAGE_FORMAT_YUV_420_888,
            4,
            &mut sess.reader,
        ) != AMEDIA_OK
            || sess.reader.is_null()
        {
            return Err(());
        }

        // Listener context (sink + teardown flag), reclaimed in Drop via
        // `listener_context` after the reader is deleted.
        sess.listener_context = Box::into_raw(Box::new(ListenerContext {
            sink: sess.sink.clone(),
            closing: sess.closing.clone(),
        }));
        sess.listener = Box::into_raw(Box::new(AImageReader_ImageListener {
            context: sess.listener_context as *mut c_void,
            on_image_available,
        }));
        if AImageReader_setImageListener(sess.reader, sess.listener) != AMEDIA_OK {
            return Err(());
        }

        let mut window: *mut ANativeWindow = std::ptr::null_mut();
        if AImageReader_getWindow(sess.reader, &mut window) != AMEDIA_OK || window.is_null() {
            return Err(());
        }

        // Session output container.
        if ACaptureSessionOutputContainer_create(&mut sess.container) != ACAMERA_OK
            || ACaptureSessionOutput_create(window, &mut sess.output) != ACAMERA_OK
            || ACaptureSessionOutputContainer_add(sess.container, sess.output) != ACAMERA_OK
        {
            return Err(());
        }

        let session_cbs = ACameraCaptureSession_stateCallbacks {
            context: std::ptr::null_mut(),
            on_closed: on_session_noop,
            on_ready: on_session_noop,
            on_active: on_session_noop,
        };
        if ACameraDevice_createCaptureSession(
            sess.device,
            sess.container,
            &session_cbs,
            &mut sess.session,
        ) != ACAMERA_OK
            || sess.session.is_null()
        {
            return Err(());
        }

        // Preview request targeting the reader window.
        if ACameraDevice_createCaptureRequest(sess.device, TEMPLATE_PREVIEW, &mut sess.request)
            != ACAMERA_OK
            || ACameraOutputTarget_create(window, &mut sess.target) != ACAMERA_OK
            || ACaptureRequest_addTarget(sess.request, sess.target) != ACAMERA_OK
        {
            return Err(());
        }

        Ok(Box::new(sess))
    }
}
