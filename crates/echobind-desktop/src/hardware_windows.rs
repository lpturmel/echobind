use super::{
    ensure_software_capture_available, spawn_capture, spawn_encoder, CaptureSlot, SessionEvent,
    VideoResolution, VIDEO_SEND_STALE_AGE,
};
use echobind_core::{
    protocol::{Packet, MAX_DATAGRAM_SIZE},
    video::fragment_video_frame_with_datagram_size,
};
use libloading::Library;
use moq_nvenc::sys::nvEncodeAPI::{
    NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION, NVENCAPI_VERSION, NVENCSTATUS,
    NV_ENCODE_API_FUNCTION_LIST, NV_ENCODE_API_FUNCTION_LIST_VER, NV_ENC_BUFFER_FORMAT,
    NV_ENC_BUFFER_USAGE, NV_ENC_CODEC_H264_GUID, NV_ENC_CONFIG_VER, NV_ENC_CREATE_BITSTREAM_BUFFER,
    NV_ENC_CREATE_BITSTREAM_BUFFER_VER, NV_ENC_DEVICE_TYPE, NV_ENC_EVENT_PARAMS,
    NV_ENC_EVENT_PARAMS_VER, NV_ENC_H264_PROFILE_HIGH_GUID, NV_ENC_INITIALIZE_PARAMS,
    NV_ENC_INITIALIZE_PARAMS_VER, NV_ENC_INPUT_RESOURCE_TYPE, NV_ENC_LOCK_BITSTREAM,
    NV_ENC_LOCK_BITSTREAM_VER, NV_ENC_MAP_INPUT_RESOURCE, NV_ENC_MAP_INPUT_RESOURCE_VER,
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
    NV_ENC_PARAMS_RC_MODE, NV_ENC_PIC_FLAGS, NV_ENC_PIC_PARAMS, NV_ENC_PIC_PARAMS_VER,
    NV_ENC_PIC_STRUCT, NV_ENC_PIC_TYPE, NV_ENC_PRESET_CONFIG, NV_ENC_PRESET_CONFIG_VER,
    NV_ENC_PRESET_P1_GUID, NV_ENC_REGISTER_RESOURCE, NV_ENC_REGISTER_RESOURCE_VER,
    NV_ENC_TUNING_INFO, NV_ENC_VUI_COLOR_PRIMARIES, NV_ENC_VUI_MATRIX_COEFFS,
    NV_ENC_VUI_TRANSFER_CHARACTERISTIC, NV_ENC_VUI_VIDEO_FORMAT,
};
use std::{
    ffi::c_void,
    mem::ManuallyDrop,
    net::{SocketAddr, UdpSocket},
    ptr,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tracing::warn;
use windows::{
    core::Interface,
    Win32::{
        Foundation::{CloseHandle, BOOL, HANDLE, HMODULE, RECT, TRUE, WAIT_OBJECT_0},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Query, ID3D11Texture2D,
                ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
                ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorOutputView,
                D3D11_ASYNC_GETDATA_DONOTFLUSH, D3D11_BIND_RENDER_TARGET,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                D3D11_QUERY_DESC, D3D11_QUERY_EVENT, D3D11_SDK_VERSION, D3D11_TEX2D_VPIV,
                D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
                D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
                D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
                D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
                D3D11_VPOV_DIMENSION_TEXTURE2D,
            },
            Dxgi::{
                Common::{
                    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
                },
                CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1,
                IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST,
                DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
            },
        },
        System::Threading::{
            CreateEventW, GetCurrentThread, SetThreadPriority, WaitForSingleObject,
            THREAD_PRIORITY_ABOVE_NORMAL,
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

const NVENC_BUFFER_COUNT: usize = 4;
const ENCODE_COMPLETION_TIMEOUT_MS: u32 = 1_000;
const GPU_COPY_COMPLETION_TIMEOUT: Duration = Duration::from_secs(1);
const DXGI_CAPTURE_STALL_TIMEOUT: Duration = Duration::from_secs(2);
const ACTIVE_CAPTURE_STALL_TIMEOUT: Duration = Duration::from_secs(2);
const CAPTURE_ACTIVITY_WINDOW: Duration = Duration::from_secs(1);
const CAPTURE_ACTIVITY_THRESHOLD: u64 = 3;
const MAX_IMMEDIATE_HARDWARE_FAILURES: u32 = 3;

struct CaptureFlags {
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    force_keyframe: Arc<AtomicBool>,
    encoded_frames: mpsc::SyncSender<EncodedHardwareFrame>,
    hardware_progress: Arc<AtomicU64>,
    events: mpsc::Sender<SessionEvent>,
    frames_per_second: u32,
    bitrate_bps: u32,
    width: u32,
    height: u32,
}

struct HardwareCapture {
    device_context: ID3D11DeviceContext,
    copy_completion_query: ID3D11Query,
    encoder: Arc<NvencEncoder>,
    scalers: Vec<Option<D3dScaler>>,
    free_slots_tx: mpsc::SyncSender<usize>,
    free_slots: mpsc::Receiver<usize>,
    completion_tx: Option<mpsc::SyncSender<PendingNvencFrame>>,
    completion_handle: Option<JoinHandle<()>>,
    completion_failure: Arc<Mutex<Option<String>>>,
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
    capture_us: u64,
    encode_us: u64,
    encoded_at: Instant,
    captured_at: Instant,
}

struct PendingNvencFrame {
    slot: usize,
    mapped_resource: *mut c_void,
    frame_id: u64,
    timestamp_us: u64,
    capture_us: u64,
    encode_started: Instant,
    captured_at: Instant,
}

unsafe impl Send for PendingNvencFrame {}

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
    slots: Vec<NvencSlot>,
    width: u32,
    height: u32,
}

struct NvencSlot {
    texture: ID3D11Texture2D,
    registered_resource: *mut c_void,
    bitstream: *mut c_void,
    completion_event: HANDLE,
}

// The NVENC API explicitly supports submission and completion on separate
// threads when asynchronous encoding is enabled.
unsafe impl Send for NvencEncoder {}
unsafe impl Sync for NvencEncoder {}

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
    active_datagram_size: Arc<AtomicUsize>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let hardware_progress = Arc::new(AtomicU64::new(0));
        let result = loop {
            let dxgi_error = match run_dxgi_pipeline(
                socket.clone(),
                running.clone(),
                active_peer.clone(),
                force_keyframe.clone(),
                events.clone(),
                frames_per_second,
                bitrate_bps,
                width,
                height,
                active_datagram_size.clone(),
                hardware_progress.clone(),
            ) {
                Ok(()) => break Ok(()),
                Err(error) if running.load(Ordering::Relaxed) => error,
                Err(_) => break Ok(()),
            };
            warn!("DXGI/NVENC pipeline unavailable: {dxgi_error}");
            let _ = events.send(SessionEvent::VideoBackend(format!(
                "Windows Graphics Capture takeover after DXGI: {dxgi_error}"
            )));

            let mut immediate_failures = 0_u32;
            let wgc_error = loop {
                let progress_before = hardware_progress.load(Ordering::Acquire);
                match run_wgc_pipeline(
                    socket.clone(),
                    running.clone(),
                    active_peer.clone(),
                    force_keyframe.clone(),
                    events.clone(),
                    frames_per_second,
                    bitrate_bps,
                    width,
                    height,
                    active_datagram_size.clone(),
                    hardware_progress.clone(),
                ) {
                    Ok(()) => break None,
                    Err(error) if running.load(Ordering::Relaxed) => {
                        if hardware_progress.load(Ordering::Acquire) > progress_before {
                            immediate_failures = 0;
                        } else {
                            immediate_failures = immediate_failures.saturating_add(1);
                        }
                        if immediate_failures >= MAX_IMMEDIATE_HARDWARE_FAILURES {
                            break Some(error);
                        }
                        warn!(
                            "Windows Graphics Capture/NVENC stopped; rebuilding hardware pipeline: {error}"
                        );
                        force_keyframe.store(true, Ordering::Release);
                        let _ = events.send(SessionEvent::VideoBackend(format!(
                            "Windows Graphics Capture/NVENC restarting after: {error}"
                        )));
                        thread::sleep(Duration::from_millis(50));
                    }
                    Err(_) => break None,
                }
            };
            let Some(wgc_error) = wgc_error else {
                break Ok(());
            };

            // Once either hardware path has delivered a frame, never demote a
            // transient game/display/driver event to the unusable 4K software
            // encoder. Recreate fresh D3D11 and NVENC sessions and keep the
            // socket/audio session alive.
            if hardware_progress.load(Ordering::Acquire) > 0 {
                warn!(
                    "Both Windows hardware capture paths stopped; restarting from DXGI: {wgc_error}"
                );
                force_keyframe.store(true, Ordering::Release);
                let _ = events.send(SessionEvent::VideoBackend(format!(
                    "Rebuilding Windows hardware video pipeline after: {wgc_error}"
                )));
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            break Err(format!(
                "DXGI/NVENC failed ({dxgi_error}); Windows Graphics Capture/NVENC failed ({wgc_error})"
            ));
        };
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
                active_datagram_size,
            ) {
                let _ = events.send(SessionEvent::Error(format!(
                    "NVENC/D3D11 failed ({error}); software fallback failed: {fallback_error}"
                )));
            }
        }
    })
}

#[derive(Debug)]
enum DxgiCaptureFailure {
    AccessLost(String),
    Fatal(String),
}

#[allow(clippy::too_many_arguments)]
fn run_dxgi_pipeline(
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    force_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    frames_per_second: u32,
    bitrate_bps: u32,
    width: u32,
    height: u32,
    active_datagram_size: Arc<AtomicUsize>,
    hardware_progress: Arc<AtomicU64>,
) -> Result<(), String> {
    let (encoded_tx, encoded_rx) = mpsc::sync_channel(2);
    let sender_handle = spawn_hardware_sender(
        socket,
        running.clone(),
        active_peer.clone(),
        force_keyframe.clone(),
        events.clone(),
        encoded_rx,
        active_datagram_size,
    );
    let mut immediate_failures = 0_u32;
    let result = loop {
        if !running.load(Ordering::Relaxed) {
            break Ok(());
        }
        let progress_before = hardware_progress.load(Ordering::Acquire);
        let session = run_dxgi_capture_session(
            running.clone(),
            active_peer.clone(),
            force_keyframe.clone(),
            events.clone(),
            encoded_tx.clone(),
            frames_per_second,
            bitrate_bps,
            width,
            height,
            hardware_progress.clone(),
        );
        if hardware_progress.load(Ordering::Acquire) > progress_before {
            immediate_failures = 0;
        } else {
            immediate_failures = immediate_failures.saturating_add(1);
        }
        match session {
            Ok(()) => break Ok(()),
            Err(DxgiCaptureFailure::AccessLost(error)) => {
                force_keyframe.store(true, Ordering::Release);
                let _ = events.send(SessionEvent::VideoBackend(
                    "DXGI display mode changed · rebuilding capture and forcing IDR".to_owned(),
                ));
                warn!("DXGI capture session was invalidated: {error}");
                if immediate_failures >= MAX_IMMEDIATE_HARDWARE_FAILURES {
                    break Err(format!(
                        "DXGI repeatedly failed before producing a frame: {error}"
                    ));
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(DxgiCaptureFailure::Fatal(error)) => {
                force_keyframe.store(true, Ordering::Release);
                if immediate_failures >= MAX_IMMEDIATE_HARDWARE_FAILURES {
                    break Err(error);
                }
                warn!("DXGI/NVENC stopped; rebuilding hardware pipeline: {error}");
                let _ = events.send(SessionEvent::VideoBackend(format!(
                    "DXGI/NVENC restarting after: {error}"
                )));
                thread::sleep(Duration::from_millis(50));
            }
        }
    };
    drop(encoded_tx);
    let _ = sender_handle.join();
    result
}

#[allow(clippy::too_many_arguments)]
fn run_dxgi_capture_session(
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    force_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    encoded_frames: mpsc::SyncSender<EncodedHardwareFrame>,
    frames_per_second: u32,
    bitrate_bps: u32,
    width: u32,
    height: u32,
    hardware_progress: Arc<AtomicU64>,
) -> Result<(), DxgiCaptureFailure> {
    let (device, device_context, output) =
        create_dxgi_device_for_primary_display().map_err(DxgiCaptureFailure::Fatal)?;
    let duplication = unsafe { output.DuplicateOutput(&device) }.map_err(|error| {
        if error.code() == DXGI_ERROR_ACCESS_LOST {
            DxgiCaptureFailure::AccessLost(format!("Unable to duplicate primary display: {error}"))
        } else {
            DxgiCaptureFailure::Fatal(format!("Unable to duplicate primary display: {error}"))
        }
    })?;
    let peer_state = active_peer.clone();
    let mut capture = HardwareCapture::new_with_device(
        &device,
        device_context,
        CaptureFlags {
            running: running.clone(),
            active_peer,
            force_keyframe,
            encoded_frames,
            hardware_progress: hardware_progress.clone(),
            events: events.clone(),
            frames_per_second,
            bitrate_bps,
            width,
            height,
        },
        "NVIDIA NVENC H.264 P1 · DXGI Desktop Duplication · D3D11 BGRA→NV12 · 4-buffer async",
    )
    .map_err(DxgiCaptureFailure::Fatal)?;
    let _ = events.send(SessionEvent::CaptureReady);
    let mut peer_became_active = None::<Instant>;
    let mut captured_for_peer = false;
    let mut activity_window_started = Instant::now();
    let mut activity_frames = 0_u64;
    let mut active_video_seen = false;
    let mut last_desktop_frame = None::<Instant>;

    while running.load(Ordering::Relaxed) {
        match acquire_dxgi_frame(&duplication, &mut capture) {
            Ok(acquired) => {
                let peer_is_active = peer_state.lock().unwrap().is_some();
                if !peer_is_active {
                    peer_became_active = None;
                    captured_for_peer = false;
                    activity_window_started = Instant::now();
                    activity_frames = 0;
                    active_video_seen = false;
                    last_desktop_frame = None;
                    continue;
                }
                let now = Instant::now();
                let active_since = *peer_became_active.get_or_insert(now);
                if acquired {
                    captured_for_peer = true;
                    last_desktop_frame = Some(now);
                    if now.duration_since(activity_window_started) > CAPTURE_ACTIVITY_WINDOW {
                        activity_window_started = now;
                        activity_frames = 1;
                    } else {
                        activity_frames = activity_frames.saturating_add(1);
                    }
                    if activity_frames >= CAPTURE_ACTIVITY_THRESHOLD {
                        active_video_seen = true;
                    }
                }
                // A static desktop legitimately produces no new duplication
                // frames after its initial image. Only fail over if a newly
                // connected or newly rebuilt session cannot produce even that
                // first frame.
                if !captured_for_peer && active_since.elapsed() >= DXGI_CAPTURE_STALL_TIMEOUT {
                    drop(capture);
                    return Err(DxgiCaptureFailure::Fatal(format!(
                        "DXGI produced no desktop frames for {} ms while a viewer was connected",
                        DXGI_CAPTURE_STALL_TIMEOUT.as_millis()
                    )));
                }
                // DXGI is allowed to stay quiet on a static desktop. Once it
                // has demonstrated a stream of changing frames, however, a
                // multi-second silence means the duplication producer has
                // stalled without reporting DXGI_ERROR_ACCESS_LOST. Rebuild
                // the D3D/NVENC session instead of leaving the client frozen.
                if active_video_seen
                    && last_desktop_frame
                        .is_some_and(|last| last.elapsed() >= ACTIVE_CAPTURE_STALL_TIMEOUT)
                {
                    drop(capture);
                    return Err(DxgiCaptureFailure::Fatal(format!(
                        "DXGI stopped producing an active desktop stream for {} ms",
                        ACTIVE_CAPTURE_STALL_TIMEOUT.as_millis()
                    )));
                }
            }
            Err(DxgiCaptureFailure::AccessLost(error)) => {
                drop(capture);
                return Err(DxgiCaptureFailure::AccessLost(error));
            }
            Err(error) => {
                drop(capture);
                return Err(error);
            }
        }
    }
    drop(capture);
    Ok(())
}

fn create_dxgi_device_for_primary_display(
) -> Result<(ID3D11Device, ID3D11DeviceContext, IDXGIOutput1), String> {
    let primary =
        Monitor::primary().map_err(|error| format!("Unable to find primary display: {error}"))?;
    let primary_handle = primary.as_raw_hmonitor();
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
        .map_err(|error| format!("Unable to create DXGI factory: {error}"))?;

    let mut adapter_index = 0;
    loop {
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => return Err(format!("Unable to enumerate display adapters: {error}")),
        };
        let mut output_index = 0;
        loop {
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(output) => output,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => return Err(format!("Unable to enumerate display outputs: {error}")),
            };
            let description = unsafe { output.GetDesc() }
                .map_err(|error| format!("Unable to inspect display output: {error}"))?;
            if description.Monitor.0 == primary_handle {
                let adapter: IDXGIAdapter = adapter
                    .cast()
                    .map_err(|error| format!("Unable to open the primary GPU adapter: {error}"))?;
                let output: IDXGIOutput1 = output.cast().map_err(|error| {
                    format!("Desktop Duplication is unavailable on the primary output: {error}")
                })?;
                let mut device = None;
                let mut context = None;
                unsafe {
                    D3D11CreateDevice(
                        &adapter,
                        D3D_DRIVER_TYPE_UNKNOWN,
                        HMODULE::default(),
                        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                        None,
                        D3D11_SDK_VERSION,
                        Some(&mut device),
                        None,
                        Some(&mut context),
                    )
                }
                .map_err(|error| format!("Unable to create primary-GPU D3D11 device: {error}"))?;
                return Ok((
                    device.ok_or_else(|| "D3D11 returned no device".to_owned())?,
                    context.ok_or_else(|| "D3D11 returned no immediate context".to_owned())?,
                    output,
                ));
            }
            output_index += 1;
        }
        adapter_index += 1;
    }
    Err("The primary monitor was not found among the active DXGI outputs".to_owned())
}

fn acquire_dxgi_frame(
    duplication: &IDXGIOutputDuplication,
    capture: &mut HardwareCapture,
) -> Result<bool, DxgiCaptureFailure> {
    if let Some(error) = capture.take_completion_failure() {
        return Err(DxgiCaptureFailure::Fatal(format!(
            "NVENC completion pipeline stopped: {error}"
        )));
    }
    let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
    let mut resource: Option<IDXGIResource> = None;
    match unsafe { duplication.AcquireNextFrame(8, &mut info, &mut resource) } {
        Ok(()) => {}
        Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
        Err(error) if error.code() == DXGI_ERROR_ACCESS_LOST => {
            return Err(DxgiCaptureFailure::AccessLost(format!(
                "Desktop Duplication access was lost: {error}"
            )))
        }
        Err(error) => {
            return Err(DxgiCaptureFailure::Fatal(format!(
                "Unable to acquire DXGI desktop frame: {error}"
            )))
        }
    }

    // A successful acquisition must always be paired with ReleaseFrame, even
    // when conversion or NVENC submission fails.
    let processing_result = (|| {
        let resource =
            resource.ok_or_else(|| "DXGI returned a frame without a desktop texture".to_owned())?;
        let texture: ID3D11Texture2D = resource
            .cast()
            .map_err(|error| format!("DXGI desktop resource is not a D3D11 texture: {error}"))?;
        let mut description = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut description) };
        if description.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(format!(
                "Unexpected DXGI desktop format {:?}",
                description.Format
            ));
        }
        capture.process_texture(&texture, description.Width, description.Height)
    })();
    let release_result = unsafe { duplication.ReleaseFrame() }
        .map_err(|error| format!("Unable to release DXGI desktop frame: {error}"));
    processing_result.map_err(DxgiCaptureFailure::Fatal)?;
    release_result.map_err(DxgiCaptureFailure::Fatal)?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn run_wgc_pipeline(
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    force_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    frames_per_second: u32,
    bitrate_bps: u32,
    width: u32,
    height: u32,
    active_datagram_size: Arc<AtomicUsize>,
    hardware_progress: Arc<AtomicU64>,
) -> Result<(), String> {
    // Keep the capture callback bounded. UDP packetization and the many send
    // syscalls needed for a large H.264 frame must never hold the Windows
    // Graphics Capture frame-pool callback.
    // Keep only two completed frames ahead of the socket. If the network ever
    // falls behind, dropping to a fresh keyframe is preferable to displaying
    // an increasingly stale queue.
    let (encoded_tx, encoded_rx) = mpsc::sync_channel(2);
    let sender_handle = spawn_hardware_sender(
        socket,
        running.clone(),
        active_peer.clone(),
        force_keyframe.clone(),
        events.clone(),
        encoded_rx,
        active_datagram_size,
    );
    let peer_state = active_peer.clone();
    let monitor =
        Monitor::primary().map_err(|error| format!("Unable to find primary display: {error}"))?;
    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::WithCursor,
        DrawBorderSettings::WithoutBorder,
        ColorFormat::Bgra8,
        CaptureFlags {
            running: running.clone(),
            active_peer,
            force_keyframe,
            encoded_frames: encoded_tx,
            hardware_progress: hardware_progress.clone(),
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

    let mut peer_became_active = None::<Instant>;
    let mut activity_window_started = Instant::now();
    let mut activity_frames = 0_u64;
    let mut active_video_seen = false;
    let mut last_progress_at = None::<Instant>;
    let mut observed_progress = hardware_progress.load(Ordering::Acquire);
    let mut watchdog_error = None;

    while running.load(Ordering::Relaxed) && !control.is_finished() {
        let peer_is_active = peer_state.lock().unwrap().is_some();
        if !peer_is_active {
            peer_became_active = None;
            activity_window_started = Instant::now();
            activity_frames = 0;
            active_video_seen = false;
            last_progress_at = None;
            observed_progress = hardware_progress.load(Ordering::Acquire);
            thread::sleep(Duration::from_millis(20));
            continue;
        }

        let now = Instant::now();
        let active_since = *peer_became_active.get_or_insert(now);
        let current_progress = hardware_progress.load(Ordering::Acquire);
        if current_progress > observed_progress {
            let new_frames = current_progress - observed_progress;
            observed_progress = current_progress;
            last_progress_at = Some(now);
            if now.duration_since(activity_window_started) > CAPTURE_ACTIVITY_WINDOW {
                activity_window_started = now;
                activity_frames = new_frames;
            } else {
                activity_frames = activity_frames.saturating_add(new_frames);
            }
            if activity_frames >= CAPTURE_ACTIVITY_THRESHOLD {
                active_video_seen = true;
            }
        }

        if last_progress_at.is_none() && active_since.elapsed() >= DXGI_CAPTURE_STALL_TIMEOUT {
            watchdog_error = Some(format!(
                "Windows Graphics Capture produced no encoded frames for {} ms",
                DXGI_CAPTURE_STALL_TIMEOUT.as_millis()
            ));
            break;
        }
        if active_video_seen
            && last_progress_at.is_some_and(|last| last.elapsed() >= ACTIVE_CAPTURE_STALL_TIMEOUT)
        {
            watchdog_error = Some(format!(
                "Windows Graphics Capture stopped producing an active stream for {} ms",
                ACTIVE_CAPTURE_STALL_TIMEOUT.as_millis()
            ));
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    let capture_result = if let Some(error) = watchdog_error {
        match control.stop() {
            Ok(()) => Err(error),
            Err(stop_error) => Err(format!("{error}; capture stop also failed: {stop_error}")),
        }
    } else if running.load(Ordering::Relaxed) {
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
        Self::new_with_device(
            &context.device,
            context.device_context,
            context.flags,
            "NVIDIA NVENC H.264 P1 · Windows Graphics Capture · D3D11 BGRA→NV12 · 4-buffer async",
        )
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let source_texture = unsafe { frame.as_raw_texture() };
        self.process_texture(source_texture, frame.width(), frame.height())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        Err("The captured display was closed".to_owned())
    }
}

impl HardwareCapture {
    fn new_with_device(
        device: &ID3D11Device,
        device_context: ID3D11DeviceContext,
        flags: CaptureFlags,
        backend: &str,
    ) -> Result<Self, String> {
        raise_current_thread_priority("capture");
        if let Ok(dxgi_device) = device.cast::<IDXGIDevice>() {
            // The capture copy is a small, bounded workload but must make
            // forward progress even while a game saturates the 3D queue.
            // Relative priority +1 is intentionally modest: it avoids capture
            // starvation without using the soft/hard realtime priorities that
            // could materially interfere with the game.
            if let Err(error) = unsafe { dxgi_device.SetGPUThreadPriority(1) } {
                warn!("Unable to raise D3D11 capture priority: {error}");
            }
        }
        let encoder = Arc::new(NvencEncoder::new(
            device,
            flags.width,
            flags.height,
            flags.frames_per_second,
            flags.bitrate_bps,
        )?);
        let copy_completion_query = create_gpu_completion_query(device)?;
        let (free_slots_tx, free_slots) = mpsc::sync_channel(NVENC_BUFFER_COUNT);
        for slot in 0..NVENC_BUFFER_COUNT {
            free_slots_tx
                .send(slot)
                .map_err(|_| "Unable to initialize the NVENC buffer ring".to_owned())?;
        }
        let (completion_tx, completion_rx) = mpsc::sync_channel(NVENC_BUFFER_COUNT);
        let completion_failure = Arc::new(Mutex::new(None));
        let completion_handle = spawn_nvenc_completion(
            encoder.clone(),
            completion_rx,
            free_slots_tx.clone(),
            flags.encoded_frames.clone(),
            flags.force_keyframe.clone(),
            flags.running.clone(),
            completion_failure.clone(),
            flags.hardware_progress.clone(),
        );
        let _ = flags
            .events
            .send(SessionEvent::VideoBackend(backend.to_owned()));
        let frame_interval =
            Duration::from_secs_f64(1.0 / f64::from(flags.frames_per_second.max(1)));

        Ok(Self {
            device_context,
            copy_completion_query,
            encoder,
            scalers: std::iter::repeat_with(|| None)
                .take(NVENC_BUFFER_COUNT)
                .collect(),
            free_slots_tx,
            free_slots,
            completion_tx: Some(completion_tx),
            completion_handle: Some(completion_handle),
            completion_failure,
            flags,
            started: Instant::now(),
            frame_interval,
            next_frame_at: Instant::now(),
            frame_id: 0,
        })
    }

    fn process_texture(
        &mut self,
        source_texture: &ID3D11Texture2D,
        source_width: u32,
        source_height: u32,
    ) -> Result<(), String> {
        if let Some(error) = self.take_completion_failure() {
            return Err(format!("NVENC completion pipeline stopped: {error}"));
        }
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
        let Ok(slot) = self.free_slots.try_recv() else {
            // All four frames are still being encoded. Never block the WGC
            // callback; sampling a newer capture is lower latency.
            return Ok(());
        };
        let capture_started = Instant::now();
        let output_texture = &self.encoder.slots[slot].texture;

        // Convert BGRA capture output directly to NV12 in the D3D11 video
        // processor. NVENC can consume this texture without performing its own
        // RGB-to-YUV conversion, leaving more encoder throughput for 4K/120.
        let scaler_needs_rebuild = self.scalers[slot].as_ref().is_none_or(|scaler| {
            scaler.source_width != source_width || scaler.source_height != source_height
        });
        if scaler_needs_rebuild {
            match D3dScaler::new(
                &self.device_context,
                output_texture,
                source_width,
                source_height,
                self.flags.width,
                self.flags.height,
                self.flags.frames_per_second,
            ) {
                Ok(scaler) => self.scalers[slot] = Some(scaler),
                Err(error) => {
                    let _ = self.free_slots_tx.try_send(slot);
                    return Err(error);
                }
            }
        }
        let prepare_result = self.scalers[slot]
            .as_ref()
            .expect("scaler was initialized")
            .scale(source_texture);
        if let Err(error) = prepare_result {
            let _ = self.free_slots_tx.try_send(slot);
            return Err(error);
        }
        // DXGI invalidates its desktop surface as soon as ReleaseFrame is
        // called. VideoProcessorBlt is asynchronous, so a
        // Flush alone does not make it legal for the caller to release that
        // source texture. Wait only for this short GPU copy/scale operation;
        // NVENC itself remains four-buffer asynchronous.
        self.wait_for_gpu_copy()?;

        let force_idr = self.flags.force_keyframe.swap(false, Ordering::Relaxed);
        let timestamp_us = self.started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        let capture_us = capture_started
            .elapsed()
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        let encode_started = Instant::now();
        let mapped_resource = match self.encoder.submit(slot, force_idr) {
            Ok(mapped) => mapped,
            Err(error) => {
                let _ = self.free_slots_tx.try_send(slot);
                return Err(error);
            }
        };
        let pending = PendingNvencFrame {
            slot,
            mapped_resource,
            frame_id: self.frame_id,
            timestamp_us,
            capture_us,
            encode_started,
            captured_at: capture_started,
        };
        self.frame_id = self.frame_id.wrapping_add(1);
        if self
            .completion_tx
            .as_ref()
            .is_none_or(|sender| sender.send(pending).is_err())
        {
            self.flags.force_keyframe.store(true, Ordering::Release);
            let _ = self.encoder.unmap(mapped_resource);
            let _ = self.free_slots_tx.try_send(slot);
            return Err("NVENC completion worker disconnected".to_owned());
        }
        Ok(())
    }

    fn take_completion_failure(&self) -> Option<String> {
        self.completion_failure.lock().unwrap().take()
    }

    fn wait_for_gpu_copy(&self) -> Result<(), String> {
        unsafe {
            self.device_context.End(&self.copy_completion_query);
            self.device_context.Flush();
        }
        let started = Instant::now();
        let mut spins = 0_u32;
        loop {
            if !self.flags.running.load(Ordering::Relaxed) {
                return Err("D3D11 desktop copy cancelled while stopping".to_owned());
            }
            let mut complete = BOOL::default();
            unsafe {
                self.device_context.GetData(
                    &self.copy_completion_query,
                    Some((&mut complete as *mut BOOL).cast()),
                    std::mem::size_of::<BOOL>() as u32,
                    D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
                )
            }
            .map_err(|error| format!("Unable to query D3D11 copy completion: {error}"))?;
            if complete.as_bool() {
                return Ok(());
            }
            if started.elapsed() >= GPU_COPY_COMPLETION_TIMEOUT {
                return Err(format!(
                    "D3D11 desktop copy did not finish within {} ms",
                    GPU_COPY_COMPLETION_TIMEOUT.as_millis()
                ));
            }
            if spins < 128 {
                std::hint::spin_loop();
                spins += 1;
            } else {
                thread::yield_now();
            }
        }
    }
}

fn create_gpu_completion_query(device: &ID3D11Device) -> Result<ID3D11Query, String> {
    let description = D3D11_QUERY_DESC {
        Query: D3D11_QUERY_EVENT,
        MiscFlags: 0,
    };
    let mut query = None;
    unsafe { device.CreateQuery(&description, Some(&mut query)) }
        .map_err(|error| format!("Unable to create D3D11 copy-completion query: {error}"))?;
    query.ok_or_else(|| "D3D11 returned no copy-completion query".to_owned())
}

impl Drop for HardwareCapture {
    fn drop(&mut self) {
        self.completion_tx.take();
        if let Some(handle) = self.completion_handle.take() {
            let _ = handle.join();
        }
    }
}

fn spawn_nvenc_completion(
    encoder: Arc<NvencEncoder>,
    pending_frames: mpsc::Receiver<PendingNvencFrame>,
    free_slots: mpsc::SyncSender<usize>,
    encoded_frames: mpsc::SyncSender<EncodedHardwareFrame>,
    force_keyframe: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    completion_failure: Arc<Mutex<Option<String>>>,
    hardware_progress: Arc<AtomicU64>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        raise_current_thread_priority("NVENC completion");
        while let Ok(pending) = pending_frames.recv() {
            let result = encoder.complete(&pending);
            match result {
                Ok(mut frame) => {
                    let accepted = loop {
                        match encoded_frames.try_send(frame) {
                            Ok(()) => break true,
                            Err(mpsc::TrySendError::Full(returned)) => {
                                if !running.load(Ordering::Relaxed) {
                                    break false;
                                }
                                frame = returned;
                                thread::sleep(Duration::from_millis(1));
                            }
                            Err(mpsc::TrySendError::Disconnected(_)) => break false,
                        }
                    };
                    if accepted {
                        hardware_progress.fetch_add(1, Ordering::Release);
                    }
                }
                Err(error) => {
                    warn!("Asynchronous NVENC completion failed: {error}");
                    force_keyframe.store(true, Ordering::Release);
                    *completion_failure.lock().unwrap() = Some(error);
                    let _ = free_slots.try_send(pending.slot);
                    break;
                }
            }
            // Return the texture only after its encoded output is accepted by
            // the sender. This propagates short socket bursts back to capture
            // instead of dropping a reference frame and starting an IDR storm.
            let _ = free_slots.try_send(pending.slot);
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn spawn_hardware_sender(
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    force_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    encoded_frames: mpsc::Receiver<EncodedHardwareFrame>,
    active_datagram_size: Arc<AtomicUsize>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        raise_current_thread_priority("video sender");
        let mut packet = Vec::with_capacity(MAX_DATAGRAM_SIZE);
        let mut stats_started = Instant::now();
        let mut stats_frames = 0_u64;
        let mut stats_bytes = 0_u64;
        let mut stats_capture_us = 0_u64;
        let mut stats_encode_us = 0_u64;
        let mut stats_send_us = 0_u64;
        let mut stats_encode_queue_us = 0_u64;
        let mut last_sent_frame = None::<u64>;
        let mut waiting_for_keyframe = true;

        while running.load(Ordering::Relaxed) {
            let frame = match encoded_frames.recv_timeout(Duration::from_millis(20)) {
                Ok(frame) => frame,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    report_sender_stats(
                        &events,
                        &mut stats_started,
                        &mut stats_frames,
                        &mut stats_bytes,
                        &mut stats_capture_us,
                        &mut stats_encode_us,
                        &mut stats_send_us,
                        &mut stats_encode_queue_us,
                    );
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let encode_queue_elapsed = frame.encoded_at.elapsed();
            if last_sent_frame.is_some_and(|last| frame.frame_id != last.wrapping_add(1)) {
                waiting_for_keyframe = true;
                force_keyframe.store(true, Ordering::Release);
            }
            if frame.captured_at.elapsed() > VIDEO_SEND_STALE_AGE {
                waiting_for_keyframe = true;
                force_keyframe.store(true, Ordering::Release);
                continue;
            }
            if waiting_for_keyframe && !frame.is_keyframe {
                force_keyframe.store(true, Ordering::Release);
                continue;
            }
            if frame.is_keyframe {
                waiting_for_keyframe = false;
            }
            let Some(peer) = *active_peer.lock().unwrap() else {
                waiting_for_keyframe = true;
                last_sent_frame = None;
                continue;
            };
            let fragments = match fragment_video_frame_with_datagram_size(
                frame.frame_id,
                frame.timestamp_us,
                frame.is_keyframe,
                &frame.data,
                active_datagram_size.load(Ordering::Acquire),
            ) {
                Ok(fragments) => fragments,
                Err(error) => {
                    let _ = events.send(SessionEvent::Error(format!(
                        "NVENC frame cannot be packetized: {error}"
                    )));
                    waiting_for_keyframe = true;
                    last_sent_frame = None;
                    force_keyframe.store(true, Ordering::Release);
                    continue;
                }
            };

            let send_started = Instant::now();
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
                last_sent_frame = Some(frame.frame_id);
                stats_frames = stats_frames.saturating_add(1);
                stats_capture_us = stats_capture_us.saturating_add(frame.capture_us);
                stats_encode_us = stats_encode_us.saturating_add(frame.encode_us);
                stats_send_us = stats_send_us.saturating_add(
                    send_started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64,
                );
                stats_encode_queue_us = stats_encode_queue_us.saturating_add(
                    encode_queue_elapsed.as_micros().min(u128::from(u64::MAX)) as u64,
                );
            } else {
                waiting_for_keyframe = true;
                last_sent_frame = None;
            }
            report_sender_stats(
                &events,
                &mut stats_started,
                &mut stats_frames,
                &mut stats_bytes,
                &mut stats_capture_us,
                &mut stats_encode_us,
                &mut stats_send_us,
                &mut stats_encode_queue_us,
            );
        }
    })
}

fn report_sender_stats(
    events: &mpsc::Sender<SessionEvent>,
    stats_started: &mut Instant,
    stats_frames: &mut u64,
    stats_bytes: &mut u64,
    stats_capture_us: &mut u64,
    stats_encode_us: &mut u64,
    stats_send_us: &mut u64,
    stats_encode_queue_us: &mut u64,
) {
    let elapsed = stats_started.elapsed();
    if elapsed < Duration::from_secs(1) {
        return;
    }
    let seconds = elapsed.as_secs_f32();
    let _ = events.send(SessionEvent::Stats {
        fps: *stats_frames as f32 / seconds,
        megabits_per_second: *stats_bytes as f32 * 8.0 / seconds / 1_000_000.0,
        capture_ms: average_milliseconds(*stats_capture_us, *stats_frames),
        encode_ms: average_milliseconds(*stats_encode_us, *stats_frames),
        send_ms: average_milliseconds(*stats_send_us, *stats_frames),
        encode_queue_ms: average_milliseconds(*stats_encode_queue_us, *stats_frames),
    });
    *stats_started = Instant::now();
    *stats_frames = 0;
    *stats_bytes = 0;
    *stats_capture_us = 0;
    *stats_encode_us = 0;
    *stats_send_us = 0;
    *stats_encode_queue_us = 0;
}

fn average_milliseconds(total_us: u64, samples: u64) -> f32 {
    if samples == 0 {
        0.0
    } else {
        total_us as f32 / samples as f32 / 1_000.0
    }
}

fn raise_current_thread_priority(role: &str) {
    if let Err(error) =
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) }
    {
        warn!("Unable to raise {role} thread priority: {error}");
    }
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
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        // The D3D11 video processor writes BGRA capture data into this NV12
        // render target. NVENC accepts the same texture via resource
        // registration without the D3D11 video-encoder bind flag.
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
            let rgb_full_range = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            // YCbCr_Matrix = BT.709 (bit 2), Nominal_Range = 16-235
            // (value 1 in bits 4-5). This matches the H.264 VUI emitted below.
            let nv12_bt709_limited = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: (1 << 2) | (1 << 4),
            };
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &rgb_full_range);
            video_context.VideoProcessorSetOutputColorSpace(&processor, &nv12_bt709_limited);
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
        width: u32,
        height: u32,
        frames_per_second: u32,
        bitrate_bps: u32,
    ) -> Result<Self, String> {
        let api = NvencApi::load()?;
        let mut result = Self {
            api,
            encoder: ptr::null_mut(),
            slots: Vec::with_capacity(NVENC_BUFFER_COUNT),
            width,
            height,
        };
        result.open(device)?;
        result.initialize(frames_per_second, bitrate_bps)?;
        for _ in 0..NVENC_BUFFER_COUNT {
            result.create_slot(device)?;
        }
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
        config.gopLength = u32::MAX;
        config.frameIntervalP = 1;
        config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
        config.rcParams.averageBitRate = bitrate_bps;
        config.rcParams.maxBitRate = bitrate_bps;
        let frame_budget = bitrate_bps / frames_per_second.max(1);
        config.rcParams.vbvBufferSize = frame_budget.max(1);
        config.rcParams.vbvInitialDelay = config.rcParams.vbvBufferSize;
        // At native ultrawide 120 FPS, throughput takes precedence over AQ.
        // The LAN bitrate budget is high enough to preserve detail without
        // spending extra NVENC cycles on adaptive quantization.
        config.rcParams.set_enableAQ(0);
        config.rcParams.set_aqStrength(0);
        config.rcParams.set_enableTemporalAQ(0);
        config.rcParams.set_enableLookahead(0);
        config.rcParams.set_zeroReorderDelay(1);
        let mut h264 = unsafe { config.encodeCodecConfig.h264Config };
        h264.idrPeriod = u32::MAX;
        // One slice per picture. Row-per-slice mode creates hundreds of NAL
        // units at native ultrawide resolutions without reducing latency
        // unless sub-frame bitstream output is also enabled.
        h264.sliceMode = 2;
        h264.sliceModeData = 1;
        h264.set_repeatSPSPPS(1);
        h264.h264VUIParameters.videoSignalTypePresentFlag = 1;
        h264.h264VUIParameters.videoFormat =
            NV_ENC_VUI_VIDEO_FORMAT::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
        h264.h264VUIParameters.videoFullRangeFlag = 0;
        h264.h264VUIParameters.colourDescriptionPresentFlag = 1;
        h264.h264VUIParameters.colourPrimaries =
            NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
        h264.h264VUIParameters.transferCharacteristics =
            NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
        h264.h264VUIParameters.colourMatrix =
            NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709;
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
            enableEncodeAsync: 1,
            enablePTD: 1,
            encodeConfig: &mut config,
            maxEncodeWidth: self.width,
            maxEncodeHeight: self.height,
            tuningInfo: NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            bufferFormat: NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
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

    fn create_slot(&mut self, device: &ID3D11Device) -> Result<(), String> {
        let register_event = required(
            self.api.functions.nvEncRegisterAsyncEvent,
            "NvEncRegisterAsyncEvent",
        )?;
        let texture = create_output_texture(device, self.width, self.height)?;
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
            bufferFormat: NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
            bufferUsage: NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
            ..Default::default()
        };
        nvenc_status(
            unsafe { register_resource(self.encoder, &mut resource) },
            "register D3D11 texture with NVENC",
        )?;

        let create = required(
            self.api.functions.nvEncCreateBitstreamBuffer,
            "NvEncCreateBitstreamBuffer",
        )?;
        let mut bitstream = NV_ENC_CREATE_BITSTREAM_BUFFER {
            version: NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
            ..Default::default()
        };
        if let Err(error) = nvenc_status(
            unsafe { create(self.encoder, &mut bitstream) },
            "create NVENC bitstream buffer",
        ) {
            unsafe {
                if let Some(unregister) = self.api.functions.nvEncUnregisterResource {
                    let _ = unregister(self.encoder, resource.registeredResource);
                }
            }
            return Err(error);
        }

        let completion_event = match unsafe { CreateEventW(None, false, false, None) } {
            Ok(event) => event,
            Err(error) => {
                unsafe {
                    if let Some(destroy) = self.api.functions.nvEncDestroyBitstreamBuffer {
                        let _ = destroy(self.encoder, bitstream.bitstreamBuffer);
                    }
                    if let Some(unregister) = self.api.functions.nvEncUnregisterResource {
                        let _ = unregister(self.encoder, resource.registeredResource);
                    }
                }
                return Err(format!("Unable to create NVENC completion event: {error}"));
            }
        };
        let mut event = NV_ENC_EVENT_PARAMS {
            version: NV_ENC_EVENT_PARAMS_VER,
            completionEvent: completion_event.0 as *mut c_void,
            ..Default::default()
        };
        if let Err(error) = nvenc_status(
            unsafe { register_event(self.encoder, &mut event) },
            "register NVENC completion event",
        ) {
            unsafe {
                let _ = CloseHandle(completion_event);
                if let Some(destroy) = self.api.functions.nvEncDestroyBitstreamBuffer {
                    let _ = destroy(self.encoder, bitstream.bitstreamBuffer);
                }
                if let Some(unregister) = self.api.functions.nvEncUnregisterResource {
                    let _ = unregister(self.encoder, resource.registeredResource);
                }
            }
            return Err(error);
        }

        self.slots.push(NvencSlot {
            texture,
            registered_resource: resource.registeredResource,
            bitstream: bitstream.bitstreamBuffer,
            completion_event,
        });
        Ok(())
    }

    fn submit(&self, slot_index: usize, force_idr: bool) -> Result<*mut c_void, String> {
        let slot = self
            .slots
            .get(slot_index)
            .ok_or_else(|| format!("Invalid NVENC slot {slot_index}"))?;
        let map_input = required(
            self.api.functions.nvEncMapInputResource,
            "NvEncMapInputResource",
        )?;
        let encode_picture = required(self.api.functions.nvEncEncodePicture, "NvEncEncodePicture")?;
        let mut mapped = NV_ENC_MAP_INPUT_RESOURCE {
            version: NV_ENC_MAP_INPUT_RESOURCE_VER,
            registeredResource: slot.registered_resource,
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
            outputBitstream: slot.bitstream,
            completionEvent: slot.completion_event.0 as *mut c_void,
            bufferFmt: mapped.mappedBufferFmt,
            pictureStruct: NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
            ..Default::default()
        };
        let encode_status = unsafe { encode_picture(self.encoder, &mut picture) };
        if let Err(error) = nvenc_status(encode_status, "submit asynchronous NVENC frame") {
            let _ = self.unmap(mapped.mappedResource);
            return Err(error);
        }
        Ok(mapped.mappedResource)
    }

    fn complete(&self, pending: &PendingNvencFrame) -> Result<EncodedHardwareFrame, String> {
        let slot = self
            .slots
            .get(pending.slot)
            .ok_or_else(|| format!("Invalid NVENC completion slot {}", pending.slot))?;
        let wait =
            unsafe { WaitForSingleObject(slot.completion_event, ENCODE_COMPLETION_TIMEOUT_MS) };
        if wait != WAIT_OBJECT_0 {
            let _ = self.unmap(pending.mapped_resource);
            return Err(format!(
                "NVENC completion event timed out or failed ({wait:?})"
            ));
        }
        let lock_bitstream = required(self.api.functions.nvEncLockBitstream, "NvEncLockBitstream")?;
        let unlock = required(
            self.api.functions.nvEncUnlockBitstream,
            "NvEncUnlockBitstream",
        )?;
        let mut lock = NV_ENC_LOCK_BITSTREAM {
            version: NV_ENC_LOCK_BITSTREAM_VER,
            outputBitstream: slot.bitstream,
            ..Default::default()
        };
        lock.set_doNotWait(1);
        if let Err(error) = nvenc_status(
            unsafe { lock_bitstream(self.encoder, &mut lock) },
            "lock completed NVENC bitstream",
        ) {
            let _ = self.unmap(pending.mapped_resource);
            return Err(error);
        }
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
        let unlock_result = nvenc_status(
            unsafe { unlock(self.encoder, slot.bitstream) },
            "unlock NVENC bitstream",
        );
        let unmap_result = self.unmap(pending.mapped_resource);
        unlock_result?;
        unmap_result?;
        if data.is_empty() {
            return Err("NVENC completed an empty frame".to_owned());
        }
        let is_keyframe = matches!(
            picture_type,
            NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR | NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I
        );
        Ok(EncodedHardwareFrame {
            frame_id: pending.frame_id,
            timestamp_us: pending.timestamp_us,
            is_keyframe,
            data,
            capture_us: pending.capture_us,
            encode_us: pending
                .encode_started
                .elapsed()
                .as_micros()
                .min(u128::from(u64::MAX)) as u64,
            encoded_at: Instant::now(),
            captured_at: pending.captured_at,
        })
    }

    fn unmap(&self, mapped_resource: *mut c_void) -> Result<(), String> {
        let unmap_input = required(
            self.api.functions.nvEncUnmapInputResource,
            "NvEncUnmapInputResource",
        )?;
        nvenc_status(
            unsafe { unmap_input(self.encoder, mapped_resource) },
            "unmap NVENC input texture",
        )
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.encoder.is_null() {
                for slot in &self.slots {
                    if let Some(unregister_event) = self.api.functions.nvEncUnregisterAsyncEvent {
                        let mut event = NV_ENC_EVENT_PARAMS {
                            version: NV_ENC_EVENT_PARAMS_VER,
                            completionEvent: slot.completion_event.0 as *mut c_void,
                            ..Default::default()
                        };
                        let _ = unregister_event(self.encoder, &mut event);
                    }
                    if let Some(destroy) = self.api.functions.nvEncDestroyBitstreamBuffer {
                        if !slot.bitstream.is_null() {
                            let _ = destroy(self.encoder, slot.bitstream);
                        }
                    }
                    if let Some(unregister) = self.api.functions.nvEncUnregisterResource {
                        if !slot.registered_resource.is_null() {
                            let _ = unregister(self.encoder, slot.registered_resource);
                        }
                    }
                    if !slot.completion_event.is_invalid() {
                        let _ = CloseHandle(slot.completion_event);
                    }
                }
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
    active_datagram_size: Arc<AtomicUsize>,
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
    );
    let _ = capture_handle.join();
    let _ = encoder_handle.join();
    Ok(())
}
