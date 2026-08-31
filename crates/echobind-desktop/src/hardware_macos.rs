use super::{
    ensure_software_capture_available, publish_server_stats, spawn_capture, spawn_encoder,
    video_pacing_batch_datagrams, CaptureSlot, DatagramPacer, PendingServerStats, SessionEvent,
    VideoResolution, VIDEO_SEND_STALE_AGE,
};
use echobind_core::{
    protocol::{Packet, ServerStats, MAX_DATAGRAM_SIZE},
    video::fragment_video_frame_with_datagram_size,
};
use screencapturekit::prelude::*;
use std::{
    ffi::c_void,
    net::{SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tracing::warn;
use videotoolbox::{ffi as vt, session::Codec};

const CAPTURE_WAIT: Duration = Duration::from_millis(30);
const ENCODE_OUTPUT_WAIT: Duration = Duration::from_millis(2);
const HARDWARE_ENCODER_IN_FLIGHT: usize = 4;

type SampleSlot = Arc<(Mutex<Option<(CMSampleBuffer, Instant)>>, Condvar)>;

struct HardwareEncodedFrame {
    data: Vec<u8>,
    sample_buffer: Option<CMSampleBuffer>,
    captured_at: Instant,
    encode_started: Instant,
    encoded_at: Instant,
}

struct HardwareEncodeContext {
    captured_at: Instant,
    encode_started: Instant,
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

#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_hardware_pipeline(
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    force_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    frames_per_second: u32,
    bitrate_bps: u32,
    resolution: VideoResolution,
    active_datagram_size: Arc<AtomicUsize>,
    pending_server_stats: PendingServerStats,
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
            resolution,
            active_datagram_size.clone(),
            pending_server_stats.clone(),
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
                resolution,
                active_datagram_size,
                pending_server_stats,
            ) {
                let _ = events.send(SessionEvent::Error(format!(
                    "Hardware H.264 failed ({error}); software fallback failed: {fallback_error}"
                )));
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn run_hardware_pipeline(
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    force_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    frames_per_second: u32,
    bitrate_bps: u32,
    resolution: VideoResolution,
    active_datagram_size: Arc<AtomicUsize>,
    pending_server_stats: PendingServerStats,
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
    let (max_width, max_height) = resolution
        .dimensions()
        .unwrap_or((display.width(), display.height()));
    let (width, height) =
        fit_capture_dimensions(display.width(), display.height(), max_width, max_height);
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
        // Keep enough IOSurfaces available for VideoToolbox to overlap capture
        // and encode. The application slot below still retains only the newest
        // not-yet-submitted sample, so this does not create a latency backlog.
        .with_queue_depth(HARDWARE_ENCODER_IN_FLIGHT as u32)
        .with_minimum_frame_interval(&frame_interval);

    let sample_slot: SampleSlot = Arc::new((Mutex::new(None), Condvar::new()));
    let callback_slot = sample_slot.clone();
    let mut stream = SCStream::new(&filter, &configuration);
    stream.add_output_handler(
        move |sample: CMSampleBuffer, _| {
            let (slot, available) = &*callback_slot;
            *slot.lock().unwrap() = Some((sample, Instant::now()));
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

    let mut sequence = 0_u64;
    let mut submitted_any = false;
    let mut outstanding = 0_usize;
    let mut output = HardwareOutputState::new(bitrate_bps);

    while running.load(Ordering::Relaxed) {
        let mut made_progress = false;
        while outstanding > 0 {
            let Some(encoded) = encoder.try_receive()? else {
                break;
            };
            outstanding -= 1;
            output.process(
                encoded,
                &socket,
                &active_peer,
                &force_keyframe,
                &active_datagram_size,
            )?;
            made_progress = true;
        }

        if outstanding < HARDWARE_ENCODER_IN_FLIGHT {
            let sample = sample_slot.0.lock().unwrap().take();
            if let Some((sample, captured_at)) = sample {
                made_progress = true;
                output.source_frames = output.source_frames.saturating_add(1);
                if captured_at.elapsed() > VIDEO_SEND_STALE_AGE {
                    output.stale_frames = output.stale_frames.saturating_add(1);
                    force_keyframe.store(true, Ordering::Release);
                } else if active_peer.lock().unwrap().is_some() {
                    let pixel_buffer = sample.image_buffer().ok_or_else(|| {
                        "ScreenCaptureKit returned a frame without an image buffer".to_owned()
                    })?;
                    let surface = pixel_buffer.io_surface().ok_or_else(|| {
                        "ScreenCaptureKit returned a frame without an IOSurface".to_owned()
                    })?;
                    let request_keyframe = if submitted_any {
                        force_keyframe.swap(false, Ordering::AcqRel)
                    } else {
                        force_keyframe.store(false, Ordering::Release);
                        true
                    };
                    encoder.submit(
                        &surface,
                        (sequence as i64, frames_per_second as i32),
                        request_keyframe,
                        captured_at,
                    )?;
                    sequence = sequence.wrapping_add(1);
                    submitted_any = true;
                    outstanding += 1;
                }
            }
        }

        output.report(&events, &pending_server_stats);
        if made_progress {
            continue;
        }

        if outstanding > 0 {
            if let Some(encoded) = encoder.receive_timeout(ENCODE_OUTPUT_WAIT)? {
                outstanding -= 1;
                output.process(
                    encoded,
                    &socket,
                    &active_peer,
                    &force_keyframe,
                    &active_datagram_size,
                )?;
            }
        } else {
            let (slot, available) = &*sample_slot;
            let guard = slot.lock().unwrap();
            let _ = available
                .wait_timeout_while(guard, CAPTURE_WAIT, |frame| {
                    frame.is_none() && running.load(Ordering::Relaxed)
                })
                .unwrap();
        }
    }

    // Complete only at end-of-stream. Calling this for every submitted frame
    // serializes VideoToolbox and destroys throughput under GPU contention.
    encoder.complete_frames()?;
    while outstanding > 0 {
        match encoder.receive_timeout(CAPTURE_WAIT)? {
            Some(_) => outstanding -= 1,
            None => break,
        }
    }
    let _ = stream.stop_capture();
    Ok(())
}

struct HardwareOutputState {
    started: Instant,
    stats_started: Instant,
    frame_id: u64,
    packet: Vec<u8>,
    pacer: DatagramPacer,
    source_frames: u64,
    stale_frames: u64,
    frames: u64,
    bytes: u64,
    capture_us: u64,
    encode_us: u64,
    packetize_us: u64,
    send_us: u64,
    send_failures: u64,
}

impl HardwareOutputState {
    fn new(bitrate_bps: u32) -> Self {
        let now = Instant::now();
        Self {
            started: now,
            stats_started: now,
            frame_id: 0,
            packet: Vec::with_capacity(MAX_DATAGRAM_SIZE),
            pacer: DatagramPacer::new(bitrate_bps),
            source_frames: 0,
            stale_frames: 0,
            frames: 0,
            bytes: 0,
            capture_us: 0,
            encode_us: 0,
            packetize_us: 0,
            send_us: 0,
            send_failures: 0,
        }
    }

    fn process(
        &mut self,
        encoded: HardwareEncodedFrame,
        socket: &UdpSocket,
        active_peer: &Mutex<Option<SocketAddr>>,
        force_keyframe: &AtomicBool,
        active_datagram_size: &AtomicUsize,
    ) -> Result<(), String> {
        if encoded.data.is_empty() {
            // Real-time VideoToolbox may drop a frame rather than block. It was
            // never added to our transport sequence, but the next submitted
            // frame must be an IDR so the receiver cannot inherit an uncertain
            // reference chain.
            force_keyframe.store(true, Ordering::Release);
            return Ok(());
        }
        let (annex_b, is_keyframe) = avcc_frame_to_annex_b(&encoded)?;
        if annex_b.is_empty() {
            force_keyframe.store(true, Ordering::Release);
            return Ok(());
        }
        let capture_elapsed = encoded.captured_at.elapsed();
        if capture_elapsed > VIDEO_SEND_STALE_AGE {
            self.stale_frames = self.stale_frames.saturating_add(1);
            force_keyframe.store(true, Ordering::Release);
            return Ok(());
        }
        let Some(peer) = *active_peer.lock().unwrap() else {
            force_keyframe.store(true, Ordering::Release);
            return Ok(());
        };

        let timestamp_us = encoded
            .captured_at
            .saturating_duration_since(self.started)
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        let datagram_size = active_datagram_size.load(Ordering::Acquire);
        let packetize_started = Instant::now();
        let fragments = fragment_video_frame_with_datagram_size(
            self.frame_id,
            timestamp_us,
            is_keyframe,
            &annex_b,
            datagram_size,
        )
        .map_err(|error| format!("Hardware frame cannot be packetized: {error}"))?;
        let packetize_us = packetize_started
            .elapsed()
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;

        let send_started = Instant::now();
        let mut frame_sent = true;
        let pacing_batch_datagrams = video_pacing_batch_datagrams(datagram_size);
        self.pacer.begin_frame();
        for (fragment_index, fragment) in fragments.into_iter().enumerate() {
            if fragment_index > 0 && fragment_index % pacing_batch_datagrams == 0 {
                self.pacer.wait();
            }
            Packet::Video(fragment).encode(&mut self.packet);
            if let Err(error) = socket.send_to(&self.packet, peer) {
                warn!("Hardware video send to {peer} failed: {error}");
                self.send_failures = self.send_failures.saturating_add(1);
                frame_sent = false;
                force_keyframe.store(true, Ordering::Release);
                break;
            }
            self.pacer.account(self.packet.len());
            self.bytes = self.bytes.saturating_add(self.packet.len() as u64);
        }
        self.frame_id = self.frame_id.wrapping_add(1);
        if frame_sent {
            self.frames = self.frames.saturating_add(1);
            self.capture_us = self.capture_us.saturating_add(
                encoded
                    .encode_started
                    .saturating_duration_since(encoded.captured_at)
                    .as_micros()
                    .min(u128::from(u64::MAX)) as u64,
            );
            self.encode_us = self.encode_us.saturating_add(
                encoded
                    .encoded_at
                    .saturating_duration_since(encoded.encode_started)
                    .as_micros()
                    .min(u128::from(u64::MAX)) as u64,
            );
            self.packetize_us = self.packetize_us.saturating_add(packetize_us);
            self.send_us =
                self.send_us.saturating_add(
                    send_started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64,
                );
        }
        Ok(())
    }

    fn report(&mut self, events: &mpsc::Sender<SessionEvent>, pending: &PendingServerStats) {
        let elapsed = self.stats_started.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let seconds = elapsed.as_secs_f32();
        publish_server_stats(
            events,
            pending,
            ServerStats {
                fps: self.frames as f32 / seconds,
                source_fps: self.source_frames as f32 / seconds,
                megabits_per_second: self.bytes as f32 * 8.0 / seconds / 1_000_000.0,
                capture_ms: super::average_milliseconds(self.capture_us, self.frames),
                encode_ms: super::average_milliseconds(self.encode_us, self.frames),
                send_ms: super::average_milliseconds(self.send_us, self.frames),
                encode_queue_ms: super::average_milliseconds(self.packetize_us, self.frames),
                stale_frames: self.stale_frames,
                video_send_failures: self.send_failures,
                ..ServerStats::default()
            },
        );
        self.stats_started = Instant::now();
        self.source_frames = 0;
        self.stale_frames = 0;
        self.frames = 0;
        self.bytes = 0;
        self.capture_us = 0;
        self.encode_us = 0;
        self.packetize_us = 0;
        self.send_us = 0;
        self.send_failures = 0;
    }
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
                .set_i32(vt::kVTCompressionPropertyKey_MaxKeyFrameInterval, i32::MAX)
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

    fn submit(
        &self,
        surface: &apple_cf::iosurface::IOSurface,
        presentation_time: (i64, i32),
        force_keyframe: bool,
        captured_at: Instant,
    ) -> Result<(), String> {
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
        let encode_context = Box::into_raw(Box::new(HardwareEncodeContext {
            captured_at,
            encode_started: Instant::now(),
        }));
        let mut info_flags = 0_u32;
        let status = unsafe {
            vt::VTCompressionSessionEncodeFrame(
                self.session,
                pixel_buffer,
                vt::CMTime::new(presentation_time.0, presentation_time.1),
                vt::CMTime::INVALID,
                frame_properties_ptr,
                encode_context.cast::<c_void>(),
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
            unsafe {
                drop(Box::from_raw(encode_context));
            }
            return Err(format!(
                "VideoToolbox rejected a frame (status {status}, flags {info_flags})"
            ));
        }
        Ok(())
    }

    fn try_receive(&self) -> Result<Option<HardwareEncodedFrame>, String> {
        match self
            .state
            .output_rx
            .lock()
            .map_err(|_| "VideoToolbox output queue was poisoned".to_owned())?
            .try_recv()
        {
            Ok(result) => result.map(Some),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("VideoToolbox output callback disconnected".to_owned())
            }
        }
    }

    fn receive_timeout(&self, timeout: Duration) -> Result<Option<HardwareEncodedFrame>, String> {
        match self
            .state
            .output_rx
            .lock()
            .map_err(|_| "VideoToolbox output queue was poisoned".to_owned())?
            .recv_timeout(timeout)
        {
            Ok(result) => result.map(Some),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("VideoToolbox output callback disconnected".to_owned())
            }
        }
    }

    fn complete_frames(&self) -> Result<(), String> {
        let status =
            unsafe { vt::VTCompressionSessionCompleteFrames(self.session, vt::CMTime::INVALID) };
        if status != 0 {
            return Err(format!(
                "VideoToolbox could not complete pending frames (status {status})"
            ));
        }
        Ok(())
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
    source_frame_ref_con: *mut c_void,
    status: vt::OSStatus,
    info_flags: vt::VTEncodeInfoFlags,
    sample_buffer: vt::CMSampleBufferRef,
) {
    let state = unsafe { Arc::from_raw(output_callback_ref_con.cast::<EncoderState>()) };
    let callback_state = state.clone();
    std::mem::forget(state);

    let context = if source_frame_ref_con.is_null() {
        None
    } else {
        Some(unsafe { Box::from_raw(source_frame_ref_con.cast::<HardwareEncodeContext>()) })
    };
    let (captured_at, encode_started) = context.as_ref().map_or_else(
        || {
            let now = Instant::now();
            (now, now)
        },
        |context| (context.captured_at, context.encode_started),
    );

    let result = if status != 0 {
        Err(format!(
            "VideoToolbox callback reported encoding status {status} (flags {info_flags})"
        ))
    } else if sample_buffer.is_null() {
        Ok(HardwareEncodedFrame {
            data: Vec::new(),
            sample_buffer: None,
            captured_at,
            encode_started,
            encoded_at: Instant::now(),
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
                    captured_at,
                    encode_started,
                    encoded_at: Instant::now(),
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

fn fit_capture_dimensions(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (max_width, max_height);
    }
    let scale = (max_width as f64 / width as f64)
        .min(max_height as f64 / height as f64)
        .min(1.0);
    let fitted_width = ((width as f64 * scale).floor() as u32).max(2) & !1;
    let fitted_height = ((height as f64 * scale).floor() as u32).max(2) & !1;
    (fitted_width, fitted_height)
}

#[allow(clippy::too_many_arguments)]
fn start_software_fallback(
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    force_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    frames_per_second: u32,
    bitrate_bps: u32,
    resolution: VideoResolution,
    active_datagram_size: Arc<AtomicUsize>,
    pending_server_stats: PendingServerStats,
) -> Result<(), String> {
    ensure_software_capture_available()?;
    let capture_slot: CaptureSlot = Arc::new((Mutex::new(None), Condvar::new()));
    let capture_handle = spawn_capture(
        running.clone(),
        capture_slot.clone(),
        events.clone(),
        frames_per_second,
        resolution,
    );
    let encoder_handle = spawn_encoder(
        socket,
        running,
        active_peer,
        force_keyframe,
        capture_slot,
        events,
        frames_per_second,
        bitrate_bps,
        resolution,
        active_datagram_size,
        pending_server_stats,
    );
    let _ = capture_handle.join();
    let _ = encoder_handle.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_aspect_ratio_and_even_dimensions() {
        assert_eq!(fit_capture_dimensions(1920, 1080, 1280, 720), (1280, 720));
        assert_eq!(fit_capture_dimensions(3456, 2234, 1280, 720), (1112, 720));
        assert_eq!(fit_capture_dimensions(800, 600, 1920, 1080), (800, 600));
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
        encoder
            .submit(&surface, (0, 30), true, Instant::now())
            .unwrap();
        encoder.complete_frames().unwrap();
        let encoded = encoder
            .receive_timeout(Duration::from_secs(1))
            .unwrap()
            .expect("hardware encoder should return a frame");
        let (annex_b, is_keyframe) = avcc_frame_to_annex_b(&encoded).unwrap();
        assert!(is_keyframe);

        let mut decoder = Decoder::new().unwrap();
        let decoded = decoder
            .decode(&annex_b)
            .unwrap()
            .expect("hardware keyframe should decode");
        assert_eq!(decoded.dimensions(), (1280, 720));
    }

    #[test]
    #[ignore = "requires Apple hardware H.264 encode and decode"]
    fn hardware_frame_decodes_to_nv12_iosurface() {
        use super::super::{
            decoder_macos, ClientMetrics, DecodeQueue, DisplayFrameData, LatestFrame,
            ReceivedVideoFrame,
        };
        use echobind_core::video::VideoFrame;
        use std::sync::atomic::AtomicU64;

        let surface =
            apple_cf::iosurface::IOSurface::create(1280, 720, u32::from_be_bytes(*b"BGRA"), 4)
                .expect("IOSurface allocation should succeed");
        let encoder = create_encoder(1280, 720, 30, 4_000_000).unwrap();
        encoder
            .submit(&surface, (0, 30), true, Instant::now())
            .unwrap();
        encoder.complete_frames().unwrap();
        let encoded = encoder
            .receive_timeout(Duration::from_secs(1))
            .unwrap()
            .expect("hardware encoder should return a frame");
        let (annex_b, is_keyframe) = avcc_frame_to_annex_b(&encoded).unwrap();
        assert!(is_keyframe);

        let running = Arc::new(AtomicBool::new(true));
        let queue = DecodeQueue::new();
        let latest: LatestFrame = Arc::new(Mutex::new(None));
        let metrics = Arc::new(ClientMetrics::default());
        let needs_keyframe = Arc::new(AtomicBool::new(false));
        let (events, _event_rx) = mpsc::channel();
        let decoder_running = running.clone();
        let decoder_queue = queue.clone();
        let decoder_latest = latest.clone();
        let decoder_metrics = metrics.clone();
        let decoder_needs_keyframe = needs_keyframe.clone();
        let decoder = thread::spawn(move || {
            decoder_macos::run_decoder(
                decoder_running,
                decoder_queue,
                decoder_latest,
                decoder_metrics,
                decoder_needs_keyframe,
                events,
                Arc::new(|| {}),
            )
        });
        let _ = queue.push(ReceivedVideoFrame {
            frame: VideoFrame {
                frame_id: 0,
                timestamp_us: 0,
                is_keyframe: true,
                payload: annex_b,
            },
            received_at: Instant::now(),
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while latest.lock().unwrap().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        running.store(false, Ordering::Release);
        queue.inner.1.notify_all();
        decoder.join().unwrap().unwrap();

        let decoded = latest
            .lock()
            .unwrap()
            .take()
            .expect("VideoToolbox should output a frame");
        match &decoded.data {
            DisplayFrameData::Nv12(pixel_buffer) => {
                assert_eq!(pixel_buffer.plane_count(), 2);
                assert!(pixel_buffer.io_surface().is_some());

                let instance = eframe::wgpu::Instance::new(
                    eframe::wgpu::InstanceDescriptor::new_without_display_handle(),
                );
                let adapter = pollster::block_on(instance.request_adapter(
                    &eframe::wgpu::RequestAdapterOptions {
                        power_preference: eframe::wgpu::PowerPreference::HighPerformance,
                        force_fallback_adapter: false,
                        compatible_surface: None,
                    },
                ))
                .expect("Metal adapter should be available");
                let (device, _queue) = pollster::block_on(
                    adapter.request_device(&eframe::wgpu::DeviceDescriptor::default()),
                )
                .expect("Metal device should be available");
                let mut renderer = crate::video_renderer_macos::MacVideoRenderer::new(
                    &device,
                    eframe::wgpu::TextureFormat::Bgra8Unorm,
                    Arc::new(AtomicU64::new(0)),
                    Arc::new(AtomicU64::new(0)),
                )
                .expect("Metal renderer should initialize");
                renderer
                    .set_frame(&device, pixel_buffer.clone(), decoded.published_at)
                    .expect("NV12 IOSurface should import into Metal without copying");
            }
            DisplayFrameData::Rgba(_) => panic!("expected zero-copy NV12 output"),
        }
    }

    #[test]
    #[ignore = "requires an Apple hardware H.264 encoder"]
    fn hardware_encoder_keeps_four_frames_in_flight_at_120_fps() {
        const FRAME_COUNT: usize = 240;

        let surface =
            apple_cf::iosurface::IOSurface::create(1920, 1080, u32::from_be_bytes(*b"BGRA"), 4)
                .expect("IOSurface allocation should succeed");
        let encoder = create_encoder(1920, 1080, 120, 40_000_000).unwrap();
        let started = Instant::now();
        let mut submitted = 0_usize;
        let mut completed = 0_usize;
        let mut in_flight = 0_usize;

        while submitted < FRAME_COUNT {
            while submitted < FRAME_COUNT && in_flight < HARDWARE_ENCODER_IN_FLIGHT {
                encoder
                    .submit(
                        &surface,
                        (submitted as i64, 120),
                        submitted == 0,
                        Instant::now(),
                    )
                    .unwrap();
                submitted += 1;
                in_flight += 1;
            }
            let frame = encoder
                .receive_timeout(Duration::from_secs(1))
                .unwrap()
                .expect("hardware encoder should make forward progress");
            assert!(
                !frame.data.is_empty(),
                "VideoToolbox dropped frame {completed}"
            );
            completed += 1;
            in_flight -= 1;
        }

        encoder.complete_frames().unwrap();
        while completed < submitted {
            let frame = encoder
                .receive_timeout(Duration::from_secs(1))
                .unwrap()
                .expect("hardware encoder should drain every submitted frame");
            assert!(
                !frame.data.is_empty(),
                "VideoToolbox dropped frame {completed}"
            );
            completed += 1;
        }
        let elapsed = started.elapsed();
        let measured_fps = completed as f64 / elapsed.as_secs_f64();
        eprintln!(
            "encoded {completed} 1080p frames in {:.3}s ({measured_fps:.1} FPS)",
            elapsed.as_secs_f64()
        );
        assert_eq!(completed, FRAME_COUNT);
        assert!(
            measured_fps >= 120.0,
            "hardware encoder produced only {measured_fps:.1} FPS"
        );
    }
}
