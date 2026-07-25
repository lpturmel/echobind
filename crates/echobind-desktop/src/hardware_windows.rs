use super::{
    ensure_software_capture_available, spawn_capture, spawn_encoder, CaptureSlot, SessionEvent,
    VideoResolution,
};
use echobind_core::{
    protocol::{Packet, MAX_DATAGRAM_SIZE},
    video::fragment_video_frame,
};
use libloading::Library;
use moq_nvenc::sys::nvEncodeAPI::{
    NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION, NVENCAPI_VERSION, NVENCSTATUS,
    NV_ENCODE_API_FUNCTION_LIST, NV_ENCODE_API_FUNCTION_LIST_VER, NV_ENC_BUFFER_FORMAT,
    NV_ENC_BUFFER_USAGE, NV_ENC_CODEC_H264_GUID, NV_ENC_CONFIG_VER, NV_ENC_CREATE_BITSTREAM_BUFFER,
    NV_ENC_CREATE_BITSTREAM_BUFFER_VER, NV_ENC_DEVICE_TYPE, NV_ENC_H264_PROFILE_HIGH_GUID,
    NV_ENC_INITIALIZE_PARAMS, NV_ENC_INITIALIZE_PARAMS_VER, NV_ENC_INPUT_RESOURCE_TYPE,
    NV_ENC_LOCK_BITSTREAM, NV_ENC_LOCK_BITSTREAM_VER, NV_ENC_MAP_INPUT_RESOURCE,
    NV_ENC_MAP_INPUT_RESOURCE_VER, NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS,
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER, NV_ENC_PARAMS_RC_MODE, NV_ENC_PIC_FLAGS,
    NV_ENC_PIC_PARAMS, NV_ENC_PIC_PARAMS_VER, NV_ENC_PIC_STRUCT, NV_ENC_PIC_TYPE,
    NV_ENC_PRESET_CONFIG, NV_ENC_PRESET_CONFIG_VER, NV_ENC_PRESET_P1_GUID,
    NV_ENC_REGISTER_RESOURCE, NV_ENC_REGISTER_RESOURCE_VER, NV_ENC_TUNING_INFO,
};
use std::{
    ffi::c_void,
    mem::ManuallyDrop,
    net::{SocketAddr, UdpSocket},
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tracing::warn;
use windows::{
    core::Interface,
    Win32::{
        Foundation::{RECT, TRUE},
        Graphics::{
            Direct3D11::{
                ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext,
                ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
                ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET, D3D11_TEX2D_VPIV,
                D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
                D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
                D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
                D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
            },
            Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC},
        },
    },
};
use windows_capture::{
    capture::{Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    monitor::Monitor,
    settings::{ColorFormat, CursorCaptureSettings, DrawBorderSettings, Settings},
};

type NvencCreateInstance = unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
type NvencGetMaxVersion = unsafe extern "C" fn(*mut u32) -> NVENCSTATUS;

struct CaptureFlags {
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    force_keyframe: Arc<AtomicBool>,
    encoded_frames: mpsc::SyncSender<EncodedHardwareFrame>,
    events: mpsc::Sender<SessionEvent>,
    frames_per_second: u32,
    bitrate_bps: u32,
    width: u32,
    height: u32,
}

struct HardwareCapture {
    device_context: ID3D11DeviceContext,
    encoder: NvencEncoder,
    scaler: Option<D3dScaler>,
    output_texture: ID3D11Texture2D,
    flags: CaptureFlags,
    started: Instant,
    frame_interval: Duration,
    next_frame_at: Instant,
    frame_id: u64,
}

struct EncodedHardwareFrame {
    frame_id: u64,
    timestamp_us: u64,
    is_keyframe: bool,
    data: Vec<u8>,
}

struct D3dScaler {
    source_width: u32,
    source_height: u32,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output_view: ID3D11VideoProcessorOutputView,
}

struct NvencApi {
    _library: Library,
    functions: NV_ENCODE_API_FUNCTION_LIST,
}

struct NvencEncoder {
    api: NvencApi,
    encoder: *mut c_void,
    registered_resource: *mut c_void,
    bitstream: *mut c_void,
    width: u32,
    height: u32,
}

// NVENC owns no Rust references and is only used from the capture callback thread.
unsafe impl Send for NvencEncoder {}

#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_hardware_pipeline(
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    force_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    frames_per_second: u32,
    bitrate_bps: u32,
    width: u32,
    height: u32,
    resolution: VideoResolution,
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
            width,
            height,
        );
        if let Err(error) = result {
            if !running.load(Ordering::Relaxed) {
                return;
            }
            warn!("NVENC/D3D11 pipeline unavailable: {error}");
            let _ = events.send(SessionEvent::VideoBackend(format!(
                "OpenH264 software fallback (NVENC unavailable: {error})"
            )));
            if let Err(fallback_error) = start_software_fallback(
                socket,
                running,
                active_peer,
                force_keyframe,
                events.clone(),
                frames_per_second,
                bitrate_bps,
                resolution,
            ) {
                let _ = events.send(SessionEvent::Error(format!(
                    "NVENC/D3D11 failed ({error}); software fallback failed: {fallback_error}"
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
    width: u32,
    height: u32,
) -> Result<(), String> {
    // Keep the capture callback bounded. UDP packetization and the many send
    // syscalls needed for a large H.264 frame must never hold the Windows
    // Graphics Capture frame-pool callback.
    let (encoded_tx, encoded_rx) = mpsc::sync_channel(2);
    let sender_handle = spawn_hardware_sender(
        socket,
        running.clone(),
        active_peer.clone(),
        force_keyframe.clone(),
        events.clone(),
        encoded_rx,
    );
    let monitor =
        Monitor::primary().map_err(|error| format!("Unable to find primary display: {error}"))?;
    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::WithCursor,
        DrawBorderSettings::WithoutBorder,
        ColorFormat::Bgra8,
        CaptureFlags {
            active_peer,
            force_keyframe,
            encoded_frames: encoded_tx,
            events: events.clone(),
            frames_per_second,
            bitrate_bps,
            width,
            height,
        },
    );
    let control = HardwareCapture::start_free_threaded(settings)
        .map_err(|error| format!("Unable to start D3D11 screen capture: {error}"))?;
    let _ = events.send(SessionEvent::CaptureReady);

    while running.load(Ordering::Relaxed) && !control.is_finished() {
        thread::sleep(Duration::from_millis(20));
    }

    let capture_result = if running.load(Ordering::Relaxed) {
        control
            .wait()
            .map_err(|error| format!("D3D11 screen capture stopped: {error}"))
    } else {
        control
            .stop()
            .map_err(|error| format!("Unable to stop D3D11 screen capture: {error}"))
    };
    let _ = sender_handle.join();
    capture_result
}

impl GraphicsCaptureApiHandler for HardwareCapture {
    type Flags = CaptureFlags;
    type Error = String;

    fn new(context: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let output_texture =
            create_output_texture(&context.device, context.flags.width, context.flags.height)?;
        let encoder = NvencEncoder::new(
            &context.device,
            &output_texture,
            context.flags.width,
            context.flags.height,
            context.flags.frames_per_second,
            context.flags.bitrate_bps,
        )?;
        let _ = context.flags.events.send(SessionEvent::VideoBackend(
            "NVIDIA NVENC H.264 · D3D11 zero-copy".to_owned(),
        ));
        let frame_interval =
            Duration::from_secs_f64(1.0 / f64::from(context.flags.frames_per_second.max(1)));

        Ok(Self {
            device_context: context.device_context,
            encoder,
            scaler: None,
            output_texture,
            flags: context.flags,
            started: Instant::now(),
            frame_interval,
            next_frame_at: Instant::now(),
            frame_id: 0,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.flags.active_peer.lock().unwrap().is_none() {
            self.next_frame_at = Instant::now();
            return Ok(());
        }
        let now = Instant::now();
        if now + Duration::from_millis(1) < self.next_frame_at {
            return Ok(());
        }
        self.next_frame_at += self.frame_interval;
        if self.next_frame_at <= now {
            self.next_frame_at = now + self.frame_interval;
        }

        // The capture texture never leaves GPU memory. CopyResource is used at
        // native size; the D3D11 video processor performs downscaling otherwise.
        let source_texture = unsafe { frame.as_raw_texture() };
        if frame.width() == self.flags.width && frame.height() == self.flags.height {
            unsafe {
                self.device_context
                    .CopyResource(&self.output_texture, source_texture);
            }
        } else {
            let scaler_needs_rebuild = self.scaler.as_ref().is_none_or(|scaler| {
                scaler.source_width != frame.width() || scaler.source_height != frame.height()
            });
            if scaler_needs_rebuild {
                self.scaler = Some(D3dScaler::new(
                    &self.device_context,
                    &self.output_texture,
                    frame.width(),
                    frame.height(),
                    self.flags.width,
                    self.flags.height,
                    self.flags.frames_per_second,
                )?);
            }
            self.scaler
                .as_ref()
                .expect("scaler was initialized")
                .scale(source_texture)?;
        }
        // Submit the copy/scale before NVENC maps the DirectX resource. This is
        // asynchronous; NVENC performs the required GPU-side synchronization.
        unsafe {
            self.device_context.Flush();
        }

        let force_idr = self.flags.force_keyframe.swap(false, Ordering::Relaxed);
        let Some((encoded, is_keyframe)) = self.encoder.encode(force_idr)? else {
            return Ok(());
        };
        let timestamp_us = self.started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        let frame = EncodedHardwareFrame {
            frame_id: self.frame_id,
            timestamp_us,
            is_keyframe,
            data: encoded,
        };
        self.frame_id = self.frame_id.wrapping_add(1);
        if self.flags.encoded_frames.try_send(frame).is_err() {
            // The sender is behind. Drop immediately and make the next
            // successfully queued frame independently decodable.
            self.flags.force_keyframe.store(true, Ordering::Release);
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Err("The captured display was closed".to_owned())
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_hardware_sender(
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    force_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    encoded_frames: mpsc::Receiver<EncodedHardwareFrame>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut packet = Vec::with_capacity(MAX_DATAGRAM_SIZE);
        let mut stats_started = Instant::now();
        let mut stats_frames = 0_u64;
        let mut stats_bytes = 0_u64;

        while running.load(Ordering::Relaxed) {
            let frame = match encoded_frames.recv_timeout(Duration::from_millis(20)) {
                Ok(frame) => frame,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    report_sender_stats(
                        &events,
                        &mut stats_started,
                        &mut stats_frames,
                        &mut stats_bytes,
                    );
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let Some(peer) = *active_peer.lock().unwrap() else {
                continue;
            };
            let fragments = match fragment_video_frame(
                frame.frame_id,
                frame.timestamp_us,
                frame.is_keyframe,
                &frame.data,
            ) {
                Ok(fragments) => fragments,
                Err(error) => {
                    let _ = events.send(SessionEvent::Error(format!(
                        "NVENC frame cannot be packetized: {error}"
                    )));
                    force_keyframe.store(true, Ordering::Release);
                    continue;
                }
            };

            let mut frame_sent = true;
            for fragment in fragments {
                Packet::Video(fragment).encode(&mut packet);
                if let Err(error) = socket.send_to(&packet, peer) {
                    warn!("Video send to {peer} failed: {error}");
                    frame_sent = false;
                    force_keyframe.store(true, Ordering::Release);
                    break;
                }
                stats_bytes = stats_bytes.saturating_add(packet.len() as u64);
            }
            if frame_sent {
                stats_frames = stats_frames.saturating_add(1);
            }
            report_sender_stats(
                &events,
                &mut stats_started,
                &mut stats_frames,
                &mut stats_bytes,
            );
        }
    })
}

fn report_sender_stats(
    events: &mpsc::Sender<SessionEvent>,
    stats_started: &mut Instant,
    stats_frames: &mut u64,
    stats_bytes: &mut u64,
) {
    let elapsed = stats_started.elapsed();
    if elapsed < Duration::from_secs(1) {
        return;
    }
    let seconds = elapsed.as_secs_f32();
    let _ = events.send(SessionEvent::Stats {
        fps: *stats_frames as f32 / seconds,
        megabits_per_second: *stats_bytes as f32 * 8.0 / seconds / 1_000_000.0,
    });
    *stats_started = Instant::now();
    *stats_frames = 0;
    *stats_bytes = 0;
}

fn create_output_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, String> {
    let description = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        // NVENC accepts externally allocated DirectX textures through
        // nvEncRegisterResource and does not require the Direct3D video-encoder
        // bind flag. That flag targets the D3D11.1 video-encoder API and some
        // drivers reject it for BGRA textures with E_INVALIDARG. Render-target
        // binding is retained because the D3D11 video processor writes here
        // when downscaling.
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe {
        device
            .CreateTexture2D(&description, None, Some(&mut texture))
            .map_err(|error| format!("Unable to create NVENC input texture: {error}"))?;
    }
    texture.ok_or_else(|| "D3D11 returned no NVENC input texture".to_owned())
}

impl D3dScaler {
    #[allow(clippy::too_many_arguments)]
    fn new(
        device_context: &ID3D11DeviceContext,
        output_texture: &ID3D11Texture2D,
        source_width: u32,
        source_height: u32,
        output_width: u32,
        output_height: u32,
        frames_per_second: u32,
    ) -> Result<Self, String> {
        let device = unsafe {
            device_context
                .GetDevice()
                .map_err(|error| format!("D3D11 context returned no device: {error}"))?
        };
        let video_device: ID3D11VideoDevice = device
            .cast()
            .map_err(|error| format!("D3D11 video processing is unavailable: {error}"))?;
        let video_context: ID3D11VideoContext = device_context
            .cast()
            .map_err(|error| format!("D3D11 video context is unavailable: {error}"))?;
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: frames_per_second,
                Denominator: 1,
            },
            InputWidth: source_width,
            InputHeight: source_height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: frames_per_second,
                Denominator: 1,
            },
            OutputWidth: output_width,
            OutputHeight: output_height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe {
            video_device
                .CreateVideoProcessorEnumerator(&content)
                .map_err(|error| format!("Unable to create D3D11 video scaler: {error}"))?
        };
        let processor = unsafe {
            video_device
                .CreateVideoProcessor(&enumerator, 0)
                .map_err(|error| format!("Unable to create D3D11 video processor: {error}"))?
        };
        let output_description = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut output_view = None;
        unsafe {
            video_device
                .CreateVideoProcessorOutputView(
                    output_texture,
                    &enumerator,
                    &output_description,
                    Some(&mut output_view),
                )
                .map_err(|error| format!("Unable to create D3D11 scaler output view: {error}"))?;
        }
        let output_view =
            output_view.ok_or_else(|| "D3D11 returned no scaler output view".to_owned())?;

        let source_rect = RECT {
            left: 0,
            top: 0,
            right: source_width as i32,
            bottom: source_height as i32,
        };
        let output_rect = RECT {
            left: 0,
            top: 0,
            right: output_width as i32,
            bottom: output_height as i32,
        };
        unsafe {
            video_context.VideoProcessorSetStreamSourceRect(
                &processor,
                0,
                TRUE,
                Some(&source_rect),
            );
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, TRUE, Some(&output_rect));
            video_context.VideoProcessorSetOutputTargetRect(&processor, TRUE, Some(&output_rect));
        }

        Ok(Self {
            source_width,
            source_height,
            video_device,
            video_context,
            enumerator,
            processor,
            output_view,
        })
    }

    fn scale(&self, source_texture: &ID3D11Texture2D) -> Result<(), String> {
        let input_description = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut input_view = None;
        unsafe {
            self.video_device
                .CreateVideoProcessorInputView(
                    source_texture,
                    &self.enumerator,
                    &input_description,
                    Some(&mut input_view),
                )
                .map_err(|error| format!("Unable to create D3D11 scaler input view: {error}"))?;
        }
        let input_view =
            input_view.ok_or_else(|| "D3D11 returned no scaler input view".to_owned())?;
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: TRUE,
            pInputSurface: ManuallyDrop::new(Some(input_view)),
            ..Default::default()
        };
        let result = unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                &self.output_view,
                0,
                std::slice::from_ref(&stream),
            )
        }
        .map_err(|error| format!("D3D11 video scaling failed: {error}"));
        unsafe {
            ManuallyDrop::drop(&mut stream.pInputSurface);
        }
        result
    }
}

impl NvencApi {
    fn load() -> Result<Self, String> {
        let library = ["nvEncodeAPI64.dll", "nvEncodeAPI.dll"]
            .iter()
            .find_map(|name| unsafe { Library::new(name).ok() })
            .ok_or_else(|| {
                "NVIDIA driver encode library was not found; install a current NVIDIA driver"
                    .to_owned()
            })?;
        let (get_max_version, create_instance) = unsafe {
            let get_max_version = library
                .get::<NvencGetMaxVersion>(b"NvEncodeAPIGetMaxSupportedVersion\0")
                .map_err(|error| format!("NVENC version symbol is unavailable: {error}"))?;
            let create_instance = library
                .get::<NvencCreateInstance>(b"NvEncodeAPICreateInstance\0")
                .map_err(|error| format!("NVENC API symbol is unavailable: {error}"))?;
            (*get_max_version, *create_instance)
        };

        let mut supported_version = 0;
        nvenc_status(
            unsafe { get_max_version(&mut supported_version) },
            "query NVENC driver version",
        )?;
        let supported_major = supported_version >> 4;
        let supported_minor = supported_version & 0xF;
        if (supported_major, supported_minor) < (NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION) {
            return Err(format!(
                "NVIDIA driver supports NVENC {supported_major}.{supported_minor}, but {}.{} is required",
                NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION
            ));
        }

        let mut functions = NV_ENCODE_API_FUNCTION_LIST {
            version: NV_ENCODE_API_FUNCTION_LIST_VER,
            ..Default::default()
        };
        nvenc_status(
            unsafe { create_instance(&mut functions) },
            "initialize NVENC API",
        )?;
        Ok(Self {
            _library: library,
            functions,
        })
    }
}

impl NvencEncoder {
    fn new(
        device: &ID3D11Device,
        texture: &ID3D11Texture2D,
        width: u32,
        height: u32,
        frames_per_second: u32,
        bitrate_bps: u32,
    ) -> Result<Self, String> {
        let api = NvencApi::load()?;
        let mut result = Self {
            api,
            encoder: ptr::null_mut(),
            registered_resource: ptr::null_mut(),
            bitstream: ptr::null_mut(),
            width,
            height,
        };
        result.open(device)?;
        result.initialize(frames_per_second, bitrate_bps)?;
        result.register(texture)?;
        result.create_bitstream()?;
        Ok(result)
    }

    fn open(&mut self, device: &ID3D11Device) -> Result<(), String> {
        let open = required(
            self.api.functions.nvEncOpenEncodeSessionEx,
            "NvEncOpenEncodeSessionEx",
        )?;
        let mut params = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
            version: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
            deviceType: NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX,
            device: device.as_raw(),
            apiVersion: NVENCAPI_VERSION,
            ..Default::default()
        };
        nvenc_status(
            unsafe { open(&mut params, &mut self.encoder) },
            "open DirectX NVENC session",
        )
    }

    fn initialize(&mut self, frames_per_second: u32, bitrate_bps: u32) -> Result<(), String> {
        let get_preset = required(
            self.api.functions.nvEncGetEncodePresetConfigEx,
            "NvEncGetEncodePresetConfigEx",
        )?;
        let mut preset = NV_ENC_PRESET_CONFIG {
            version: NV_ENC_PRESET_CONFIG_VER,
            ..Default::default()
        };
        preset.presetCfg.version = NV_ENC_CONFIG_VER;
        nvenc_status(
            unsafe {
                get_preset(
                    self.encoder,
                    NV_ENC_CODEC_H264_GUID,
                    NV_ENC_PRESET_P1_GUID,
                    NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                    &mut preset,
                )
            },
            "load NVENC low-latency preset",
        )?;

        let mut config = preset.presetCfg;
        config.version = NV_ENC_CONFIG_VER;
        config.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
        config.gopLength = frames_per_second.max(1);
        config.frameIntervalP = 1;
        config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
        config.rcParams.averageBitRate = bitrate_bps;
        config.rcParams.maxBitRate = bitrate_bps;
        let frame_budget = bitrate_bps / frames_per_second.max(1);
        config.rcParams.vbvBufferSize = frame_budget.saturating_mul(2).max(64_000);
        config.rcParams.vbvInitialDelay = config.rcParams.vbvBufferSize;
        config.rcParams.set_enableAQ(0);
        config.rcParams.set_enableLookahead(0);
        config.rcParams.set_zeroReorderDelay(1);
        let mut h264 = unsafe { config.encodeCodecConfig.h264Config };
        h264.idrPeriod = frames_per_second.max(1);
        h264.sliceMode = 3;
        h264.sliceModeData = 1;
        h264.set_repeatSPSPPS(1);
        config.encodeCodecConfig.h264Config = h264;

        let mut initialize = NV_ENC_INITIALIZE_PARAMS {
            version: NV_ENC_INITIALIZE_PARAMS_VER,
            encodeGUID: NV_ENC_CODEC_H264_GUID,
            presetGUID: NV_ENC_PRESET_P1_GUID,
            encodeWidth: self.width,
            encodeHeight: self.height,
            darWidth: self.width,
            darHeight: self.height,
            frameRateNum: frames_per_second,
            frameRateDen: 1,
            enablePTD: 1,
            encodeConfig: &mut config,
            maxEncodeWidth: self.width,
            maxEncodeHeight: self.height,
            tuningInfo: NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            bufferFormat: NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
            ..Default::default()
        };
        let initialize_encoder = required(
            self.api.functions.nvEncInitializeEncoder,
            "NvEncInitializeEncoder",
        )?;
        nvenc_status(
            unsafe { initialize_encoder(self.encoder, &mut initialize) },
            "initialize NVENC H.264 encoder",
        )
    }

    fn register(&mut self, texture: &ID3D11Texture2D) -> Result<(), String> {
        let register_resource = required(
            self.api.functions.nvEncRegisterResource,
            "NvEncRegisterResource",
        )?;
        let mut resource = NV_ENC_REGISTER_RESOURCE {
            version: NV_ENC_REGISTER_RESOURCE_VER,
            resourceType: NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
            width: self.width,
            height: self.height,
            resourceToRegister: texture.as_raw(),
            bufferFormat: NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
            bufferUsage: NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
            ..Default::default()
        };
        nvenc_status(
            unsafe { register_resource(self.encoder, &mut resource) },
            "register D3D11 texture with NVENC",
        )?;
        self.registered_resource = resource.registeredResource;
        Ok(())
    }

    fn create_bitstream(&mut self) -> Result<(), String> {
        let create = required(
            self.api.functions.nvEncCreateBitstreamBuffer,
            "NvEncCreateBitstreamBuffer",
        )?;
        let mut bitstream = NV_ENC_CREATE_BITSTREAM_BUFFER {
            version: NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
            ..Default::default()
        };
        nvenc_status(
            unsafe { create(self.encoder, &mut bitstream) },
            "create NVENC bitstream buffer",
        )?;
        self.bitstream = bitstream.bitstreamBuffer;
        Ok(())
    }

    fn encode(&mut self, force_idr: bool) -> Result<Option<(Vec<u8>, bool)>, String> {
        let map_input = required(
            self.api.functions.nvEncMapInputResource,
            "NvEncMapInputResource",
        )?;
        let encode_picture = required(self.api.functions.nvEncEncodePicture, "NvEncEncodePicture")?;
        let unmap_input = required(
            self.api.functions.nvEncUnmapInputResource,
            "NvEncUnmapInputResource",
        )?;
        let lock_bitstream = required(self.api.functions.nvEncLockBitstream, "NvEncLockBitstream")?;
        let unlock = required(
            self.api.functions.nvEncUnlockBitstream,
            "NvEncUnlockBitstream",
        )?;
        let mut mapped = NV_ENC_MAP_INPUT_RESOURCE {
            version: NV_ENC_MAP_INPUT_RESOURCE_VER,
            registeredResource: self.registered_resource,
            ..Default::default()
        };
        nvenc_status(
            unsafe { map_input(self.encoder, &mut mapped) },
            "map D3D11 texture for NVENC",
        )?;

        let mut flags = 0;
        if force_idr {
            flags |= NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32;
            flags |= NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS as u32;
        }
        let mut picture = NV_ENC_PIC_PARAMS {
            version: NV_ENC_PIC_PARAMS_VER,
            inputWidth: self.width,
            inputHeight: self.height,
            encodePicFlags: flags,
            inputBuffer: mapped.mappedResource,
            outputBitstream: self.bitstream,
            bufferFmt: mapped.mappedBufferFmt,
            pictureStruct: NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
            ..Default::default()
        };
        let encode_status = unsafe { encode_picture(self.encoder, &mut picture) };
        let unmap_status = unsafe { unmap_input(self.encoder, mapped.mappedResource) };
        nvenc_status(encode_status, "encode NVENC frame")?;
        nvenc_status(unmap_status, "unmap NVENC input texture")?;

        let mut lock = NV_ENC_LOCK_BITSTREAM {
            version: NV_ENC_LOCK_BITSTREAM_VER,
            outputBitstream: self.bitstream,
            ..Default::default()
        };
        nvenc_status(
            unsafe { lock_bitstream(self.encoder, &mut lock) },
            "lock NVENC bitstream",
        )?;
        let data = if lock.bitstreamBufferPtr.is_null() || lock.bitstreamSizeInBytes == 0 {
            Vec::new()
        } else {
            unsafe {
                std::slice::from_raw_parts(
                    lock.bitstreamBufferPtr.cast::<u8>(),
                    lock.bitstreamSizeInBytes as usize,
                )
                .to_vec()
            }
        };
        let picture_type = lock.pictureType;
        nvenc_status(
            unsafe { unlock(self.encoder, self.bitstream) },
            "unlock NVENC bitstream",
        )?;
        if data.is_empty() {
            return Ok(None);
        }
        let is_keyframe = matches!(
            picture_type,
            NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR | NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I
        );
        Ok(Some((data, is_keyframe)))
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.bitstream.is_null() && !self.encoder.is_null() {
                if let Some(destroy) = self.api.functions.nvEncDestroyBitstreamBuffer {
                    let _ = destroy(self.encoder, self.bitstream);
                }
            }
            if !self.registered_resource.is_null() && !self.encoder.is_null() {
                if let Some(unregister) = self.api.functions.nvEncUnregisterResource {
                    let _ = unregister(self.encoder, self.registered_resource);
                }
            }
            if !self.encoder.is_null() {
                if let Some(destroy) = self.api.functions.nvEncDestroyEncoder {
                    let _ = destroy(self.encoder);
                }
            }
        }
    }
}

fn required<T: Copy>(function: Option<T>, name: &str) -> Result<T, String> {
    function.ok_or_else(|| format!("NVIDIA driver did not provide {name}"))
}

fn nvenc_status(status: NVENCSTATUS, operation: &str) -> Result<(), String> {
    if status == NVENCSTATUS::NV_ENC_SUCCESS {
        Ok(())
    } else {
        Err(format!("{operation} failed with {status:?}"))
    }
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
) -> Result<(), String> {
    ensure_software_capture_available()?;
    let capture_slot: CaptureSlot = Arc::new((Mutex::new(None), Condvar::new()));
    let _capture_handle = spawn_capture(
        running.clone(),
        capture_slot.clone(),
        events.clone(),
        frames_per_second,
        resolution,
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
        resolution,
    );
    Ok(())
}
