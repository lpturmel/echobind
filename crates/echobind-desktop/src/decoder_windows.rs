//! Hardware H.264 decoding for Windows viewers through Media Foundation.
//!
//! OpenH264 decodes NVENC's High-profile CABAC stream on one core, and the
//! build ships its C fallback unless NASM is installed, so a 1440p stream
//! took longer to decode than the host took to send it. Every frame that
//! overflowed the two-slot decode queue then forced a keyframe wait, which
//! collapsed the picture rate to about 10 FPS. Microsoft's decoder runs the
//! stream on the GPU's video engine (DXVA) through a Direct3D 11 device and
//! hands back NV12 that the GPU converts to RGB while drawing.

use super::{
    cap_sps_level, ClientMetrics, DecodeQueue, DisplayFrame, DisplayFrameData, FrameNotifier,
    LatestFrame, SessionEvent, VIDEO_STALE_AGE,
};
use crate::video_renderer::{YuvFrame, YuvPlane};
use std::{
    mem::{size_of, ManuallyDrop},
    ptr, slice,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};
use tracing::warn;
use windows::{
    core::{Interface, GUID},
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11Multithread,
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
            },
            Dxgi::IDXGIAdapter,
        },
        Media::MediaFoundation::{
            CLSID_MSH264DecoderMFT, IMF2DBuffer, IMF2DBuffer2, IMFDXGIDeviceManager, IMFMediaType,
            IMFSample, IMFTransform, MF2DBuffer_LockFlags_Read, MFCreateDXGIDeviceManager,
            MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFShutdown,
            MFStartup, MFVideoArea, MFVideoFormat_H264, MFVideoFormat_NV12,
            MFVideoInterlace_Progressive, MFSTARTUP_LITE, MFT_MESSAGE_COMMAND_FLUSH,
            MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
            MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
            MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
            MF_E_NOTACCEPTING, MF_E_NO_MORE_TYPES, MF_E_TRANSFORM_NEED_MORE_INPUT,
            MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE,
            MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE,
            MF_SA_D3D11_AWARE, MF_VERSION,
        },
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
            COINIT_MULTITHREADED,
        },
    },
};

/// The highest H.264 level Microsoft documents for its decoder (5.1).
///
/// NVENC signals level 6.x at high resolutions and frame rates. As with
/// OpenH264, the level only states the throughput a stream needs.
const MEDIA_FOUNDATION_MAX_LEVEL_IDC: u8 = 51;
/// Keyframes that may fail before any picture decodes before the viewer
/// gives up on Media Foundation and falls back to OpenH264.
const MAX_FAILED_KEYFRAMES: u32 = 3;

pub(super) fn run_decoder(
    running: Arc<AtomicBool>,
    decode_queue: DecodeQueue,
    latest_frame: LatestFrame,
    metrics: Arc<ClientMetrics>,
    needs_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    frame_notifier: FrameNotifier,
) -> Result<(), String> {
    let _com = ComApartment::enter();
    let _media_foundation = MediaFoundation::start()?;
    let mut decoder = H264Decoder::new()?;
    let _ = events.send(SessionEvent::VideoBackend(if decoder.hardware {
        "Media Foundation hardware decoder (DXVA) · GPU NV12 conversion".to_owned()
    } else {
        "Media Foundation software decoder · GPU NV12 conversion".to_owned()
    }));
    let mut decoder_generation = decode_queue.generation();
    let mut decoded_any = false;
    let mut failed_keyframes = 0;

    while running.load(Ordering::Relaxed) {
        let Some((received, generation)) = decode_queue.pop_timeout(Duration::from_millis(20))
        else {
            continue;
        };
        // Use the generation popped with the frame (see DecodeQueue::pop_timeout).
        if generation != decoder_generation {
            decoder.flush();
            decoder_generation = generation;
        }
        let queue_elapsed = received.received_at.elapsed();
        if queue_elapsed > VIDEO_STALE_AGE {
            metrics.dropped_frames.fetch_add(1, Ordering::Relaxed);
            needs_keyframe.store(true, Ordering::Release);
            continue;
        }
        metrics.decode_queue_us.fetch_add(
            queue_elapsed.as_micros().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
        metrics.decode_queue_samples.fetch_add(1, Ordering::Relaxed);
        let mut frame = received.frame;
        if frame.is_keyframe {
            cap_sps_level(&mut frame.payload, MEDIA_FOUNDATION_MAX_LEVEL_IDC);
        }

        let decode_started = Instant::now();
        let result = decoder.decode(&frame.payload, frame.timestamp_us);
        if !decoded_any && frame.is_keyframe && !matches!(result, Ok(Some(_))) {
            failed_keyframes += 1;
            if failed_keyframes >= MAX_FAILED_KEYFRAMES {
                return Err(match result {
                    Err(error) => error,
                    Ok(_) => "Media Foundation returned no picture for a keyframe".to_owned(),
                });
            }
        }
        match result {
            Ok(Some(picture)) => {
                decoded_any = true;
                *latest_frame.lock().unwrap() = Some(DisplayFrame {
                    width: picture.width,
                    height: picture.height,
                    data: DisplayFrameData::Yuv(picture.yuv),
                    published_at: Instant::now(),
                });
                frame_notifier();
                metrics.decoded_frames.fetch_add(1, Ordering::Relaxed);
                metrics.decode_us.fetch_add(
                    decode_started
                        .elapsed()
                        .as_micros()
                        .min(u128::from(u64::MAX)) as u64,
                    Ordering::Relaxed,
                );
                metrics.decode_samples.fetch_add(1, Ordering::Relaxed);
            }
            Ok(None) => {}
            Err(error) => {
                warn!("Media Foundation H.264 decode failed: {error}");
                metrics.dropped_frames.fetch_add(1, Ordering::Relaxed);
                needs_keyframe.store(true, Ordering::Release);
                decoder.flush();
            }
        }
    }
    Ok(())
}

/// Joins the thread to COM's multithreaded apartment for its lifetime.
struct ComApartment {
    initialized: bool,
}

impl ComApartment {
    fn enter() -> Self {
        // A thread already in another apartment can still create the
        // in-process decoder, so only a successful call is undone.
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        Self { initialized }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { CoUninitialize() };
        }
    }
}

struct MediaFoundation;

impl MediaFoundation {
    fn start() -> Result<Self, String> {
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }
            .map_err(|error| format!("Unable to start Media Foundation: {error}"))?;
        Ok(Self)
    }
}

impl Drop for MediaFoundation {
    fn drop(&mut self) {
        let _ = unsafe { MFShutdown() };
    }
}

struct Picture {
    width: usize,
    height: usize,
    yuv: YuvFrame,
}

#[derive(Clone, Copy, Debug)]
struct OutputFormat {
    width: usize,
    height: usize,
    /// Rows in the decoder's luma plane, which pads 1080 to 1088.
    plane_height: usize,
    /// Row pitch of system-memory output, which has no 2D lock.
    default_stride: usize,
}

enum Output {
    Picture(Picture),
    FormatChanged,
    NeedMoreInput,
}

struct H264Decoder {
    // Declared first so the decoder is released before its device.
    transform: IMFTransform,
    _device_manager: Option<IMFDXGIDeviceManager>,
    hardware: bool,
    format: Option<OutputFormat>,
    /// The sample a software decoder writes into, with its capacity.
    software_sample: Option<(IMFSample, u32)>,
}

impl H264Decoder {
    fn new() -> Result<Self, String> {
        let transform: IMFTransform =
            unsafe { CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER) }
                .map_err(|error| format!("Unable to create the H.264 decoder: {error}"))?;
        let attributes = unsafe { transform.GetAttributes() }
            .map_err(|error| format!("H.264 decoder has no attributes: {error}"))?;
        // Without this the decoder holds several pictures back for
        // reordering that a stream without B-frames never needs.
        unsafe { attributes.SetUINT32(&MF_LOW_LATENCY, 1) }
            .map_err(|error| format!("Unable to enable low-latency decoding: {error}"))?;
        let device_manager = match attach_hardware_device(&transform) {
            Ok(manager) => Some(manager),
            Err(error) => {
                warn!("Decoding H.264 in software: {error}");
                None
            }
        };

        let input = video_media_type(&MFVideoFormat_H264)
            .and_then(|input| unsafe {
                input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                Ok(input)
            })
            .map_err(|error| format!("Unable to describe the H.264 input: {error}"))?;
        unsafe { transform.SetInputType(0, &input, 0) }
            .map_err(|error| format!("H.264 decoder rejected its input type: {error}"))?;
        select_nv12_output(&transform)?;
        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .and_then(|()| transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0))
        }
        .map_err(|error| format!("Unable to start the H.264 decoder: {error}"))?;

        Ok(Self {
            transform,
            hardware: device_manager.is_some(),
            _device_manager: device_manager,
            format: None,
            software_sample: None,
        })
    }

    /// Drops every reference picture so the next IDR starts a clean stream.
    fn flush(&mut self) {
        if let Err(error) = unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) } {
            warn!("Unable to flush the H.264 decoder: {error}");
        }
    }

    /// Decodes one access unit and returns the newest picture it produced.
    fn decode(&mut self, payload: &[u8], timestamp_us: u64) -> Result<Option<Picture>, String> {
        let sample = input_sample(payload, timestamp_us)?;
        let mut newest = None;
        if let Err(error) = unsafe { self.transform.ProcessInput(0, &sample, 0) } {
            if error.code() != MF_E_NOTACCEPTING {
                return Err(format!("H.264 decoder rejected a frame: {error}"));
            }
            // Output is pending; collect it, then the decoder takes input.
            newest = self.drain()?;
            unsafe { self.transform.ProcessInput(0, &sample, 0) }
                .map_err(|error| format!("H.264 decoder rejected a frame: {error}"))?;
        }
        Ok(self.drain()?.or(newest))
    }

    fn drain(&mut self) -> Result<Option<Picture>, String> {
        let mut newest = None;
        loop {
            match self.process_output()? {
                Output::Picture(picture) => newest = Some(picture),
                Output::FormatChanged => {}
                Output::NeedMoreInput => return Ok(newest),
            }
        }
    }

    fn process_output(&mut self) -> Result<Output, String> {
        let info = unsafe { self.transform.GetOutputStreamInfo(0) }
            .map_err(|error| format!("Unable to query H.264 decoder output: {error}"))?;
        let provides_samples = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                as u32
            != 0;
        let output_sample = if provides_samples {
            None
        } else {
            Some(self.software_sample(info.cbSize)?)
        };
        let mut output = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(output_sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0;
        let result = unsafe { self.transform.ProcessOutput(0, &mut output, &mut status) };
        let sample = unsafe { ManuallyDrop::take(&mut output[0].pSample) };
        drop(unsafe { ManuallyDrop::take(&mut output[0].pEvents) });

        match result {
            Ok(()) => {}
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                return Ok(Output::NeedMoreInput);
            }
            Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                select_nv12_output(&self.transform)?;
                self.format = Some(output_format(&self.transform)?);
                self.software_sample = None;
                return Ok(Output::FormatChanged);
            }
            Err(error) => return Err(format!("H.264 decoder failed: {error}")),
        }
        let sample = sample.ok_or_else(|| "H.264 decoder returned no picture".to_owned())?;
        let format = match self.format {
            Some(format) => format,
            None => *self.format.insert(output_format(&self.transform)?),
        };
        read_nv12(&sample, format).map(Output::Picture)
    }

    fn software_sample(&mut self, size: u32) -> Result<IMFSample, String> {
        if let Some((sample, capacity)) = &self.software_sample {
            if *capacity >= size {
                return Ok(sample.clone());
            }
        }
        // Before the first picture the decoder may not know its size yet.
        let size = size.max(1);
        let sample = memory_sample(size)
            .map_err(|error| format!("Unable to allocate a decoded picture: {error}"))?;
        self.software_sample = Some((sample.clone(), size));
        Ok(sample)
    }
}

/// Hands the decoder a Direct3D 11 device so it decodes with DXVA.
fn attach_hardware_device(transform: &IMFTransform) -> Result<IMFDXGIDeviceManager, String> {
    let attributes = unsafe { transform.GetAttributes() }.map_err(|error| error.to_string())?;
    if unsafe { attributes.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) == 0 {
        return Err("the decoder does not support Direct3D 11".to_owned());
    }
    let mut device = None::<ID3D11Device>;
    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }
    .map_err(|error| format!("Unable to create a Direct3D 11 video device: {error}"))?;
    let device = device.ok_or_else(|| "Direct3D 11 returned no device".to_owned())?;
    // The decoder and Media Foundation's readback share the immediate context.
    let multithread: ID3D11Multithread = device
        .cast()
        .map_err(|error| format!("Direct3D 11 device is not thread-safe: {error}"))?;
    let _ = unsafe { multithread.SetMultithreadProtected(true) };

    let mut reset_token = 0;
    let mut manager = None;
    unsafe { MFCreateDXGIDeviceManager(&mut reset_token, &mut manager) }
        .map_err(|error| format!("Unable to create a DXGI device manager: {error}"))?;
    let manager =
        manager.ok_or_else(|| "Media Foundation returned no device manager".to_owned())?;
    unsafe { manager.ResetDevice(&device, reset_token) }
        .map_err(|error| format!("Unable to share the Direct3D 11 device: {error}"))?;
    unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) }
        .map_err(|error| format!("the decoder refused the Direct3D 11 device: {error}"))?;
    Ok(manager)
}

fn video_media_type(subtype: &GUID) -> windows::core::Result<IMFMediaType> {
    unsafe {
        let media_type = MFCreateMediaType()?;
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        Ok(media_type)
    }
}

fn memory_sample(size: u32) -> windows::core::Result<IMFSample> {
    unsafe {
        let buffer = MFCreateMemoryBuffer(size)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        Ok(sample)
    }
}

fn select_nv12_output(transform: &IMFTransform) -> Result<(), String> {
    for index in 0.. {
        let available = match unsafe { transform.GetOutputAvailableType(0, index) } {
            Ok(available) => available,
            Err(error) if error.code() == MF_E_NO_MORE_TYPES => break,
            Err(error) => return Err(format!("Unable to list H.264 decoder outputs: {error}")),
        };
        if unsafe { available.GetGUID(&MF_MT_SUBTYPE) }.ok() == Some(MFVideoFormat_NV12) {
            return unsafe { transform.SetOutputType(0, &available, 0) }
                .map_err(|error| format!("H.264 decoder rejected NV12 output: {error}"));
        }
    }
    Err("H.264 decoder offers no NV12 output".to_owned())
}

fn output_format(transform: &IMFTransform) -> Result<OutputFormat, String> {
    let current = unsafe { transform.GetOutputCurrentType(0) }
        .map_err(|error| format!("H.264 decoder has no output type: {error}"))?;
    let frame_size = unsafe { current.GetUINT64(&MF_MT_FRAME_SIZE) }
        .map_err(|error| format!("H.264 decoder output has no frame size: {error}"))?;
    let (coded_width, coded_height) = ((frame_size >> 32) as usize, frame_size as u32 as usize);
    let mut aperture = MFVideoArea::default();
    let (width, height) = match unsafe {
        current.GetBlob(
            &MF_MT_MINIMUM_DISPLAY_APERTURE,
            slice::from_raw_parts_mut(
                ptr::addr_of_mut!(aperture).cast::<u8>(),
                size_of::<MFVideoArea>(),
            ),
            None,
        )
    } {
        Ok(()) if aperture.Area.cx > 0 && aperture.Area.cy > 0 => {
            (aperture.Area.cx as usize, aperture.Area.cy as usize)
        }
        _ => (coded_width, coded_height),
    };
    let default_stride = unsafe { current.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
        .map_or(coded_width, |stride| stride as i32 as usize);
    if width == 0 || height == 0 || width > coded_width || height > coded_height {
        return Err(format!(
            "H.264 decoder reported a {width}x{height} picture in a {coded_width}x{coded_height} frame"
        ));
    }
    Ok(OutputFormat {
        width,
        height,
        plane_height: coded_height,
        default_stride,
    })
}

fn input_sample(payload: &[u8], timestamp_us: u64) -> Result<IMFSample, String> {
    let length = u32::try_from(payload.len()).map_err(|_| "H.264 frame is too large".to_owned())?;
    wrap_payload(payload, length, timestamp_us)
        .map_err(|error| format!("Unable to wrap an H.264 frame: {error}"))
}

fn wrap_payload(
    payload: &[u8],
    length: u32,
    timestamp_us: u64,
) -> windows::core::Result<IMFSample> {
    unsafe {
        let buffer = MFCreateMemoryBuffer(length)?;
        let mut data = ptr::null_mut();
        buffer.Lock(&mut data, None, None)?;
        ptr::copy_nonoverlapping(payload.as_ptr(), data, payload.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(length)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        // Media Foundation counts time in 100 ns units.
        sample.SetSampleTime(timestamp_us.min(i64::MAX as u64 / 10) as i64 * 10)?;
        Ok(sample)
    }
}

/// Copies the visible part of a decoded NV12 picture out of its sample.
fn read_nv12(sample: &IMFSample, format: OutputFormat) -> Result<Picture, String> {
    let buffer = unsafe { sample.GetBufferByIndex(0) }
        .map_err(|error| format!("Decoded picture has no buffer: {error}"))?;
    // A GPU picture is copied to system memory by the lock; the read-only
    // lock skips copying it back on unlock.
    if let Ok(buffer) = buffer.cast::<IMF2DBuffer2>() {
        let (mut scanline0, mut pitch, mut start, mut length) =
            (ptr::null_mut(), 0, ptr::null_mut(), 0);
        unsafe {
            buffer.Lock2DSize(
                MF2DBuffer_LockFlags_Read,
                &mut scanline0,
                &mut pitch,
                &mut start,
                &mut length,
            )
        }
        .map_err(|error| format!("Unable to read a decoded picture: {error}"))?;
        let available = (length as usize).saturating_sub(scanline0 as usize - start as usize);
        let picture = unsafe { nv12_picture(scanline0, available, pitch, format) };
        let _ = unsafe { buffer.Unlock2D() };
        return picture;
    }
    if let Ok(buffer) = buffer.cast::<IMF2DBuffer>() {
        let (mut scanline0, mut pitch) = (ptr::null_mut(), 0);
        unsafe { buffer.Lock2D(&mut scanline0, &mut pitch) }
            .map_err(|error| format!("Unable to read a decoded picture: {error}"))?;
        let available = pitch.max(0) as usize * (format.plane_height + format.plane_height / 2);
        let picture = unsafe { nv12_picture(scanline0, available, pitch, format) };
        let _ = unsafe { buffer.Unlock2D() };
        return picture;
    }
    let (mut data, mut length) = (ptr::null_mut(), 0);
    unsafe { buffer.Lock(&mut data, None, Some(&mut length)) }
        .map_err(|error| format!("Unable to read a decoded picture: {error}"))?;
    let picture =
        unsafe { nv12_picture(data, length as usize, format.default_stride as i32, format) };
    let _ = unsafe { buffer.Unlock() };
    picture
}

/// # Safety
///
/// `data` must point to `length` readable bytes.
unsafe fn nv12_picture(
    data: *const u8,
    length: usize,
    pitch: i32,
    format: OutputFormat,
) -> Result<Picture, String> {
    if data.is_null() {
        return Err("Decoded picture has no pixels".to_owned());
    }
    let bytes = unsafe { slice::from_raw_parts(data, length) };
    nv12_planes(bytes, pitch, format).map(|yuv| Picture {
        width: format.width,
        height: format.height,
        yuv,
    })
}

fn nv12_planes(bytes: &[u8], pitch: i32, format: OutputFormat) -> Result<YuvFrame, String> {
    let pitch = usize::try_from(pitch)
        .ok()
        .filter(|&pitch| pitch >= format.width)
        .ok_or_else(|| format!("Decoded picture has an unusable pitch of {pitch}"))?;
    let chroma_rows = format.height.div_ceil(2);
    let chroma_row_bytes = format.width.div_ceil(2) * 2;
    let uv_offset = pitch * format.plane_height;
    let required = uv_offset + pitch * (chroma_rows - 1) + chroma_row_bytes;
    if bytes.len() < required {
        return Err(format!(
            "Decoded picture holds {} bytes, but {}x{} NV12 needs {required}",
            bytes.len(),
            format.width,
            format.height
        ));
    }
    Ok(YuvFrame::Nv12 {
        y: YuvPlane::copy_rows(bytes, pitch, format.width, format.height),
        uv: YuvPlane::copy_rows(&bytes[uv_offset..], pitch, chroma_row_bytes, chroma_rows),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PADDED_1080P: OutputFormat = OutputFormat {
        width: 1920,
        height: 1080,
        plane_height: 1088,
        default_stride: 1920,
    };

    #[test]
    fn nv12_planes_skip_the_decoders_row_padding() {
        let pitch = 2048;
        let mut bytes = vec![0u8; pitch * (1088 + 544)];
        bytes[pitch * 1079] = 1;
        bytes[pitch * 1088] = 2;
        let YuvFrame::Nv12 { y, uv } = nv12_planes(&bytes, pitch as i32, PADDED_1080P).unwrap()
        else {
            panic!("expected NV12");
        };
        assert_eq!((y.stride, uv.stride), (pitch, pitch));
        assert_eq!(y.data.len(), pitch * 1079 + 1920);
        assert_eq!(y.data[pitch * 1079], 1);
        assert_eq!(uv.data[0], 2);
        assert_eq!(uv.data.len(), pitch * 539 + 1920);
    }

    #[test]
    fn nv12_planes_reject_short_buffers_and_bad_pitches() {
        let bytes = vec![0u8; 1920 * 1088];
        assert!(nv12_planes(&bytes, 1920, PADDED_1080P).is_err());
        assert!(nv12_planes(&bytes, -1920, PADDED_1080P).is_err());
        assert!(nv12_planes(&bytes, 1280, PADDED_1080P).is_err());
    }

    #[test]
    #[ignore = "requires Media Foundation's H.264 decoder"]
    fn media_foundation_decodes_an_openh264_keyframe() {
        let width = 128;
        let height = 72;
        let source = scap::frame::YUVFrame {
            display_time: 0,
            width,
            height,
            luminance_bytes: vec![96; (width * height) as usize],
            luminance_stride: width,
            chrominance_bytes: vec![128; (width * height / 2) as usize],
            chrominance_stride: width,
        };
        let mut i420 = crate::video::I420Frame::default();
        i420.update_from_nv12(&source).unwrap();
        let encoded = openh264::encoder::Encoder::new()
            .unwrap()
            .encode(&i420)
            .unwrap()
            .to_vec();

        let _com = ComApartment::enter();
        let _media_foundation = MediaFoundation::start().unwrap();
        let mut decoder = H264Decoder::new().unwrap();
        let picture = decoder
            .decode(&encoded, 0)
            .unwrap()
            .expect("a low-latency decoder outputs the keyframe at once");
        assert_eq!((picture.width, picture.height), (128, 72));
        let YuvFrame::Nv12 { y, .. } = picture.yuv else {
            panic!("expected NV12");
        };
        assert!(y.data[..128].iter().all(|&luma| luma.abs_diff(96) <= 2));
    }
}
