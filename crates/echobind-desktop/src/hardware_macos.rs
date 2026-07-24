use super::{
    ensure_software_capture_available, spawn_capture, spawn_encoder, CaptureSlot, SessionEvent,
};
use echobind_core::{
    protocol::{Packet, MAX_DATAGRAM_SIZE},
    video::fragment_video_frame,
};
use screencapturekit::prelude::*;
use std::{
    ffi::c_void,
    net::{SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tracing::warn;
use videotoolbox::{ffi as vt, session::Codec};

const MAX_CAPTURE_WIDTH: u32 = 1280;
const MAX_CAPTURE_HEIGHT: u32 = 720;
const ENCODER_WAIT: Duration = Duration::from_millis(30);

type SampleSlot = Arc<(Mutex<Option<CMSampleBuffer>>, Condvar)>;

struct HardwareEncodedFrame {
    data: Vec<u8>,
    sample_buffer: Option<CMSampleBuffer>,
}

struct EncoderState {
    output_tx: Mutex<mpsc::Sender<Result<HardwareEncodedFrame, String>>>,
    output_rx: Mutex<mpsc::Receiver<Result<HardwareEncodedFrame, String>>>,
}

struct HardwareEncoder {
    session: vt::VTCompressionSessionRef,
    state: Arc<EncoderState>,
    callback_state: *const EncoderState,
}

pub(super) fn spawn_hardware_pipeline(
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    force_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    frames_per_second: u32,
    bitrate_bps: u32,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let result = run_hardware_pipeline(
            socket.clone(),
            running.clone(),
            active_peer.clone(),
            force_keyframe.clone(),
            events.clone(),
            frames_per_second,
            bitrate_bps,
        );
        if let Err(error) = result {
            if !running.load(Ordering::Relaxed) {
                return;
            }
            warn!("VideoToolbox pipeline unavailable: {error}");
            let _ = events.send(SessionEvent::VideoBackend(
                "OpenH264 software fallback".to_owned(),
            ));
            if let Err(fallback_error) = start_software_fallback(
                socket,
                running,
                active_peer,
                force_keyframe,
                events.clone(),
                frames_per_second,
                bitrate_bps,
            ) {
                let _ = events.send(SessionEvent::Error(format!(
                    "Hardware H.264 failed ({error}); software fallback failed: {fallback_error}"
                )));
            }
        }
    })
}

fn run_hardware_pipeline(
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    force_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    frames_per_second: u32,
    bitrate_bps: u32,
) -> Result<(), String> {
    let content = SCShareableContent::get().map_err(|error| {
        format!(
            "ScreenCaptureKit could not enumerate displays: {error}. Grant Screen Recording permission in System Settings and restart Echobind"
        )
    })?;
    let displays = content.displays();
    let display = displays
        .first()
        .ok_or_else(|| "ScreenCaptureKit found no displays".to_owned())?;
    let (width, height) = fit_capture_dimensions(display.width(), display.height());
    let frame_interval = CMTime::new(1, frames_per_second as i32);
    let filter = SCContentFilter::create()
        .with_display(display)
        .with_excluding_windows(&[])
        .build();
    let configuration = SCStreamConfiguration::new()
        .with_width(width)
        .with_height(height)
        .with_scales_to_fit(true)
        .with_pixel_format(PixelFormat::YCbCr_420v)
        .with_shows_cursor(true)
        .with_queue_depth(1)
        .with_minimum_frame_interval(&frame_interval);

    let sample_slot: SampleSlot = Arc::new((Mutex::new(None), Condvar::new()));
    let callback_slot = sample_slot.clone();
    let mut stream = SCStream::new(&filter, &configuration);
    stream.add_output_handler(
        move |sample: CMSampleBuffer, _| {
            let (slot, available) = &*callback_slot;
            *slot.lock().unwrap() = Some(sample);
            available.notify_one();
        },
        SCStreamOutputType::Screen,
    );

    let encoder = create_encoder(width, height, frames_per_second, bitrate_bps)?;
    stream
        .start_capture()
        .map_err(|error| format!("ScreenCaptureKit could not start capture: {error}"))?;
    let _ = events.send(SessionEvent::VideoBackend(
        "VideoToolbox hardware encoder · zero-copy capture".to_owned(),
    ));
    let _ = events.send(SessionEvent::CaptureReady);

    let started = Instant::now();
    let mut sequence = 0_u64;
    let mut frame_id = 0_u64;
    let mut encoded_any = false;
    let mut packet = Vec::with_capacity(MAX_DATAGRAM_SIZE);
    let mut stats_started = Instant::now();
    let mut stats_frames = 0_u64;
    let mut stats_bytes = 0_u64;

    while running.load(Ordering::Relaxed) {
        let sample = {
            let (slot, available) = &*sample_slot;
            let guard = slot.lock().unwrap();
            let (mut guard, _) = available
                .wait_timeout_while(guard, ENCODER_WAIT, |frame| {
                    frame.is_none() && running.load(Ordering::Relaxed)
                })
                .unwrap();
            guard.take()
        };
        let Some(sample) = sample else {
            continue;
        };
        let Some(peer) = *active_peer.lock().unwrap() else {
            continue;
        };

        let request_keyframe = encoded_any && force_keyframe.swap(false, Ordering::Relaxed);

        let Some(pixel_buffer) = sample.image_buffer() else {
            continue;
        };
        let Some(surface) = pixel_buffer.io_surface() else {
            continue;
        };
        let encoded = encoder
            .encode(
                &surface,
                (sequence as i64, frames_per_second as i32),
                request_keyframe,
            )
            .map_err(|error| format!("VideoToolbox H.264 encoding failed: {error}"))?;
        sequence = sequence.wrapping_add(1);
        if encoded.data.is_empty() {
            continue;
        }

        let (annex_b, is_keyframe) = avcc_frame_to_annex_b(&encoded)?;
        if annex_b.is_empty() {
            continue;
        }
        if !encoded_any {
            force_keyframe.store(false, Ordering::Relaxed);
        }
        encoded_any = true;

        let timestamp_us = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        let fragments = fragment_video_frame(frame_id, timestamp_us, is_keyframe, &annex_b)
            .map_err(|error| format!("Hardware frame cannot be packetized: {error}"))?;

        let mut frame_sent = true;
        for fragment in fragments {
            Packet::Video(fragment).encode(&mut packet);
            if let Err(error) = socket.send_to(&packet, peer) {
                warn!("Hardware video send to {peer} failed: {error}");
                frame_sent = false;
                break;
            }
            stats_bytes = stats_bytes.saturating_add(packet.len() as u64);
        }
        frame_id = frame_id.wrapping_add(1);
        if frame_sent {
            stats_frames = stats_frames.saturating_add(1);
        }

        let elapsed = stats_started.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let seconds = elapsed.as_secs_f32();
            let _ = events.send(SessionEvent::Stats {
                fps: stats_frames as f32 / seconds,
                megabits_per_second: stats_bytes as f32 * 8.0 / seconds / 1_000_000.0,
            });
            stats_started = Instant::now();
            stats_frames = 0;
            stats_bytes = 0;
        }
    }

    let _ = stream.stop_capture();
    Ok(())
}

fn create_encoder(
    width: u32,
    height: u32,
    frames_per_second: u32,
    bitrate_bps: u32,
) -> Result<HardwareEncoder, String> {
    HardwareEncoder::new(width, height, frames_per_second, bitrate_bps)
}

impl HardwareEncoder {
    fn new(
        width: u32,
        height: u32,
        frames_per_second: u32,
        bitrate_bps: u32,
    ) -> Result<Self, String> {
        let (output_tx, output_rx) = mpsc::channel();
        let state = Arc::new(EncoderState {
            output_tx: Mutex::new(output_tx),
            output_rx: Mutex::new(output_rx),
        });
        let callback_state = Arc::into_raw(state.clone());
        let callback_ref_con = callback_state.cast_mut().cast::<c_void>();
        let specification = unsafe {
            vt::CFDictionaryCreateMutable(
                vt::kCFAllocatorDefault,
                2,
                (&raw const vt::kCFTypeDictionaryKeyCallBacks).cast(),
                (&raw const vt::kCFTypeDictionaryValueCallBacks).cast(),
            )
        };
        if specification.is_null() {
            unsafe {
                drop(Arc::from_raw(callback_state));
            }
            return Err("Unable to allocate VideoToolbox encoder specification".to_owned());
        }
        unsafe {
            vt::CFDictionarySetValue(
                specification,
                vt::kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder.cast(),
                vt::kCFBooleanTrue.cast(),
            );
            vt::CFDictionarySetValue(
                specification,
                vt::kVTVideoEncoderSpecification_EnableLowLatencyRateControl.cast(),
                vt::kCFBooleanTrue.cast(),
            );
        }

        let mut session = std::ptr::null_mut();
        let status = unsafe {
            vt::VTCompressionSessionCreate(
                vt::kCFAllocatorDefault,
                width as i32,
                height as i32,
                Codec::H264.as_cm_codec_type(),
                specification,
                std::ptr::null(),
                vt::kCFAllocatorDefault,
                Some(hardware_encode_callback),
                callback_ref_con,
                &mut session,
            )
        };
        unsafe {
            vt::CFRelease(specification.cast());
        }
        if status != 0 || session.is_null() {
            unsafe {
                drop(Arc::from_raw(callback_state));
            }
            return Err(format!(
                "VideoToolbox could not create a required hardware H.264 encoder (status {status})"
            ));
        }

        let encoder = Self {
            session,
            state,
            callback_state,
        };
        unsafe {
            encoder
                .set_bool(vt::kVTCompressionPropertyKey_RealTime, true)
                .map_err(|error| format!("RealTime: {error}"))?;
            encoder
                .set_bool(vt::kVTCompressionPropertyKey_AllowFrameReordering, false)
                .map_err(|error| format!("AllowFrameReordering: {error}"))?;
            encoder
                .set_i32(
                    vt::kVTCompressionPropertyKey_AverageBitRate,
                    bitrate_bps.min(i32::MAX as u32) as i32,
                )
                .map_err(|error| format!("AverageBitRate: {error}"))?;
            encoder
                .set_f64(
                    vt::kVTCompressionPropertyKey_ExpectedFrameRate,
                    frames_per_second as f64,
                )
                .map_err(|error| format!("ExpectedFrameRate: {error}"))?;
            encoder
                .set_i32(
                    vt::kVTCompressionPropertyKey_MaxKeyFrameInterval,
                    frames_per_second.saturating_mul(2) as i32,
                )
                .map_err(|error| format!("MaxKeyFrameInterval: {error}"))?;
            encoder
                .set_cf_value(
                    vt::kVTCompressionPropertyKey_ProfileLevel,
                    vt::kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel.cast(),
                )
                .map_err(|error| format!("ProfileLevel: {error}"))?;
        }

        let status = unsafe { vt::VTCompressionSessionPrepareToEncodeFrames(encoder.session) };
        if status != 0 {
            return Err(format!(
                "VideoToolbox could not prepare the hardware encoder (status {status})"
            ));
        }
        Ok(encoder)
    }

    fn encode(
        &self,
        surface: &apple_cf::iosurface::IOSurface,
        presentation_time: (i64, i32),
        force_keyframe: bool,
    ) -> Result<HardwareEncodedFrame, String> {
        let mut pixel_buffer = std::ptr::null_mut();
        let status = unsafe {
            vt::CVPixelBufferCreateWithIOSurface(
                vt::kCFAllocatorDefault,
                surface.as_ptr().cast::<c_void>(),
                std::ptr::null(),
                &mut pixel_buffer,
            )
        };
        if status != 0 || pixel_buffer.is_null() {
            return Err(format!(
                "VideoToolbox could not wrap the capture IOSurface (status {status})"
            ));
        }

        let frame_properties = force_keyframe.then(|| unsafe {
            let dictionary = vt::CFDictionaryCreateMutable(
                vt::kCFAllocatorDefault,
                1,
                (&raw const vt::kCFTypeDictionaryKeyCallBacks).cast(),
                (&raw const vt::kCFTypeDictionaryValueCallBacks).cast(),
            );
            if !dictionary.is_null() {
                vt::CFDictionarySetValue(
                    dictionary,
                    vt::kVTEncodeFrameOptionKey_ForceKeyFrame.cast(),
                    vt::kCFBooleanTrue.cast(),
                );
            }
            dictionary
        });
        let frame_properties_ptr = frame_properties.unwrap_or(std::ptr::null_mut());
        let mut info_flags = 0_u32;
        let status = unsafe {
            vt::VTCompressionSessionEncodeFrame(
                self.session,
                pixel_buffer,
                vt::CMTime::new(presentation_time.0, presentation_time.1),
                vt::CMTime::INVALID,
                frame_properties_ptr,
                std::ptr::null_mut(),
                &mut info_flags,
            )
        };
        unsafe {
            vt::CFRelease(pixel_buffer.cast());
            if !frame_properties_ptr.is_null() {
                vt::CFRelease(frame_properties_ptr.cast());
            }
        }
        if status != 0 {
            return Err(format!(
                "VideoToolbox rejected a frame (status {status}, flags {info_flags})"
            ));
        }

        let status =
            unsafe { vt::VTCompressionSessionCompleteFrames(self.session, vt::CMTime::INVALID) };
        if status != 0 {
            return Err(format!(
                "VideoToolbox could not complete a frame (status {status})"
            ));
        }
        self.state
            .output_rx
            .lock()
            .map_err(|_| "VideoToolbox output queue was poisoned".to_owned())?
            .recv_timeout(Duration::from_secs(1))
            .map_err(|error| format!("VideoToolbox output timed out: {error}"))?
    }

    fn set_bool(&self, key: vt::CFStringRef, value: bool) -> Result<(), String> {
        let value = if value {
            unsafe { vt::kCFBooleanTrue }
        } else {
            unsafe { vt::kCFBooleanFalse }
        };
        self.set_cf_value(key, value.cast())
    }

    fn set_i32(&self, key: vt::CFStringRef, value: i32) -> Result<(), String> {
        let number = unsafe {
            vt::CFNumberCreate(
                vt::kCFAllocatorDefault,
                vt::kCFNumberSInt32Type,
                std::ptr::from_ref(&value).cast(),
            )
        };
        if number.is_null() {
            return Err("Unable to allocate a VideoToolbox integer property".to_owned());
        }
        let result = self.set_cf_value(key, number.cast());
        unsafe {
            vt::CFRelease(number.cast());
        }
        result
    }

    fn set_f64(&self, key: vt::CFStringRef, value: f64) -> Result<(), String> {
        let number = unsafe {
            vt::CFNumberCreate(
                vt::kCFAllocatorDefault,
                vt::kCFNumberFloat64Type,
                std::ptr::from_ref(&value).cast(),
            )
        };
        if number.is_null() {
            return Err("Unable to allocate a VideoToolbox floating-point property".to_owned());
        }
        let result = self.set_cf_value(key, number.cast());
        unsafe {
            vt::CFRelease(number.cast());
        }
        result
    }

    fn set_cf_value(&self, key: vt::CFStringRef, value: vt::CFTypeRef) -> Result<(), String> {
        let status = unsafe { vt::VTSessionSetProperty(self.session, key, value) };
        if status == 0 {
            Ok(())
        } else {
            Err(format!(
                "VideoToolbox rejected a low-latency property (status {status})"
            ))
        }
    }
}

impl Drop for HardwareEncoder {
    fn drop(&mut self) {
        if !self.session.is_null() {
            unsafe {
                vt::VTCompressionSessionInvalidate(self.session);
                vt::CFRelease(self.session.cast());
            }
        }
        if !self.callback_state.is_null() {
            unsafe {
                drop(Arc::from_raw(self.callback_state));
            }
        }
    }
}

unsafe extern "C" fn hardware_encode_callback(
    output_callback_ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: vt::OSStatus,
    _info_flags: vt::VTEncodeInfoFlags,
    sample_buffer: vt::CMSampleBufferRef,
) {
    let state = unsafe { Arc::from_raw(output_callback_ref_con.cast::<EncoderState>()) };
    let callback_state = state.clone();
    std::mem::forget(state);

    let result = if status != 0 {
        Err(format!(
            "VideoToolbox callback reported encoding status {status}"
        ))
    } else if sample_buffer.is_null() {
        Ok(HardwareEncodedFrame {
            data: Vec::new(),
            sample_buffer: None,
        })
    } else {
        let block_buffer = unsafe { vt::CMSampleBufferGetDataBuffer(sample_buffer) };
        if block_buffer.is_null() {
            Err("VideoToolbox returned a frame without encoded data".to_owned())
        } else {
            let length = unsafe { vt::CMBlockBufferGetDataLength(block_buffer) };
            let mut data = vec![0_u8; length];
            let copy_status = unsafe {
                vt::CMBlockBufferCopyDataBytes(
                    block_buffer,
                    0,
                    length,
                    data.as_mut_ptr().cast::<c_void>(),
                )
            };
            if copy_status != 0 {
                Err(format!(
                    "VideoToolbox could not copy encoded output (status {copy_status})"
                ))
            } else {
                let sample_buffer =
                    unsafe { CMSampleBuffer::from_raw_retained(sample_buffer.cast()) };
                Ok(HardwareEncodedFrame {
                    data,
                    sample_buffer,
                })
            }
        }
    };

    if let Ok(sender) = callback_state.output_tx.lock() {
        let _ = sender.send(result);
    };
}

fn avcc_frame_to_annex_b(encoded: &HardwareEncodedFrame) -> Result<(Vec<u8>, bool), String> {
    let is_keyframe = avcc_contains_idr(&encoded.data)?;
    let mut output = Vec::with_capacity(encoded.data.len() + 128);

    if is_keyframe {
        if let Some((sps, pps)) = extract_h264_parameter_sets(encoded) {
            append_annex_b_nal(&mut output, &sps);
            append_annex_b_nal(&mut output, &pps);
        }
    }

    let mut position = 0;
    while position < encoded.data.len() {
        let length = read_avcc_length(&encoded.data, position)?;
        position += 4;
        append_annex_b_nal(&mut output, &encoded.data[position..position + length]);
        position += length;
    }
    Ok((output, is_keyframe))
}

fn avcc_contains_idr(data: &[u8]) -> Result<bool, String> {
    let mut position = 0;
    let mut is_keyframe = false;
    while position < data.len() {
        let length = read_avcc_length(data, position)?;
        position += 4;
        if data[position] & 0x1f == 5 {
            is_keyframe = true;
        }
        position += length;
    }
    Ok(is_keyframe)
}

fn read_avcc_length(data: &[u8], position: usize) -> Result<usize, String> {
    let header = data
        .get(position..position + 4)
        .ok_or_else(|| "VideoToolbox returned a truncated AVCC length".to_owned())?;
    let length = u32::from_be_bytes(header.try_into().unwrap()) as usize;
    if length == 0 || position + 4 + length > data.len() {
        return Err("VideoToolbox returned an invalid AVCC NAL unit".to_owned());
    }
    Ok(length)
}

fn append_annex_b_nal(output: &mut Vec<u8>, nal: &[u8]) {
    output.extend_from_slice(&[0, 0, 0, 1]);
    output.extend_from_slice(nal);
}

fn extract_h264_parameter_sets(encoded: &HardwareEncodedFrame) -> Option<(Vec<u8>, Vec<u8>)> {
    let sample = encoded.sample_buffer.as_ref()?;
    let description = sample.format_description()?;

    unsafe {
        let mut sps_pointer = std::ptr::null();
        let mut sps_size = 0_usize;
        let mut parameter_count = 0_usize;
        let mut nal_header_length = 0_i32;
        let status = apple_cf::raw::CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            description.as_ptr().cast(),
            0,
            &mut sps_pointer,
            &mut sps_size,
            &mut parameter_count,
            &mut nal_header_length,
        );
        if status != 0 || sps_pointer.is_null() || parameter_count < 2 {
            return None;
        }

        let mut pps_pointer = std::ptr::null();
        let mut pps_size = 0_usize;
        let status = apple_cf::raw::CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            description.as_ptr().cast(),
            1,
            &mut pps_pointer,
            &mut pps_size,
            &mut parameter_count,
            &mut nal_header_length,
        );
        if status != 0 || pps_pointer.is_null() {
            return None;
        }

        Some((
            std::slice::from_raw_parts(sps_pointer, sps_size).to_vec(),
            std::slice::from_raw_parts(pps_pointer, pps_size).to_vec(),
        ))
    }
}

fn fit_capture_dimensions(width: u32, height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (MAX_CAPTURE_WIDTH, MAX_CAPTURE_HEIGHT);
    }
    let scale = (MAX_CAPTURE_WIDTH as f64 / width as f64)
        .min(MAX_CAPTURE_HEIGHT as f64 / height as f64)
        .min(1.0);
    let fitted_width = ((width as f64 * scale).floor() as u32).max(2) & !1;
    let fitted_height = ((height as f64 * scale).floor() as u32).max(2) & !1;
    (fitted_width, fitted_height)
}

fn start_software_fallback(
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    force_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    frames_per_second: u32,
    bitrate_bps: u32,
) -> Result<(), String> {
    ensure_software_capture_available()?;
    let capture_slot: CaptureSlot = Arc::new((Mutex::new(None), Condvar::new()));
    let _capture_handle = spawn_capture(
        running.clone(),
        capture_slot.clone(),
        events.clone(),
        frames_per_second,
    );
    let _encoder_handle = spawn_encoder(
        socket,
        running,
        active_peer,
        force_keyframe,
        capture_slot,
        events,
        frames_per_second,
        bitrate_bps,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_aspect_ratio_and_even_dimensions() {
        assert_eq!(fit_capture_dimensions(1920, 1080), (1280, 720));
        assert_eq!(fit_capture_dimensions(3456, 2234), (1112, 720));
        assert_eq!(fit_capture_dimensions(800, 600), (800, 600));
    }

    #[test]
    fn converts_avcc_nals_to_annex_b_shape() {
        let data = [
            0, 0, 0, 3, 0x65, 1, 2, //
            0, 0, 0, 2, 0x41, 3,
        ];
        assert!(avcc_contains_idr(&data).unwrap());
        assert!(read_avcc_length(&data, 0).is_ok());
        assert!(avcc_contains_idr(&data[..6]).is_err());
    }

    #[test]
    #[ignore = "requires an Apple hardware H.264 encoder"]
    fn hardware_frame_decodes_with_the_wire_decoder() {
        use openh264::{decoder::Decoder, formats::YUVSource};

        let surface =
            apple_cf::iosurface::IOSurface::create(1280, 720, u32::from_be_bytes(*b"BGRA"), 4)
                .expect("IOSurface allocation should succeed");
        let encoder = create_encoder(1280, 720, 30, 4_000_000).unwrap();
        let encoded = encoder.encode(&surface, (0, 30), true).unwrap();
        let (annex_b, is_keyframe) = avcc_frame_to_annex_b(&encoded).unwrap();
        assert!(is_keyframe);

        let mut decoder = Decoder::new().unwrap();
        let decoded = decoder
            .decode(&annex_b)
            .unwrap()
            .expect("hardware keyframe should decode");
        assert_eq!(decoded.dimensions(), (1280, 720));
    }
}
