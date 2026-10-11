//! AVFoundation capture backend for macOS and iOS.
//!
//! Uses the objc2 bindings (fetched by cargo, no system deps beyond the OS
//! frameworks that ship with every install). Enumeration is via
//! AVCaptureDeviceDiscoverySession; capture pushes each frame from an
//! AVCaptureVideoDataOutput delegate straight to the registered FrameSink.

use std::ffi::{c_char, c_void, CStr};
use std::sync::Mutex;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceDiscoverySession,
    AVCaptureDeviceInput, AVCaptureDevicePosition, AVCaptureDeviceType,
    AVCaptureDeviceTypeBuiltInWideAngleCamera, AVCaptureOutput, AVCaptureSession,
    AVCaptureVideoDataOutput, AVCaptureVideoDataOutputSampleBufferDelegate, AVMediaTypeVideo,
};
use objc2_core_media::{CMSampleBuffer, CMTimeFlags, CMVideoFormatDescriptionGetDimensions};
use objc2_core_video::{
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSObject, NSObjectProtocol, NSString};

use crate::{Device, Format, Frame, FrameSink, FrameSource, PixelFormat};

/// The device types we discover: built-in cameras plus external webcams.
fn device_types() -> Retained<NSArray<AVCaptureDeviceType>> {
    let mut types = vec![unsafe { AVCaptureDeviceTypeBuiltInWideAngleCamera }];
    types.extend(external_device_type());
    NSArray::from_slice(&types)
}

/// The device type for external (USB/UVC) cameras, if this OS has one.
///
/// AVCaptureDeviceTypeExternal is new in macOS 14 / iOS 17, and Rust cannot
/// weak-link an extern static: naming it directly makes dyld refuse to load
/// the library on anything older. Look it up at runtime instead. Older macOS
/// calls it AVCaptureDeviceTypeExternalUnknown (10.15+, deprecated in 14);
/// older iOS has no external camera type, so it is skipped there.
fn external_device_type() -> Option<&'static AVCaptureDeviceType> {
    let external = av_constant(c"AVCaptureDeviceTypeExternal");
    #[cfg(target_os = "macos")]
    let external = external.or_else(|| av_constant(c"AVCaptureDeviceTypeExternalUnknown"));
    external
}

/// Resolve an AVFoundation `NSString *const` constant by symbol name.
fn av_constant(name: &CStr) -> Option<&'static NSString> {
    extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }
    // RTLD_DEFAULT on Apple platforms: search every loaded image.
    const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;
    // SAFETY: dlsym returns the address of the exported pointer variable (or
    // null); the NSString it points at is a constant that lives forever.
    unsafe {
        let sym = dlsym(RTLD_DEFAULT, name.as_ptr()) as *const *const NSString;
        if sym.is_null() {
            return None;
        }
        (*sym).as_ref()
    }
}

/// Build a discovery session over all video device types and positions.
unsafe fn discovery_session() -> Retained<AVCaptureDeviceDiscoverySession> {
    AVCaptureDeviceDiscoverySession::discoverySessionWithDeviceTypes_mediaType_position(
        &device_types(),
        AVMediaTypeVideo,
        AVCaptureDevicePosition::Unspecified,
    )
}

/// Ensure camera access, requesting it once if undetermined.
///
/// The discovery session reports no devices until access is granted. When the
/// status is NotDetermined we ask for it and block on the answer. Returns true
/// if access is (now) authorized.
fn ensure_access() -> bool {
    unsafe {
        let media = AVMediaTypeVideo;
        let status = AVCaptureDevice::authorizationStatusForMediaType(media.unwrap());
        if status == AVAuthorizationStatus::Authorized {
            return true;
        }
        if status != AVAuthorizationStatus::NotDetermined {
            return false;
        }
        let (tx, rx) = std::sync::mpsc::channel::<bool>();
        let handler = block2::RcBlock::new(move |granted: Bool| {
            let _ = tx.send(granted.as_bool());
        });
        AVCaptureDevice::requestAccessForMediaType_completionHandler(media.unwrap(), &handler);
        rx.recv_timeout(Duration::from_secs(60)).unwrap_or(false)
    }
}

/// List the video capture devices the host exposes.
pub fn enumerate() -> Vec<Device> {
    if !ensure_access() {
        return Vec::new();
    }
    let mut out = Vec::new();
    unsafe {
        let session = discovery_session();
        for device in session.devices().iter() {
            out.push(device_to_descriptor(&device));
        }
    }
    out
}

/// Build our descriptor from an AVCaptureDevice.
unsafe fn device_to_descriptor(device: &AVCaptureDevice) -> Device {
    let id = device.uniqueID().to_string();
    let name = device.localizedName().to_string();
    let facing = match device.position() {
        AVCaptureDevicePosition::Front => "front",
        AVCaptureDevicePosition::Back => "back",
        _ => "external",
    }
    .to_string();

    let mut formats = Vec::new();
    for format in device.formats().iter() {
        let desc = format.formatDescription();
        let dims = CMVideoFormatDescriptionGetDimensions(&desc);
        let mut max_fps = 0.0;
        for range in format.videoSupportedFrameRateRanges().iter() {
            let r = range.maxFrameRate();
            if r > max_fps {
                max_fps = r;
            }
        }
        // Native formats that differ only in pixel format (e.g. 420v vs 420f)
        // all deliver BGRA here, so keep one entry per size at its best rate.
        let same_size = |f: &&mut Format| f.width == dims.width && f.height == dims.height;
        if let Some(existing) = formats.iter_mut().find(same_size) {
            if max_fps > existing.frame_rate {
                existing.frame_rate = max_fps;
            }
            continue;
        }
        formats.push(Format {
            width: dims.width,
            height: dims.height,
            // We request BGRA from the output, so that is the delivered format
            // regardless of the device's native format.
            pixel_format: PixelFormat::Bgra8888,
            frame_rate: max_fps,
        });
    }

    Device {
        id,
        name,
        facing,
        formats,
    }
}

/// Ivars for the capture delegate: the current frame sink, if streaming.
struct DelegateIvars {
    sink: Mutex<Option<FrameSink>>,
}

define_class!(
    // SAFETY: subclasses NSObject and declares no unusual requirements. The
    // delegate is only messaged by AVFoundation on the capture queue, so it is
    // an any-thread class (not MainThreadOnly).
    #[unsafe(super(NSObject))]
    #[name = "CameraControlCaptureDelegate"]
    #[ivars = DelegateIvars]
    struct CaptureDelegate;

    unsafe impl NSObjectProtocol for CaptureDelegate {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for CaptureDelegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        unsafe fn did_output_sample_buffer(
            &self,
            _output: &AVCaptureOutput,
            sample_buffer: &CMSampleBuffer,
            _connection: &AVCaptureConnection,
        ) {
            // Hold the lock across emit, not just to read the sink: this makes
            // stop() (which takes the same lock to clear the sink) synchronous
            // with respect to an in-flight frame, so the callback can never run
            // after the consumer has detached. Without this, a frame already
            // past the lock could emit into a torn-down Dart isolate and abort.
            let guard = self.ivars().sink.lock().unwrap();
            let Some(sink) = guard.as_ref() else { return };
            if let Some(frame) = extract_frame(sample_buffer) {
                sink.emit(frame);
            }
        }
    }
);

impl CaptureDelegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars {
            sink: Mutex::new(None),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn set_sink(&self, sink: Option<FrameSink>) {
        *self.ivars().sink.lock().unwrap() = sink;
    }
}

/// Copy a BGRA frame out of a sample buffer into a tightly-packed Vec.
unsafe fn extract_frame(sample_buffer: &CMSampleBuffer) -> Option<Frame> {
    let pixel_buffer = sample_buffer.image_buffer()?;
    if CVPixelBufferLockBaseAddress(&pixel_buffer, CVPixelBufferLockFlags::ReadOnly) != 0 {
        return None;
    }
    let width = CVPixelBufferGetWidth(&pixel_buffer);
    let height = CVPixelBufferGetHeight(&pixel_buffer);
    let bytes_per_row = CVPixelBufferGetBytesPerRow(&pixel_buffer);
    let base = CVPixelBufferGetBaseAddress(&pixel_buffer);
    if base.is_null() {
        // No pixels to copy: drop the frame rather than emit an empty buffer
        // that still claims width x height.
        CVPixelBufferUnlockBaseAddress(&pixel_buffer, CVPixelBufferLockFlags::ReadOnly);
        return None;
    }

    let mut bytes = Vec::with_capacity(width * 4 * height);
    let src = base as *const u8;
    let row_len = width * 4;
    for row in 0..height {
        let row_ptr = src.add(row * bytes_per_row);
        bytes.extend_from_slice(std::slice::from_raw_parts(row_ptr, row_len));
    }
    CVPixelBufferUnlockBaseAddress(&pixel_buffer, CVPixelBufferLockFlags::ReadOnly);

    Some(Frame {
        data: bytes,
        width: width as i32,
        height: height as i32,
        pixel_format: PixelFormat::Bgra8888,
        timestamp_us: timestamp_us(sample_buffer),
    })
}

/// The sample buffer's presentation time in microseconds, or 0 if invalid.
///
/// Capture buffers are stamped on the host clock, which is monotonic. The
/// math is widened to i128 because `value * 1_000_000` overflows i64 for a
/// nanosecond timescale after a few hours of uptime.
unsafe fn timestamp_us(sample_buffer: &CMSampleBuffer) -> i64 {
    let pts = sample_buffer.presentation_time_stamp();
    if !pts.flags.contains(CMTimeFlags::Valid) || pts.timescale <= 0 {
        return 0;
    }
    (pts.value as i128 * 1_000_000 / pts.timescale as i128) as i64
}

/// A running capture session that pushes frames to the delegate's sink.
struct AvSession {
    session: Retained<AVCaptureSession>,
    output: Retained<AVCaptureVideoDataOutput>,
    delegate: Retained<CaptureDelegate>,
    // Retained for the session's lifetime: the delegate queue must outlive the
    // running session, and we do not rely on AVFoundation retaining it for us.
    queue: dispatch2::DispatchRetained<dispatch2::DispatchQueue>,
}

// SAFETY: the AVFoundation objects are only configured on open() and torn down
// on close(), both serialized by the crate's session table. Frame delivery
// happens on the capture queue and only touches the thread-safe FrameSink.
unsafe impl Send for AvSession {}

impl AvSession {
    /// Tear down so that no FrameSink callback is in flight, or can start,
    /// after this returns. Required before the Dart callback is closed. Step 2
    /// is what guarantees it: the delegate holds the sink lock across emit, so
    /// clearing the sink waits out an in-flight frame, and any callback that
    /// runs afterwards finds no sink. Must not be called from the capture
    /// queue itself (the sink lock and the barrier would deadlock); teardown
    /// runs on the caller's thread.
    fn drain_and_stop(&mut self) {
        unsafe {
            // 1. Detach the delegate: no new sample-buffer dispatches.
            self.output.setSampleBufferDelegate_queue(None, None);
        }
        // 2. Clear the sink under its lock: waits for an in-flight emit, and
        //    a block that runs later returns without emitting.
        self.delegate.set_sink(None);
        // 3. Barrier: flush blocks already enqueued on the serial capture
        //    queue. Not needed for the guarantee above; it leaves the queue
        //    idle before the session stops.
        self.queue.exec_sync(|| {});
        // 4. Stop the pipeline. start() reattaches the delegate and restarts.
        unsafe { self.session.stopRunning() };
    }
}

impl FrameSource for AvSession {
    fn start(&mut self, sink: FrameSink) {
        self.delegate.set_sink(Some(sink));
        // stop() detaches the delegate and stops the session; undo both so a
        // stream restarted on the same handle gets frames again. Harmless on
        // the first start, when both are already in place.
        unsafe {
            let proto = ProtocolObject::from_ref(&*self.delegate);
            self.output
                .setSampleBufferDelegate_queue(Some(proto), Some(&self.queue));
            if !self.session.isRunning() {
                self.session.startRunning();
            }
        }
    }

    fn stop(&mut self) {
        self.drain_and_stop();
    }
}

/// Open a device by uniqueID and start the session running.
///
/// The session runs immediately; frames are dropped until a sink is registered
/// via `start`, so there is no captured data until the caller streams.
pub fn open(id: &str) -> Result<Box<dyn FrameSource>, ()> {
    if !ensure_access() {
        return Err(());
    }
    unsafe {
        let device = find_device(id).ok_or(())?;
        let input = AVCaptureDeviceInput::deviceInputWithDevice_error(&device).map_err(|_| ())?;

        let session = AVCaptureSession::new();
        session.beginConfiguration();
        if !session.canAddInput(&input) {
            return Err(());
        }
        session.addInput(&input);

        let output = AVCaptureVideoDataOutput::new();
        output.setAlwaysDiscardsLateVideoFrames(true);
        output.setVideoSettings(Some(&bgra_video_settings()));

        let delegate = CaptureDelegate::new();
        // A NULL attr (DispatchQueueAttr::SERIAL) yields a serial queue, which
        // is what a sample-buffer delegate wants.
        let queue = dispatch2::DispatchQueue::new(
            "camera_cli.capture",
            dispatch2::DispatchQueueAttr::SERIAL,
        );
        let proto = ProtocolObject::from_ref(&*delegate);
        output.setSampleBufferDelegate_queue(Some(proto), Some(&queue));

        if !session.canAddOutput(&output) {
            return Err(());
        }
        session.addOutput(&output);
        orient_connection(&output, &device);
        session.commitConfiguration();
        // startRunning blocks until capture starts, on the caller's thread.
        // Under Flutter that can be the main thread (iOS runs Dart on the
        // platform thread), which Apple advises against blocking.
        session.startRunning();

        Ok(Box::new(AvSession {
            session,
            output,
            delegate,
            queue,
        }))
    }
}

/// Orient the video connection so delivered buffers are portrait-upright.
///
/// AVCaptureVideoDataOutput delivers buffers in the sensor's native (landscape)
/// orientation; on iPhone in portrait that is 90 degrees off. We rotate at the
/// connection so AVFoundation delivers upright buffers with no per-frame cost.
/// extract_frame reads the live buffer dimensions each frame, so the width/
/// height swap from rotation is handled automatically.
///
/// iOS only: macOS webcams are mounted landscape and are already upright.
/// Mirrors the front camera, the usual selfie-preview convention. Limitation:
/// the orientation is fixed to portrait; rotating the device to landscape will
/// be wrong until this observes device orientation.
#[cfg(target_os = "ios")]
unsafe fn orient_connection(output: &AVCaptureVideoDataOutput, device: &AVCaptureDevice) {
    let Some(connection) = output.connectionWithMediaType(AVMediaTypeVideo.unwrap()) else {
        return;
    };
    // Call the orientation selectors via msg_send: the objc2 0.3.2 typed methods
    // fail to resolve for the iOS target, but the selectors exist at runtime.
    // AVCaptureVideoOrientation Portrait == 1.
    const AV_CAPTURE_VIDEO_ORIENTATION_PORTRAIT: isize = 1;
    let supported: bool = msg_send![&connection, isVideoOrientationSupported];
    if supported {
        let _: () = msg_send![
            &connection,
            setVideoOrientation: AV_CAPTURE_VIDEO_ORIENTATION_PORTRAIT
        ];
    }
    if device.position() == AVCaptureDevicePosition::Front
        && connection.isVideoMirroringSupported()
    {
        connection.setAutomaticallyAdjustsVideoMirroring(false);
        connection.setVideoMirrored(true);
    }
}

#[cfg(not(target_os = "ios"))]
unsafe fn orient_connection(_output: &AVCaptureVideoDataOutput, _device: &AVCaptureDevice) {
    // macOS webcams are mounted landscape and are already upright.
}

/// videoSettings dictionary requesting 32-bit BGRA output.
unsafe fn bgra_video_settings() -> Retained<NSDictionary<NSString, AnyObject>> {
    // The CV format-type key is a CFString, toll-free bridged to NSString.
    let key: &NSString = &*(kCVPixelBufferPixelFormatTypeKey as *const _ as *const NSString);
    let value = NSNumber::new_u32(kCVPixelFormatType_32BGRA);
    let value_obj: &AnyObject = &*(Retained::as_ptr(&value) as *const AnyObject);
    NSDictionary::from_slices(&[key], &[value_obj])
}

/// Look up a device by uniqueID across the discoverable types.
unsafe fn find_device(id: &str) -> Option<Retained<AVCaptureDevice>> {
    discovery_session()
        .devices()
        .iter()
        .find(|d| d.uniqueID().to_string() == id)
}
