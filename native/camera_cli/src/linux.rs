//! V4L2 capture backend for Linux, talking to the kernel directly via ioctl.
//!
//! Honors the crate's no-system-deps rule: we do not link `libv4l`, GStreamer,
//! or any `-dev` package. The only thing we use is `libc` (the syscalls
//! open/close/ioctl/mmap/poll, all glibc-resident) and the `/dev/video*` device
//! nodes a default install already ships. The handful of V4L2 structs, ioctl
//! request codes, and FOURCC constants we need are declared here rather than
//! pulled from `linux/videodev2.h`, mirroring how the macOS backend declares the
//! AVFoundation bits it uses.
//!
//! enumerate() scans `/dev/video*`, keeping the nodes that report a single-plane
//! VIDEO_CAPTURE capability (this drops the metadata-only nodes a UVC webcam also
//! exposes), and lists each one's discrete formats. open() validates the node;
//! the capture pipeline itself is built on a worker thread when streaming starts
//! (S_FMT -> REQBUFS/MMAP -> STREAMON -> DQBUF/QBUF loop), converting each frame
//! to BGRA and pushing it to the FrameSink, just like the other backends.

#![allow(non_camel_case_types, non_upper_case_globals)]

use std::ffi::CStr;
use std::os::raw::c_void;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::{Device, Format, Frame, FrameSink, FrameSource, PixelFormat};

// --- ioctl request-code encoding (asm-generic, used by every arch we target) -

const _IOC_NRBITS: u32 = 8;
const _IOC_TYPEBITS: u32 = 8;
const _IOC_SIZEBITS: u32 = 14;
const _IOC_NRSHIFT: u32 = 0;
const _IOC_TYPESHIFT: u32 = _IOC_NRSHIFT + _IOC_NRBITS;
const _IOC_SIZESHIFT: u32 = _IOC_TYPESHIFT + _IOC_TYPEBITS;
const _IOC_DIRSHIFT: u32 = _IOC_SIZESHIFT + _IOC_SIZEBITS;

const _IOC_WRITE: u32 = 1;
const _IOC_READ: u32 = 2;

const fn _ioc(dir: u32, typ: u32, nr: u32, size: u32) -> u32 {
    (dir << _IOC_DIRSHIFT)
        | (typ << _IOC_TYPESHIFT)
        | (nr << _IOC_NRSHIFT)
        | (size << _IOC_SIZESHIFT)
}
const fn _ior(typ: u32, nr: u32, size: u32) -> u32 {
    _ioc(_IOC_READ, typ, nr, size)
}
const fn _iow(typ: u32, nr: u32, size: u32) -> u32 {
    _ioc(_IOC_WRITE, typ, nr, size)
}
const fn _iowr(typ: u32, nr: u32, size: u32) -> u32 {
    _ioc(_IOC_READ | _IOC_WRITE, typ, nr, size)
}

const VIDIOC_TYPE: u32 = b'V' as u32;

// Request codes are derived from our own struct sizes, so they automatically
// match the kernel as long as the layouts below mirror videodev2.h.
const VIDIOC_QUERYCAP: u32 = _ior(VIDIOC_TYPE, 0, size_of::<v4l2_capability>() as u32);
const VIDIOC_ENUM_FMT: u32 = _iowr(VIDIOC_TYPE, 2, size_of::<v4l2_fmtdesc>() as u32);
const VIDIOC_G_FMT: u32 = _iowr(VIDIOC_TYPE, 4, size_of::<v4l2_format>() as u32);
const VIDIOC_S_FMT: u32 = _iowr(VIDIOC_TYPE, 5, size_of::<v4l2_format>() as u32);
const VIDIOC_REQBUFS: u32 = _iowr(VIDIOC_TYPE, 8, size_of::<v4l2_requestbuffers>() as u32);
const VIDIOC_QUERYBUF: u32 = _iowr(VIDIOC_TYPE, 9, size_of::<v4l2_buffer>() as u32);
const VIDIOC_QBUF: u32 = _iowr(VIDIOC_TYPE, 15, size_of::<v4l2_buffer>() as u32);
const VIDIOC_DQBUF: u32 = _iowr(VIDIOC_TYPE, 17, size_of::<v4l2_buffer>() as u32);
const VIDIOC_STREAMON: u32 = _iow(VIDIOC_TYPE, 18, size_of::<i32>() as u32);
const VIDIOC_STREAMOFF: u32 = _iow(VIDIOC_TYPE, 19, size_of::<i32>() as u32);
const VIDIOC_ENUM_FRAMESIZES: u32 = _iowr(VIDIOC_TYPE, 74, size_of::<v4l2_frmsizeenum>() as u32);
const VIDIOC_ENUM_FRAMEINTERVALS: u32 =
    _iowr(VIDIOC_TYPE, 75, size_of::<v4l2_frmivalenum>() as u32);

// --- V4L2 constants ----------------------------------------------------------

const V4L2_CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
const V4L2_CAP_VIDEO_CAPTURE_MPLANE: u32 = 0x0000_1000;
const V4L2_CAP_STREAMING: u32 = 0x0400_0000;
const V4L2_CAP_DEVICE_CAPS: u32 = 0x8000_0000;

const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const V4L2_MEMORY_MMAP: u32 = 1;
const V4L2_FIELD_NONE: u32 = 1;

const V4L2_FRMSIZE_TYPE_DISCRETE: u32 = 1;
const V4L2_FRMIVAL_TYPE_DISCRETE: u32 = 1;

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

// The raw formats we can deliver. UVC webcams universally offer YUYV; we convert
// it (and the closely-related UYVY) to BGRA in software. MJPEG would need a JPEG
// decoder we do not depend on, so we never negotiate it for capture.
const V4L2_PIX_FMT_YUYV: u32 = fourcc(b'Y', b'U', b'Y', b'V');
const V4L2_PIX_FMT_UYVY: u32 = fourcc(b'U', b'Y', b'V', b'Y');
const V4L2_PIX_FMT_RGB24: u32 = fourcc(b'R', b'G', b'B', b'3');
const V4L2_PIX_FMT_BGR24: u32 = fourcc(b'B', b'G', b'R', b'3');
const V4L2_PIX_FMT_MJPEG: u32 = fourcc(b'M', b'J', b'P', b'G');

// Resolution we ask the driver for; it clamps to the nearest supported size and
// reports back what we got, which is what every Frame then carries.
const PREFERRED_WIDTH: u32 = 1280;
const PREFERRED_HEIGHT: u32 = 720;

// --- struct layouts (must mirror linux/videodev2.h) --------------------------

#[repr(C)]
struct v4l2_capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct v4l2_pix_format {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

// struct v4l2_format: a type tag followed by a 200-byte union. The union has
// arms containing pointers (v4l2_window), so it takes pointer alignment. On
// 64-bit targets that means 4 bytes of padding after `type_`, putting `pix` at
// offset 8 and the whole struct at 208 bytes; on 32-bit targets there is no
// padding and the struct is 204 bytes. We only ever touch the v4l2_pix_format
// arm and pad out the rest of the union.
#[repr(C)]
struct v4l2_format {
    type_: u32,
    #[cfg(target_pointer_width = "64")]
    _align_pad: u32,
    pix: v4l2_pix_format,
    _union_pad: [u8; 200 - size_of::<v4l2_pix_format>()],
}

#[repr(C)]
struct v4l2_requestbuffers {
    count: u32,
    type_: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

#[repr(C)]
struct v4l2_timecode {
    type_: u32,
    flags: u32,
    frames: u8,
    seconds: u8,
    minutes: u8,
    hours: u8,
    userbits: [u8; 4],
}

#[repr(C)]
struct v4l2_buffer {
    index: u32,
    type_: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    timestamp: libc::timeval,
    timecode: v4l2_timecode,
    sequence: u32,
    memory: u32,
    // union m { __u32 offset; unsigned long userptr; void* planes; __s32 fd; }.
    // Pointer-width on every target, so usize matches the kernel's size/align;
    // for MMAP buffers the low 32 bits are the mmap offset.
    m: usize,
    length: u32,
    reserved2: u32,
    // union { __s32 request_fd; __u32 reserved; }.
    request_fd: u32,
}

#[repr(C)]
struct v4l2_fmtdesc {
    index: u32,
    type_: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct v4l2_frmsize_discrete {
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct v4l2_frmsize_stepwise {
    min_width: u32,
    max_width: u32,
    step_width: u32,
    min_height: u32,
    max_height: u32,
    step_height: u32,
}

#[repr(C)]
union v4l2_frmsize_union {
    discrete: v4l2_frmsize_discrete,
    stepwise: v4l2_frmsize_stepwise,
}

#[repr(C)]
struct v4l2_frmsizeenum {
    index: u32,
    pixel_format: u32,
    type_: u32,
    size: v4l2_frmsize_union,
    reserved: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct v4l2_fract {
    numerator: u32,
    denominator: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct v4l2_frmival_stepwise {
    min: v4l2_fract,
    max: v4l2_fract,
    step: v4l2_fract,
}

#[repr(C)]
union v4l2_frmival_union {
    discrete: v4l2_fract,
    stepwise: v4l2_frmival_stepwise,
}

#[repr(C)]
struct v4l2_frmivalenum {
    index: u32,
    pixel_format: u32,
    width: u32,
    height: u32,
    type_: u32,
    interval: v4l2_frmival_union,
    reserved: [u32; 2],
}

// --- thin syscall helpers ----------------------------------------------------

/// ioctl that retries on EINTR. Returns the raw return value (< 0 is an error).
unsafe fn xioctl(fd: i32, request: u32, arg: *mut c_void) -> i32 {
    loop {
        let r = libc::ioctl(fd, request as libc::c_ulong, arg);
        if r == -1 && *libc::__errno_location() == libc::EINTR {
            continue;
        }
        return r;
    }
}

/// An owned file descriptor that closes itself on drop.
struct Fd(i32);

impl Fd {
    /// Open a device node read/write and non-blocking (we drive it with poll()).
    fn open(path: &CStr) -> Option<Fd> {
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK) };
        if fd < 0 {
            None
        } else {
            Some(Fd(fd))
        }
    }
}

impl Drop for Fd {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

/// Trim a fixed-size, NUL-padded C string field into a Rust String.
fn c_field(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Does this capability bitset describe a single-plane video capture device we
/// can stream from?
fn is_streaming_capture(cap: &v4l2_capability) -> bool {
    // Prefer device_caps (this specific node) when the driver advertises it.
    let caps = if cap.capabilities & V4L2_CAP_DEVICE_CAPS != 0 {
        cap.device_caps
    } else {
        cap.capabilities
    };
    caps & V4L2_CAP_VIDEO_CAPTURE != 0
        && caps & V4L2_CAP_VIDEO_CAPTURE_MPLANE == 0
        && caps & V4L2_CAP_STREAMING != 0
}

// --- enumeration -------------------------------------------------------------

/// List the V4L2 capture devices the host exposes.
pub fn enumerate() -> Vec<Device> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/dev") else {
        return out;
    };
    // Collect, then sort by node index so video0 comes before video10.
    let mut nodes: Vec<(u32, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(idx) = name
            .strip_prefix("video")
            .and_then(|n| n.parse::<u32>().ok())
        {
            nodes.push((idx, entry.path()));
        }
    }
    nodes.sort_by_key(|(idx, _)| *idx);

    for (_, path) in nodes {
        if let Some(dev) = probe_device(&path) {
            out.push(dev);
        }
    }
    out
}

/// Query one node; return a Device if it is a streaming capture device.
fn probe_device(path: &std::path::Path) -> Option<Device> {
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let fd = Fd::open(&c_path)?;

    let mut cap: v4l2_capability = unsafe { std::mem::zeroed() };
    if unsafe { xioctl(fd.0, VIDIOC_QUERYCAP, &mut cap as *mut _ as *mut c_void) } < 0 {
        return None;
    }
    if !is_streaming_capture(&cap) {
        return None;
    }

    let name = {
        let card = c_field(&cap.card);
        if card.is_empty() {
            path.to_string_lossy().into_owned()
        } else {
            card
        }
    };

    Some(Device {
        id: path.to_string_lossy().into_owned(),
        name,
        // V4L2 does not report a lens facing for USB/built-in webcams.
        facing: "external".to_string(),
        formats: enumerate_formats(fd.0),
    })
}

/// Enumerate a device's discrete (pixel format, size, fps) combinations.
///
/// Bounded so a camera advertising a large matrix cannot produce an unwieldy
/// device list; the per-frame dimensions remain authoritative regardless.
fn enumerate_formats(fd: i32) -> Vec<Format> {
    const MAX_FORMATS: usize = 64;
    let mut out = Vec::new();

    let mut fmt_index = 0u32;
    loop {
        let mut fmt: v4l2_fmtdesc = unsafe { std::mem::zeroed() };
        fmt.index = fmt_index;
        fmt.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        if unsafe { xioctl(fd, VIDIOC_ENUM_FMT, &mut fmt as *mut _ as *mut c_void) } < 0 {
            break;
        }
        fmt_index += 1;

        let Some(pixel_format) = map_pixel_format(fmt.pixelformat) else {
            continue;
        };

        for (w, h) in enumerate_sizes(fd, fmt.pixelformat) {
            if out.len() >= MAX_FORMATS {
                return out;
            }
            out.push(Format {
                width: w as i32,
                height: h as i32,
                pixel_format,
                frame_rate: best_frame_rate(fd, fmt.pixelformat, w, h),
            });
        }
    }
    out
}

/// Discrete frame sizes for a pixel format (stepwise/continuous reduced to max).
fn enumerate_sizes(fd: i32, pixel_format: u32) -> Vec<(u32, u32)> {
    let mut sizes = Vec::new();
    let mut index = 0u32;
    loop {
        let mut fs: v4l2_frmsizeenum = unsafe { std::mem::zeroed() };
        fs.index = index;
        fs.pixel_format = pixel_format;
        if unsafe { xioctl(fd, VIDIOC_ENUM_FRAMESIZES, &mut fs as *mut _ as *mut c_void) } < 0 {
            break;
        }
        index += 1;
        if fs.type_ == V4L2_FRMSIZE_TYPE_DISCRETE {
            let d = unsafe { fs.size.discrete };
            sizes.push((d.width, d.height));
        } else {
            // Stepwise/continuous: report the maximum as a representative size.
            let s = unsafe { fs.size.stepwise };
            sizes.push((s.max_width, s.max_height));
            break;
        }
        if sizes.len() >= 16 {
            break;
        }
    }
    sizes
}

/// Highest discrete frame rate the device offers for a format/size, or 0.0.
fn best_frame_rate(fd: i32, pixel_format: u32, width: u32, height: u32) -> f64 {
    let mut best = 0.0f64;
    let mut index = 0u32;
    loop {
        let mut fi: v4l2_frmivalenum = unsafe { std::mem::zeroed() };
        fi.index = index;
        fi.pixel_format = pixel_format;
        fi.width = width;
        fi.height = height;
        if unsafe {
            xioctl(
                fd,
                VIDIOC_ENUM_FRAMEINTERVALS,
                &mut fi as *mut _ as *mut c_void,
            )
        } < 0
        {
            break;
        }
        index += 1;
        if fi.type_ != V4L2_FRMIVAL_TYPE_DISCRETE {
            break;
        }
        let d = unsafe { fi.interval.discrete };
        if d.numerator > 0 && d.denominator > 0 {
            let fps = d.denominator as f64 / d.numerator as f64;
            if fps > best {
                best = fps;
            }
        }
        if index >= 32 {
            break;
        }
    }
    best
}

/// Map a V4L2 FOURCC to our wire PixelFormat, or None if we do not expose it.
fn map_pixel_format(fourcc: u32) -> Option<PixelFormat> {
    match fourcc {
        V4L2_PIX_FMT_YUYV | V4L2_PIX_FMT_UYVY => Some(PixelFormat::Yuyv422),
        V4L2_PIX_FMT_RGB24 | V4L2_PIX_FMT_BGR24 => Some(PixelFormat::Rgba8888),
        V4L2_PIX_FMT_MJPEG => Some(PixelFormat::Mjpeg),
        _ => None,
    }
}

// --- capture -----------------------------------------------------------------

/// Open a device by its `/dev/videoN` path. Validates it is a capture node; the
/// capture pipeline is built on the worker thread when streaming starts.
pub fn open(id: &str) -> Result<Box<dyn FrameSource>, ()> {
    let c_path = std::ffi::CString::new(id).map_err(|_| ())?;
    let fd = Fd::open(&c_path).ok_or(())?;
    let mut cap: v4l2_capability = unsafe { std::mem::zeroed() };
    if unsafe { xioctl(fd.0, VIDIOC_QUERYCAP, &mut cap as *mut _ as *mut c_void) } < 0
        || !is_streaming_capture(&cap)
    {
        return Err(());
    }
    Ok(Box::new(V4l2Session {
        path: c_path,
        stop: Arc::new(AtomicBool::new(false)),
        thread: None,
    }))
}

/// A capture session: a worker thread running a V4L2 mmap streaming loop.
struct V4l2Session {
    path: std::ffi::CString,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl V4l2Session {
    /// Signal the worker to stop and join it, so no emit is in flight after.
    fn stop_thread(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl FrameSource for V4l2Session {
    fn start(&mut self, sink: FrameSink) {
        // Replace any prior stream.
        self.stop_thread();
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = stop.clone();
        let path = self.path.clone();
        self.thread = Some(std::thread::spawn(move || {
            // Errors here just mean no frames are delivered; the consumer sees an
            // empty stream rather than a crash.
            let _ = capture_loop(&path, sink, &stop);
        }));
    }

    fn stop(&mut self) {
        self.stop_thread();
    }
}

impl Drop for V4l2Session {
    fn drop(&mut self) {
        self.stop_thread();
    }
}

/// One mmap'd capture buffer.
struct MappedBuffer {
    start: *mut u8,
    length: usize,
}

impl MappedBuffer {
    unsafe fn unmap(&self) {
        if !self.start.is_null() {
            libc::munmap(self.start as *mut c_void, self.length);
        }
    }
}

/// Set up streaming on `path` and pump frames to `sink` until `stop` is set.
fn capture_loop(path: &CStr, sink: FrameSink, stop: &AtomicBool) -> Result<(), ()> {
    let fd = Fd::open(path).ok_or(())?;

    // Negotiate a raw format we can convert. Ask for YUYV at our preferred size;
    // the driver clamps the size and tells us the format it set.
    let mut fmt: v4l2_format = unsafe { std::mem::zeroed() };
    fmt.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    fmt.pix.width = PREFERRED_WIDTH;
    fmt.pix.height = PREFERRED_HEIGHT;
    fmt.pix.pixelformat = V4L2_PIX_FMT_YUYV;
    fmt.pix.field = V4L2_FIELD_NONE;
    if unsafe { xioctl(fd.0, VIDIOC_S_FMT, &mut fmt as *mut _ as *mut c_void) } < 0 {
        // Fall back to whatever the device currently reports.
        if unsafe { xioctl(fd.0, VIDIOC_G_FMT, &mut fmt as *mut _ as *mut c_void) } < 0 {
            return Err(());
        }
    }

    let width = fmt.pix.width;
    let height = fmt.pix.height;
    let bytesperline = fmt.pix.bytesperline.max(width) as usize;
    let pixfmt = fmt.pix.pixelformat;
    // We can only deliver formats we know how to turn into BGRA. MJPEG and other
    // compressed/exotic formats are rejected rather than emitting garbage.
    if convert_to_bgra(pixfmt, &[], width, height, bytesperline).is_none()
        && !is_convertible(pixfmt)
    {
        return Err(());
    }

    // Request a small pool of mmap buffers.
    const BUFFER_COUNT: u32 = 4;
    let mut req: v4l2_requestbuffers = unsafe { std::mem::zeroed() };
    req.count = BUFFER_COUNT;
    req.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    req.memory = V4L2_MEMORY_MMAP;
    if unsafe { xioctl(fd.0, VIDIOC_REQBUFS, &mut req as *mut _ as *mut c_void) } < 0
        || req.count < 1
    {
        return Err(());
    }

    // Map and queue every buffer.
    let mut buffers: Vec<MappedBuffer> = Vec::with_capacity(req.count as usize);
    for i in 0..req.count {
        let mut buf: v4l2_buffer = unsafe { std::mem::zeroed() };
        buf.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        buf.memory = V4L2_MEMORY_MMAP;
        buf.index = i;
        if unsafe { xioctl(fd.0, VIDIOC_QUERYBUF, &mut buf as *mut _ as *mut c_void) } < 0 {
            unmap_all(&buffers);
            return Err(());
        }
        let offset = buf.m as libc::off_t;
        let start = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                buf.length as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.0,
                offset,
            )
        };
        if start == libc::MAP_FAILED {
            unmap_all(&buffers);
            return Err(());
        }
        buffers.push(MappedBuffer {
            start: start as *mut u8,
            length: buf.length as usize,
        });

        // Queue it so the driver can fill it.
        let mut qbuf: v4l2_buffer = unsafe { std::mem::zeroed() };
        qbuf.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        qbuf.memory = V4L2_MEMORY_MMAP;
        qbuf.index = i;
        if unsafe { xioctl(fd.0, VIDIOC_QBUF, &mut qbuf as *mut _ as *mut c_void) } < 0 {
            unmap_all(&buffers);
            return Err(());
        }
    }

    // Start streaming.
    let mut buf_type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    if unsafe {
        xioctl(
            fd.0,
            VIDIOC_STREAMON,
            &mut buf_type as *mut _ as *mut c_void,
        )
    } < 0
    {
        unmap_all(&buffers);
        return Err(());
    }

    // Dequeue / convert / requeue until asked to stop.
    while !stop.load(Ordering::SeqCst) {
        let mut pfd = libc::pollfd {
            fd: fd.0,
            events: libc::POLLIN,
            revents: 0,
        };
        // 200 ms timeout so we re-check the stop flag promptly.
        let pr = unsafe { libc::poll(&mut pfd, 1, 200) };
        if pr < 0 {
            if unsafe { *libc::__errno_location() } == libc::EINTR {
                continue;
            }
            break;
        }
        // An unplugged device (or a queue in error) reports POLLERR/POLLHUP
        // immediately and never POLLIN, so waiting for POLLIN would spin.
        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            break;
        }
        if pr == 0 || pfd.revents & libc::POLLIN == 0 {
            continue;
        }

        let mut buf: v4l2_buffer = unsafe { std::mem::zeroed() };
        buf.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        buf.memory = V4L2_MEMORY_MMAP;
        if unsafe { xioctl(fd.0, VIDIOC_DQBUF, &mut buf as *mut _ as *mut c_void) } < 0 {
            let err = unsafe { *libc::__errno_location() };
            if err == libc::EAGAIN {
                continue;
            }
            break;
        }

        let index = buf.index as usize;
        if index < buffers.len() {
            let mapped = &buffers[index];
            let used = (buf.bytesused as usize).min(mapped.length);
            let bytes = unsafe { std::slice::from_raw_parts(mapped.start, used) };
            if let Some(data) = convert_to_bgra(pixfmt, bytes, width, height, bytesperline) {
                let ts = buf.timestamp;
                let timestamp_us = (ts.tv_sec as i64) * 1_000_000 + ts.tv_usec as i64;
                sink.emit(Frame {
                    data,
                    width: width as i32,
                    height: height as i32,
                    pixel_format: PixelFormat::Bgra8888,
                    timestamp_us,
                });
            }
        }

        // Requeue the buffer for reuse.
        if unsafe { xioctl(fd.0, VIDIOC_QBUF, &mut buf as *mut _ as *mut c_void) } < 0 {
            break;
        }
    }

    // Stop streaming and release the buffers.
    let mut buf_type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    unsafe {
        xioctl(
            fd.0,
            VIDIOC_STREAMOFF,
            &mut buf_type as *mut _ as *mut c_void,
        );
    }
    unmap_all(&buffers);
    Ok(())
}

fn unmap_all(buffers: &[MappedBuffer]) {
    for b in buffers {
        unsafe { b.unmap() };
    }
}

/// Whether we have a software conversion to BGRA for this FOURCC.
fn is_convertible(fourcc: u32) -> bool {
    matches!(
        fourcc,
        V4L2_PIX_FMT_YUYV | V4L2_PIX_FMT_UYVY | V4L2_PIX_FMT_RGB24 | V4L2_PIX_FMT_BGR24
    )
}

/// Convert one captured frame to tightly-packed top-down BGRA, honoring the
/// device's row stride. Returns None for formats we cannot decode here.
fn convert_to_bgra(
    fourcc: u32,
    src: &[u8],
    width: u32,
    height: u32,
    bytesperline: usize,
) -> Option<Vec<u8>> {
    // The probe call passes an empty slice purely to test format support.
    if src.is_empty() {
        return None;
    }
    match fourcc {
        V4L2_PIX_FMT_YUYV => Some(yuv422_to_bgra(src, width, height, bytesperline, false)),
        V4L2_PIX_FMT_UYVY => Some(yuv422_to_bgra(src, width, height, bytesperline, true)),
        V4L2_PIX_FMT_RGB24 => Some(rgb24_to_bgra(src, width, height, bytesperline, false)),
        V4L2_PIX_FMT_BGR24 => Some(rgb24_to_bgra(src, width, height, bytesperline, true)),
        _ => None,
    }
}

/// Packed YUV 4:2:2 (YUYV, or UYVY when `uyvy`) -> BGRA, two pixels per macro.
fn yuv422_to_bgra(src: &[u8], width: u32, height: u32, stride: usize, uyvy: bool) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut out = vec![0u8; w * h * 4];
    for row in 0..h {
        let row_off = row * stride;
        if row_off + w * 2 > src.len() {
            break;
        }
        for col2 in 0..(w / 2) {
            let i = row_off + col2 * 4;
            let (y0, u, y1, v) = if uyvy {
                (src[i + 1], src[i], src[i + 3], src[i + 2])
            } else {
                (src[i], src[i + 1], src[i + 2], src[i + 3])
            };
            let o = (row * w + col2 * 2) * 4;
            write_yuv_pixel(&mut out, o, y0, u, v);
            write_yuv_pixel(&mut out, o + 4, y1, u, v);
        }
    }
    out
}

#[inline]
fn write_yuv_pixel(out: &mut [u8], o: usize, y: u8, u: u8, v: u8) {
    let c = y as i32 - 16;
    let d = u as i32 - 128;
    let e = v as i32 - 128;
    let r = (298 * c + 409 * e + 128) >> 8;
    let g = (298 * c - 100 * d - 208 * e + 128) >> 8;
    let b = (298 * c + 516 * d + 128) >> 8;
    out[o] = clamp_u8(b);
    out[o + 1] = clamp_u8(g);
    out[o + 2] = clamp_u8(r);
    out[o + 3] = 255;
}

/// Packed 24-bit RGB (or BGR when `bgr`) -> BGRA.
fn rgb24_to_bgra(src: &[u8], width: u32, height: u32, stride: usize, bgr: bool) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let mut out = vec![0u8; w * h * 4];
    for row in 0..h {
        let row_off = row * stride;
        if row_off + w * 3 > src.len() {
            break;
        }
        for col in 0..w {
            let i = row_off + col * 3;
            let (r, g, b) = if bgr {
                (src[i + 2], src[i + 1], src[i])
            } else {
                (src[i], src[i + 1], src[i + 2])
            };
            let o = (row * w + col) * 4;
            out[o] = b;
            out[o + 1] = g;
            out[o + 2] = r;
            out[o + 3] = 255;
        }
    }
    out
}

#[inline]
fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    // The ioctl request codes are size-derived; if a struct layout drifts from
    // the kernel's these change, so pin the well-known values from videodev2.h
    // before a drift can silently corrupt a real ioctl. v4l2_format and
    // v4l2_buffer hold pointer-sized members, so their codes differ between
    // 64-bit and 32-bit targets (the 32-bit values assume a 32-bit time_t).
    #[test]
    fn ioctl_codes_match_kernel() {
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_ENUM_FMT, 0xc040_5602);
        assert_eq!(VIDIOC_REQBUFS, 0xc014_5608);
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(VIDIOC_G_FMT, 0xc0d0_5604);
            assert_eq!(VIDIOC_S_FMT, 0xc0d0_5605);
            assert_eq!(VIDIOC_QUERYBUF, 0xc058_5609);
            assert_eq!(VIDIOC_QBUF, 0xc058_560f);
            assert_eq!(VIDIOC_DQBUF, 0xc058_5611);
        }
        #[cfg(target_pointer_width = "32")]
        {
            assert_eq!(VIDIOC_G_FMT, 0xc0cc_5604);
            assert_eq!(VIDIOC_S_FMT, 0xc0cc_5605);
            assert_eq!(VIDIOC_QUERYBUF, 0xc044_5609);
            assert_eq!(VIDIOC_QBUF, 0xc044_560f);
            assert_eq!(VIDIOC_DQBUF, 0xc044_5611);
        }
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_STREAMOFF, 0x4004_5613);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xc02c_564a);
        assert_eq!(VIDIOC_ENUM_FRAMEINTERVALS, 0xc034_564b);
    }

    #[test]
    fn struct_sizes_match_kernel() {
        #[cfg(target_pointer_width = "64")]
        let (fmt_size, pix_offset, buf_size) = (208, 8, 88);
        #[cfg(target_pointer_width = "32")]
        let (fmt_size, pix_offset, buf_size) = (204, 4, 68);
        assert_eq!(size_of::<v4l2_capability>(), 104);
        assert_eq!(size_of::<v4l2_pix_format>(), 48);
        assert_eq!(size_of::<v4l2_format>(), fmt_size);
        assert_eq!(std::mem::offset_of!(v4l2_format, pix), pix_offset);
        assert_eq!(size_of::<v4l2_requestbuffers>(), 20);
        assert_eq!(size_of::<v4l2_buffer>(), buf_size);
        assert_eq!(size_of::<v4l2_fmtdesc>(), 64);
        assert_eq!(size_of::<v4l2_frmsizeenum>(), 44);
        assert_eq!(size_of::<v4l2_frmivalenum>(), 52);
    }

    #[test]
    fn yuyv_gray_roundtrips_to_neutral_bgra() {
        // Y=mid, U=V=128 (neutral chroma) should yield a near-gray opaque pixel.
        let src = [126u8, 128, 126, 128];
        let out = yuv422_to_bgra(&src, 2, 1, 4, false);
        assert_eq!(out.len(), 2 * 4);
        assert_eq!(out[3], 255);
        // B, G, R all roughly equal for neutral chroma.
        assert!((out[0] as i32 - out[2] as i32).abs() <= 2);
    }
}
