use super::{publish_server_stats, PendingServerStats, SessionEvent, VIDEO_SEND_STALE_AGE};
use echobind_core::{
    protocol::{CursorPosition, Packet, ServerStats, MAX_DATAGRAM_SIZE},
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
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tracing::warn;
use windows::{
    core::{w, Interface},
    Win32::{
        Foundation::{CloseHandle, BOOL, HANDLE, HMODULE, LUID, RECT, TRUE, WAIT_OBJECT_0},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11InfoQueue,
                ID3D11Multithread, ID3D11Query, ID3D11Texture2D, ID3D11VideoContext,
                ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
                ID3D11VideoProcessorOutputView, D3D11_ASYNC_GETDATA_DONOTFLUSH,
                D3D11_BIND_RENDER_TARGET, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_CREATE_DEVICE_DEBUG, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MESSAGE,
                D3D11_QUERY_DESC, D3D11_QUERY_EVENT, D3D11_SDK_VERSION, D3D11_TEX2D_VPIV,
                D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
                D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT,
                D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
                D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
                D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VPIV_DIMENSION_TEXTURE2D,
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
        Security::{
            AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES,
            SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
        },
        System::Threading::{
            CreateEventW, GetCurrentProcess, GetCurrentThread, OpenProcessToken, SetThreadPriority,
            WaitForSingleObject, THREAD_PRIORITY, THREAD_PRIORITY_ABOVE_NORMAL,
            THREAD_PRIORITY_HIGHEST,
        },
    },
};
use windows_capture::monitor::Monitor;

type NvencCreateInstance = unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
type NvencGetMaxVersion = unsafe extern "C" fn(*mut u32) -> NVENCSTATUS;
type D3dKmtSetProcessSchedulingPriorityClass = unsafe extern "system" fn(HANDLE, i32) -> i32;

const NVENC_BUFFER_COUNT: usize = 4;
const ENCODE_COMPLETION_TIMEOUT_MS: u32 = 1_000;
const WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const GPU_COPY_COMPLETION_TIMEOUT: Duration = Duration::from_secs(1);
const DXGI_CAPTURE_STALL_TIMEOUT: Duration = Duration::from_secs(2);
const ACTIVE_CAPTURE_STALL_TIMEOUT: Duration = Duration::from_secs(2);
const ACTIVE_ENCODE_STALL_TIMEOUT: Duration = Duration::from_secs(2);
const CAPTURE_ACTIVITY_WINDOW: Duration = Duration::from_secs(1);
const CAPTURE_ACTIVITY_THRESHOLD: u64 = 3;
const ACCESS_LOST_RETRY_DELAY_MS: u64 = 25;
const ACCESS_LOST_MAX_RETRY_DELAY_MS: u64 = 200;
const VIDEO_PACING_BATCH_DATAGRAMS: usize = 16;
const VIDEO_PACING_RATE_MULTIPLIER: u64 = 4;
const VIDEO_PACING_SLEEP_GUARD: Duration = Duration::from_micros(500);
const CURSOR_REFRESH_INTERVAL: Duration = Duration::from_millis(100);
// Keep capture/conversion work ahead of a saturated game without using DXGI's
// absolute realtime priorities, which Microsoft reserves for privileged work
// and which can make a HAGS system unresponsive when VRAM is exhausted.
const CAPTURE_GPU_THREAD_PRIORITY: i32 = 7;
// D3DKMT_SCHEDULINGPRIORITYCLASS_HIGH. Realtime is intentionally not used:
// NVIDIA documents no contract for it, and production streamers avoid it on
// HAGS because it can freeze encoding or reset the driver near the VRAM limit.
const D3DKMT_PROCESS_GPU_PRIORITY_HIGH: i32 = 4;

struct CaptureFlags {
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    force_keyframe: Arc<AtomicBool>,
    encoded_frames: mpsc::SyncSender<EncodedHardwareFrame>,
    metrics: Arc<WindowsCaptureMetrics>,
    hardware_progress: Arc<AtomicU64>,
    events: mpsc::Sender<SessionEvent>,
    frames_per_second: u32,
    bitrate_bps: u32,
    width: u32,
    height: u32,
}

#[derive(Default)]
struct WindowsCaptureMetrics {
    source_frames: AtomicU64,
    dxgi_timeouts: AtomicU64,
    dxgi_backlog: AtomicU64,
    dxgi_backlog_max: AtomicU64,
    pacing_skips: AtomicU64,
    slot_busy_skips: AtomicU64,
    cursor_only_frames: AtomicU64,
    stale_frames: AtomicU64,
}

struct HardwareCapture {
    device_context: ID3D11DeviceContext,
    debug_info_queue: Option<ID3D11InfoQueue>,
    copy_completion_query: ID3D11Query,
    encoder: Arc<NvencEncoder>,
    cursor_position: Option<CursorPosition>,
    cursor_peer: Option<SocketAddr>,
    cursor_sent_at: Option<Instant>,
    cursor_packet: Vec<u8>,
    capture_textures: Vec<Option<CaptureTexture>>,
    free_slots_tx: mpsc::SyncSender<usize>,
    free_slots: mpsc::Receiver<usize>,
    prepared_tx: Option<mpsc::SyncSender<PreparedHardwareFrame>>,
    submission_handle: Option<JoinHandle<()>>,
    submission_done: mpsc::Receiver<()>,
    completion_handle: Option<JoinHandle<()>>,
    completion_done: mpsc::Receiver<()>,
    completion_failure: Arc<Mutex<Option<String>>>,
    flags: CaptureFlags,
    started: Instant,
    frame_interval: Duration,
    next_frame_at: Instant,
    frame_id: u64,
}

struct CaptureTexture {
    width: u32,
    height: u32,
    texture: ID3D11Texture2D,
}

struct PreparedHardwareFrame {
    slot: usize,
    texture: ID3D11Texture2D,
    source_width: u32,
    source_height: u32,
    frame_id: u64,
    timestamp_us: u64,
    gpu_wait_us: u64,
    gpu_lock_us: u64,
    captured_at: Instant,
}

struct EncodedHardwareFrame {
    frame_id: u64,
    timestamp_us: u64,
    is_keyframe: bool,
    data: Vec<u8>,
    capture_us: u64,
    gpu_wait_us: u64,
    gpu_lock_us: u64,
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
    gpu_wait_us: u64,
    gpu_lock_us: u64,
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
    gpu_api_lock: Mutex<()>,
    poisoned: AtomicBool,
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
    active_datagram_size: Arc<AtomicUsize>,
    pending_server_stats: PendingServerStats,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let hardware_progress = Arc::new(AtomicU64::new(0));
        let result = run_dxgi_pipeline(
            socket,
            running.clone(),
            active_peer,
            force_keyframe,
            events.clone(),
            frames_per_second,
            bitrate_bps,
            width,
            height,
            active_datagram_size,
            hardware_progress,
            pending_server_stats,
        );
        if let Err(error) = result {
            if running.load(Ordering::Relaxed) {
                // Hardware mode is a hard requirement on Windows. Continuing
                // with OpenH264 conceals D3D/NVENC defects and makes
                // performance data meaningless. The poisoned-resource path
                // avoids further driver calls, then this deliberate abort lets
                // Windows Error Reporting capture the failure.
                let fatal = format!("fatal Windows D3D11/NVENC pipeline failure: {error}");
                warn!("{fatal}");
                let _ = events.send(SessionEvent::Error(fatal.clone()));
                eprintln!("{fatal}");
                std::process::abort();
            }
        }
    })
}

fn d3d_debug_requested() -> bool {
    std::env::var("ECHOBIND_D3D_DEBUG")
        .is_ok_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

#[derive(Debug)]
enum DxgiCaptureFailure {
    AccessLost(String),
    DeviceLost(String),
    EncoderStalled(String),
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
    pending_server_stats: PendingServerStats,
) -> Result<(), String> {
    let (encoded_tx, encoded_rx) = mpsc::sync_channel(2);
    let metrics = Arc::new(WindowsCaptureMetrics::default());
    let sender_running = Arc::new(AtomicBool::new(true));
    let sender_handle = spawn_hardware_sender(
        socket.clone(),
        running.clone(),
        sender_running.clone(),
        active_peer.clone(),
        force_keyframe.clone(),
        events.clone(),
        encoded_rx,
        active_datagram_size,
        bitrate_bps,
        metrics.clone(),
        pending_server_stats,
    );
    let mut consecutive_access_losses = 0_u32;
    let result = loop {
        if !running.load(Ordering::Relaxed) {
            break Ok(());
        }
        let progress_before = hardware_progress.load(Ordering::Acquire);
        let session = run_dxgi_capture_session(
            socket.clone(),
            running.clone(),
            active_peer.clone(),
            force_keyframe.clone(),
            events.clone(),
            encoded_tx.clone(),
            frames_per_second,
            bitrate_bps,
            width,
            height,
            metrics.clone(),
            hardware_progress.clone(),
        );
        if hardware_progress.load(Ordering::Acquire) > progress_before {
            consecutive_access_losses = 0;
        }
        match session {
            Ok(()) => break Ok(()),
            Err(DxgiCaptureFailure::AccessLost(error)) => {
                force_keyframe.store(true, Ordering::Release);
                consecutive_access_losses = consecutive_access_losses.saturating_add(1);
                if consecutive_access_losses == 1 {
                    let _ = events.send(SessionEvent::VideoBackend(
                        "DXGI display mode changed · rebuilding capture and forcing IDR".to_owned(),
                    ));
                    warn!("DXGI capture session was invalidated: {error}");
                } else if consecutive_access_losses.is_multiple_of(20) {
                    warn!(
                        "DXGI capture is still unavailable after {consecutive_access_losses} rebuild attempts: {error}"
                    );
                }
                // ACCESS_LOST is the documented Desktop Duplication response
                // to full-screen application, display-mode, and desktop
                // switches. It does not mean the D3D device or NVENC session
                // failed. Keep recreating the capture session with a bounded
                // backoff; promoting three quick transitions to a fatal error
                // made normal game startup abort the process.
                let retry_shift = consecutive_access_losses.saturating_sub(1).min(3);
                let retry_delay_ms =
                    (ACCESS_LOST_RETRY_DELAY_MS << retry_shift).min(ACCESS_LOST_MAX_RETRY_DELAY_MS);
                thread::sleep(Duration::from_millis(retry_delay_ms));
            }
            Err(DxgiCaptureFailure::DeviceLost(error)) => {
                // A removed D3D device cannot recover. Retrying this inner
                // loop only creates another encoder while the driver is still
                // resetting and can exhaust the process NVENC-session limit.
                force_keyframe.store(true, Ordering::Release);
                break Err(error);
            }
            Err(DxgiCaptureFailure::EncoderStalled(error)) => {
                force_keyframe.store(true, Ordering::Release);
                break Err(error);
            }
            Err(DxgiCaptureFailure::Fatal(error)) => {
                force_keyframe.store(true, Ordering::Release);
                break Err(error);
            }
        }
    };
    sender_running.store(false, Ordering::Release);
    drop(encoded_tx);
    let _ = sender_handle.join();
    result
}

#[allow(clippy::too_many_arguments)]
fn run_dxgi_capture_session(
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    force_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    encoded_frames: mpsc::SyncSender<EncodedHardwareFrame>,
    frames_per_second: u32,
    bitrate_bps: u32,
    width: u32,
    height: u32,
    metrics: Arc<WindowsCaptureMetrics>,
    hardware_progress: Arc<AtomicU64>,
) -> Result<(), DxgiCaptureFailure> {
    let high_process_gpu_priority = configure_process_gpu_priority();
    let (device, device_context, output) =
        create_dxgi_device_for_primary_display().map_err(classify_dxgi_capture_error)?;
    let duplication = unsafe { output.DuplicateOutput(&device) }.map_err(|error| {
        if error.code() == DXGI_ERROR_ACCESS_LOST {
            DxgiCaptureFailure::AccessLost(format!("Unable to duplicate primary display: {error}"))
        } else {
            classify_dxgi_capture_error(format!("Unable to duplicate primary display: {error}"))
        }
    })?;
    let peer_state = active_peer.clone();
    let process_priority_label = if high_process_gpu_priority {
        "process GPU high"
    } else {
        "process GPU normal"
    };
    let backend = format!(
        "NVIDIA NVENC H.264 P1 · DXGI Desktop Duplication · staged D3D11 BGRA→NV12 · {process_priority_label} / device +7 · 4-slot async"
    );
    let mut capture = HardwareCapture::new_with_device(
        &device,
        device_context,
        CaptureFlags {
            socket,
            running: running.clone(),
            active_peer,
            force_keyframe,
            encoded_frames,
            metrics,
            hardware_progress: hardware_progress.clone(),
            events: events.clone(),
            frames_per_second,
            bitrate_bps,
            width,
            height,
        },
        &backend,
    )
    .map_err(classify_dxgi_capture_error)?;
    let mut peer_became_active = None::<Instant>;
    let mut captured_for_peer = false;
    let mut activity_window_started = Instant::now();
    let mut activity_frames = 0_u64;
    let mut active_video_seen = false;
    let mut last_desktop_frame = None::<Instant>;
    let mut observed_encoded_progress = hardware_progress.load(Ordering::Acquire);
    let mut last_encoded_progress = None::<Instant>;

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
                    observed_encoded_progress = hardware_progress.load(Ordering::Acquire);
                    last_encoded_progress = None;
                    continue;
                }
                let now = Instant::now();
                let active_since = *peer_became_active.get_or_insert(now);
                let encoded_progress = hardware_progress.load(Ordering::Acquire);
                if encoded_progress > observed_encoded_progress {
                    observed_encoded_progress = encoded_progress;
                    last_encoded_progress = Some(now);
                }
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
                if active_video_seen
                    && last_encoded_progress.unwrap_or(active_since).elapsed()
                        >= ACTIVE_ENCODE_STALL_TIMEOUT
                {
                    drop(capture);
                    return Err(DxgiCaptureFailure::EncoderStalled(format!(
                        "NVENC completed no frames for {} ms while DXGI capture remained active",
                        ACTIVE_ENCODE_STALL_TIMEOUT.as_millis()
                    )));
                }
            }
            Err(DxgiCaptureFailure::AccessLost(error)) => {
                drop(capture);
                return Err(DxgiCaptureFailure::AccessLost(error));
            }
            Err(DxgiCaptureFailure::DeviceLost(error)) => {
                drop(capture);
                return Err(DxgiCaptureFailure::DeviceLost(error));
            }
            Err(DxgiCaptureFailure::EncoderStalled(error)) => {
                drop(capture);
                return Err(DxgiCaptureFailure::EncoderStalled(error));
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
                let base_flags =
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
                let debug_requested = d3d_debug_requested();
                let create_flags = if debug_requested {
                    base_flags | D3D11_CREATE_DEVICE_DEBUG
                } else {
                    base_flags
                };
                let mut device = None;
                let mut context = None;
                let mut create_result = unsafe {
                    D3D11CreateDevice(
                        &adapter,
                        D3D_DRIVER_TYPE_UNKNOWN,
                        HMODULE::default(),
                        create_flags,
                        None,
                        D3D11_SDK_VERSION,
                        Some(&mut device),
                        None,
                        Some(&mut context),
                    )
                };
                if create_result.is_err() && debug_requested {
                    warn!(
                        "ECHOBIND_D3D_DEBUG was requested but the D3D11 debug layer is unavailable; install the Windows Graphics Tools optional feature"
                    );
                    device = None;
                    context = None;
                    create_result = unsafe {
                        D3D11CreateDevice(
                            &adapter,
                            D3D_DRIVER_TYPE_UNKNOWN,
                            HMODULE::default(),
                            base_flags,
                            None,
                            D3D11_SDK_VERSION,
                            Some(&mut device),
                            None,
                            Some(&mut context),
                        )
                    };
                }
                create_result.map_err(|error| {
                    format!("Unable to create primary-GPU D3D11 device: {error}")
                })?;
                let device = device.ok_or_else(|| "D3D11 returned no device".to_owned())?;
                let context =
                    context.ok_or_else(|| "D3D11 returned no immediate context".to_owned())?;
                enable_d3d11_multithread_protection(&context)?;
                configure_capture_gpu_priority(&device)?;
                return Ok((device, context, output));
            }
            output_index += 1;
        }
        adapter_index += 1;
    }
    Err("The primary monitor was not found among the active DXGI outputs".to_owned())
}

fn enable_d3d11_multithread_protection(context: &ID3D11DeviceContext) -> Result<(), String> {
    // NVENC documents that NvEncLockBitstream may use the application's
    // DirectX device on its completion thread. D3D11 immediate-context
    // protection is off by default, so leaving this disabled permits that
    // internal use to race CopyResource/GetData on the capture thread.
    let multithread: ID3D11Multithread = context.cast().map_err(|error| {
        format!("Unable to enable D3D11 immediate-context thread protection: {error}")
    })?;
    unsafe {
        let _ = multithread.SetMultithreadProtected(TRUE);
    }
    if !unsafe { multithread.GetMultithreadProtected() }.as_bool() {
        return Err("D3D11 refused to enable immediate-context thread protection".to_owned());
    }
    Ok(())
}

fn configure_capture_gpu_priority(device: &ID3D11Device) -> Result<(), String> {
    // With the default priority (0), a game that saturates the graphics queue
    // can delay our BGRA->NV12 video-processor command for tens of
    // milliseconds. process_texture must wait for that command before the
    // acquired duplication surface may be released, so this starvation
    // directly collapses capture FPS. +7 is DXGI's highest *relative*
    // priority and applies to work submitted by this D3D11 device only.
    let dxgi_device: IDXGIDevice = device
        .cast()
        .map_err(|error| format!("Unable to configure capture GPU priority: {error}"))?;
    unsafe { dxgi_device.SetGPUThreadPriority(CAPTURE_GPU_THREAD_PRIORITY) }
        .map_err(|error| format!("Unable to raise capture GPU priority: {error}"))?;
    let actual = unsafe { dxgi_device.GetGPUThreadPriority() }
        .map_err(|error| format!("Unable to verify capture GPU priority: {error}"))?;
    if actual != CAPTURE_GPU_THREAD_PRIORITY {
        return Err(format!(
            "The graphics driver accepted capture GPU priority {}, but reported {actual}",
            CAPTURE_GPU_THREAD_PRIORITY
        ));
    }
    Ok(())
}

fn configure_process_gpu_priority() -> bool {
    // SetGPUThreadPriority controls ordering inside this D3D device, while
    // D3DKMT's process scheduling class controls how Windows arbitrates our
    // GPU queues against a game. Both are needed when the game saturates the
    // graphics engine. Resolve this WDDM entry point dynamically so older
    // Windows versions fail visibly without adding another linked subsystem.
    enable_gpu_scheduling_privilege();
    let library = match unsafe { Library::new("gdi32.dll") } {
        Ok(library) => library,
        Err(error) => {
            warn!("Unable to load gdi32.dll for capture GPU scheduling: {error}");
            return false;
        }
    };
    let set_priority = match unsafe {
        library.get::<D3dKmtSetProcessSchedulingPriorityClass>(
            b"D3DKMTSetProcessSchedulingPriorityClass\0",
        )
    } {
        Ok(function) => function,
        Err(error) => {
            warn!("Windows does not expose process GPU scheduling priority: {error}");
            return false;
        }
    };
    let status = unsafe { set_priority(GetCurrentProcess(), D3DKMT_PROCESS_GPU_PRIORITY_HIGH) };
    if status < 0 {
        warn!(
            "Unable to select high capture GPU scheduling priority (NTSTATUS 0x{:08X}); run the host elevated for full performance under GPU load",
            status as u32
        );
        false
    } else {
        true
    }
}

fn enable_gpu_scheduling_privilege() {
    let mut token = HANDLE::default();
    if let Err(error) = unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    } {
        warn!("Unable to open the host process token for GPU scheduling: {error}");
        return;
    }

    let result = (|| {
        let mut luid = LUID::default();
        unsafe { LookupPrivilegeValueW(None, w!("SeIncreaseBasePriorityPrivilege"), &mut luid) }
            .map_err(|error| format!("Unable to find the GPU scheduling privilege: {error}"))?;
        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        unsafe { AdjustTokenPrivileges(token, false, Some(&privileges), 0, None, None) }
            .map_err(|error| format!("Unable to enable the GPU scheduling privilege: {error}"))
    })();
    unsafe {
        let _ = CloseHandle(token);
    }
    if let Err(error) = result {
        warn!("{error}");
    }
}

fn acquire_dxgi_frame(
    duplication: &IDXGIOutputDuplication,
    capture: &mut HardwareCapture,
) -> Result<bool, DxgiCaptureFailure> {
    if let Some(error) = capture.take_completion_failure() {
        return Err(DxgiCaptureFailure::Fatal(format!(
            "D3D11/NVENC worker stopped: {error}"
        )));
    }
    let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
    let mut resource: Option<IDXGIResource> = None;
    match unsafe { duplication.AcquireNextFrame(8, &mut info, &mut resource) } {
        Ok(()) => {}
        Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => {
            capture
                .flags
                .metrics
                .dxgi_timeouts
                .fetch_add(1, Ordering::Relaxed);
            return Ok(false);
        }
        Err(error) if error.code() == DXGI_ERROR_ACCESS_LOST => {
            return Err(DxgiCaptureFailure::AccessLost(format!(
                "Desktop Duplication access was lost: {error}"
            )))
        }
        Err(error) => {
            return Err(classify_dxgi_capture_error(format!(
                "Unable to acquire DXGI desktop frame: {error}"
            )))
        }
    }

    let desktop_updated = info.LastPresentTime != 0;
    if desktop_updated {
        let accumulated = u64::from(info.AccumulatedFrames);
        let backlog = accumulated.saturating_sub(1);
        capture
            .flags
            .metrics
            .source_frames
            .fetch_add(1, Ordering::Relaxed);
        capture
            .flags
            .metrics
            .dxgi_backlog
            .fetch_add(backlog, Ordering::Relaxed);
        capture
            .flags
            .metrics
            .dxgi_backlog_max
            .fetch_max(backlog, Ordering::Relaxed);
    } else {
        capture
            .flags
            .metrics
            .cursor_only_frames
            .fetch_add(1, Ordering::Relaxed);
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
        capture.send_cursor_update(&info, description.Width, description.Height);
        if desktop_updated {
            capture.process_texture(&texture, description.Width, description.Height)
        } else {
            // The cursor is transported independently. Encoding the unchanged
            // desktop surface wastes an NVENC slot and competes with the next
            // real game presentation.
            Ok(())
        }
    })();
    let release_result = unsafe { duplication.ReleaseFrame() }.map_err(|error| {
        let message = format!("Unable to release DXGI desktop frame: {error}");
        if error.code() == DXGI_ERROR_ACCESS_LOST {
            DxgiCaptureFailure::AccessLost(message)
        } else {
            classify_dxgi_capture_error(message)
        }
    });
    processing_result.map_err(classify_dxgi_capture_error)?;
    release_result?;
    Ok(desktop_updated)
}

fn classify_dxgi_capture_error(error: String) -> DxgiCaptureFailure {
    if is_dxgi_access_lost_error(&error) {
        DxgiCaptureFailure::AccessLost(error)
    } else if is_d3d_device_lost_error(&error) {
        DxgiCaptureFailure::DeviceLost(error)
    } else {
        DxgiCaptureFailure::Fatal(error)
    }
}

fn is_dxgi_access_lost_error(error: &str) -> bool {
    // Some D3D/DXGI entry points reach this shared string-based classifier.
    // Preserve ACCESS_LOST as recoverable regardless of whether windows-rs
    // rendered the symbolic name or only the HRESULT.
    let error = error.to_ascii_uppercase();
    error.contains("0X887A0026") || error.contains("DXGI_ERROR_ACCESS_LOST")
}

fn is_d3d_device_lost_error(error: &str) -> bool {
    // DXGI_ERROR_DEVICE_REMOVED, DEVICE_HUNG, DEVICE_RESET, and
    // DRIVER_INTERNAL_ERROR. Error strings include both the immediate HRESULT
    // and GetDeviceRemovedReason, so matching either one retires the device.
    ["0x887A0005", "0x887A0006", "0x887A0007", "0x887A0020"]
        .iter()
        .any(|code| error.contains(code))
}

impl HardwareCapture {
    fn new_with_device(
        device: &ID3D11Device,
        device_context: ID3D11DeviceContext,
        flags: CaptureFlags,
        backend: &str,
    ) -> Result<Self, String> {
        raise_current_thread_priority("capture", THREAD_PRIORITY_HIGHEST);
        let encoder = Arc::new(NvencEncoder::new(
            device,
            flags.width,
            flags.height,
            flags.frames_per_second,
            flags.bitrate_bps,
        )?);
        let copy_completion_query = create_gpu_completion_query(device)?;
        let submission_completion_query = create_gpu_completion_query(device)?;
        let debug_info_queue: Option<ID3D11InfoQueue> = device.cast().ok();
        let (free_slots_tx, free_slots) = mpsc::sync_channel(NVENC_BUFFER_COUNT);
        for slot in 0..NVENC_BUFFER_COUNT {
            free_slots_tx
                .send(slot)
                .map_err(|_| "Unable to initialize the NVENC buffer ring".to_owned())?;
        }
        let (completion_tx, completion_rx) = mpsc::sync_channel(NVENC_BUFFER_COUNT);
        let (completion_done_tx, completion_done) = mpsc::channel();
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
            flags.events.clone(),
            completion_done_tx,
        );
        let (prepared_tx, prepared_rx) = mpsc::sync_channel(NVENC_BUFFER_COUNT);
        let (submission_done_tx, submission_done) = mpsc::channel();
        let submission_handle = spawn_nvenc_submission(
            device_context.clone(),
            debug_info_queue.clone(),
            submission_completion_query,
            encoder.clone(),
            prepared_rx,
            completion_tx,
            free_slots_tx.clone(),
            flags.force_keyframe.clone(),
            flags.running.clone(),
            flags.metrics.clone(),
            completion_failure.clone(),
            flags.frames_per_second,
            submission_done_tx,
        );
        let _ = flags
            .events
            .send(SessionEvent::VideoBackend(backend.to_owned()));
        let frame_interval =
            Duration::from_secs_f64(1.0 / f64::from(flags.frames_per_second.max(1)));

        Ok(Self {
            device_context,
            debug_info_queue,
            copy_completion_query,
            encoder,
            cursor_position: None,
            cursor_peer: None,
            cursor_sent_at: None,
            cursor_packet: Vec::with_capacity(14),
            capture_textures: std::iter::repeat_with(|| None)
                .take(NVENC_BUFFER_COUNT)
                .collect(),
            free_slots_tx,
            free_slots,
            prepared_tx: Some(prepared_tx),
            submission_handle: Some(submission_handle),
            submission_done,
            completion_handle: Some(completion_handle),
            completion_done,
            completion_failure,
            flags,
            started: Instant::now(),
            frame_interval,
            next_frame_at: Instant::now(),
            frame_id: 0,
        })
    }

    fn send_cursor_update(
        &mut self,
        frame_info: &DXGI_OUTDUPL_FRAME_INFO,
        source_width: u32,
        source_height: u32,
    ) {
        let pointer_updated = frame_info.LastMouseUpdateTime != 0;
        if pointer_updated {
            let pointer = frame_info.PointerPosition;
            let visible = pointer.Visible.as_bool();
            self.cursor_position = Some(CursorPosition {
                x: if visible {
                    scale_cursor_coordinate(pointer.Position.x, self.flags.width, source_width)
                } else {
                    0
                },
                y: if visible {
                    scale_cursor_coordinate(pointer.Position.y, self.flags.height, source_height)
                } else {
                    0
                },
                visible,
            });
        }

        let peer = *self.flags.active_peer.lock().unwrap();
        let peer_changed = peer != self.cursor_peer;
        self.cursor_peer = peer;
        let Some(peer) = peer else {
            self.cursor_sent_at = None;
            return;
        };
        let refresh_due = self
            .cursor_sent_at
            .is_none_or(|sent| sent.elapsed() >= CURSOR_REFRESH_INTERVAL);
        if !pointer_updated && !peer_changed && !refresh_due {
            return;
        }
        let Some(position) = self.cursor_position else {
            return;
        };

        Packet::CursorPosition(position).encode(&mut self.cursor_packet);
        if let Err(error) = self.flags.socket.send_to(&self.cursor_packet, peer) {
            warn!("Cursor position send to {peer} failed: {error}");
        } else {
            self.cursor_sent_at = Some(Instant::now());
        }
    }

    fn process_texture(
        &mut self,
        source_texture: &ID3D11Texture2D,
        source_width: u32,
        source_height: u32,
    ) -> Result<(), String> {
        if let Some(error) = self.take_completion_failure() {
            return Err(format!("D3D11/NVENC worker stopped: {error}"));
        }
        if self.flags.active_peer.lock().unwrap().is_none() {
            self.next_frame_at = Instant::now();
            return Ok(());
        }
        let now = Instant::now();
        if now + Duration::from_millis(1) < self.next_frame_at {
            self.flags
                .metrics
                .pacing_skips
                .fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        self.next_frame_at += self.frame_interval;
        if self.next_frame_at <= now {
            self.next_frame_at = now + self.frame_interval;
        }
        let Ok(slot) = self.free_slots.try_recv() else {
            // All four frames are still being encoded. Never block desktop
            // acquisition; sampling a newer capture is lower latency.
            self.flags
                .metrics
                .slot_busy_skips
                .fetch_add(1, Ordering::Relaxed);
            return Ok(());
        };
        // Copy the DXGI-owned surface into a slot owned by this process. The
        // expensive BGRA->NV12 conversion and NVENC submission happen on the
        // submission worker after this function returns and ReleaseFrame has
        // made the next desktop presentation available to DXGI.
        let capture_started = Instant::now();
        let texture_needs_rebuild = self.capture_textures[slot]
            .as_ref()
            .is_none_or(|texture| texture.width != source_width || texture.height != source_height);
        if texture_needs_rebuild {
            match create_capture_texture(&self.device_context, source_width, source_height) {
                Ok(texture) => {
                    self.capture_textures[slot] = Some(CaptureTexture {
                        width: source_width,
                        height: source_height,
                        texture,
                    });
                }
                Err(error) => {
                    let _ = self.free_slots_tx.try_send(slot);
                    return Err(error);
                }
            }
        }
        let capture_texture = self.capture_textures[slot]
            .as_ref()
            .expect("capture texture was initialized")
            .texture
            .clone();
        let gpu_lock_started = Instant::now();
        let gpu_api_guard = self.encoder.gpu_api_lock.lock().unwrap();
        let gpu_lock_us = gpu_lock_started
            .elapsed()
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        unsafe {
            self.device_context
                .CopyResource(&capture_texture, source_texture);
        }
        self.begin_copy_completion();
        drop(gpu_api_guard);
        let gpu_wait_started = Instant::now();
        if let Err(error) = self.wait_for_copy_completion() {
            let _ = self.free_slots_tx.try_send(slot);
            return Err(error);
        }
        let gpu_wait_us = gpu_wait_started
            .elapsed()
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;

        let timestamp_us = self.started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        let prepared = PreparedHardwareFrame {
            slot,
            texture: capture_texture,
            source_width,
            source_height,
            frame_id: self.frame_id,
            timestamp_us,
            gpu_wait_us,
            gpu_lock_us,
            captured_at: capture_started,
        };
        self.frame_id = self.frame_id.wrapping_add(1);
        match self
            .prepared_tx
            .as_ref()
            .map(|sender| sender.try_send(prepared))
        {
            Some(Ok(())) => Ok(()),
            Some(Err(mpsc::TrySendError::Full(_))) => {
                // This should be unreachable because every queued frame owns
                // one of the same four slots. Keep the low-latency policy if a
                // future channel-size change invalidates that invariant.
                self.flags
                    .metrics
                    .slot_busy_skips
                    .fetch_add(1, Ordering::Relaxed);
                self.flags.force_keyframe.store(true, Ordering::Release);
                let _ = self.free_slots_tx.try_send(slot);
                Ok(())
            }
            Some(Err(mpsc::TrySendError::Disconnected(_))) | None => {
                self.flags.force_keyframe.store(true, Ordering::Release);
                let _ = self.free_slots_tx.try_send(slot);
                Err("D3D11 conversion/NVENC submission worker disconnected".to_owned())
            }
        }
    }

    fn take_completion_failure(&self) -> Option<String> {
        self.completion_failure.lock().unwrap().take()
    }

    fn begin_copy_completion(&self) {
        unsafe {
            self.device_context.End(&self.copy_completion_query);
            self.device_context.Flush();
        }
    }

    fn wait_for_copy_completion(&self) -> Result<(), String> {
        wait_for_gpu_completion(
            &self.device_context,
            &self.copy_completion_query,
            &self.flags.running,
            &self.encoder,
            self.debug_info_queue.as_ref(),
            "D3D11 desktop copy",
        )
    }
}

fn wait_for_gpu_completion(
    device_context: &ID3D11DeviceContext,
    query: &ID3D11Query,
    running: &AtomicBool,
    encoder: &NvencEncoder,
    debug_info_queue: Option<&ID3D11InfoQueue>,
    operation: &str,
) -> Result<(), String> {
    let started = Instant::now();
    let mut spins = 0_u32;
    loop {
        if !running.load(Ordering::Relaxed) {
            return Err(format!("{operation} cancelled while stopping"));
        }
        let mut complete = BOOL::default();
        let completion = unsafe {
            device_context.GetData(
                query,
                Some((&mut complete as *mut BOOL).cast()),
                std::mem::size_of::<BOOL>() as u32,
                D3D11_ASYNC_GETDATA_DONOTFLUSH.0 as u32,
            )
        };
        if let Err(error) = completion {
            encoder.poison();
            return Err(format!(
                "Unable to query {operation} completion: {error}; {}{}",
                d3d11_device_removed_reason(device_context),
                d3d_debug_messages(debug_info_queue)
            ));
        }
        if complete.as_bool() {
            return Ok(());
        }
        if started.elapsed() >= GPU_COPY_COMPLETION_TIMEOUT {
            encoder.poison();
            return Err(format!(
                "{operation} did not finish within {} ms{}",
                GPU_COPY_COMPLETION_TIMEOUT.as_millis(),
                d3d_debug_messages(debug_info_queue)
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

fn d3d_debug_messages(queue: Option<&ID3D11InfoQueue>) -> String {
    let Some(queue) = queue else {
        return String::new();
    };
    let count = unsafe { queue.GetNumStoredMessages() };
    let first = count.saturating_sub(16);
    let mut messages = Vec::new();
    for index in first..count {
        let mut byte_length = 0_usize;
        if unsafe { queue.GetMessage(index, None, &mut byte_length) }.is_err()
            || byte_length < std::mem::size_of::<D3D11_MESSAGE>()
        {
            continue;
        }
        let word_count = byte_length.div_ceil(std::mem::size_of::<usize>());
        let mut storage = vec![0_usize; word_count];
        let message_ptr = storage.as_mut_ptr().cast::<D3D11_MESSAGE>();
        if unsafe { queue.GetMessage(index, Some(message_ptr), &mut byte_length) }.is_err() {
            continue;
        }
        let message = unsafe { &*message_ptr };
        let description = if message.pDescription.is_null() || message.DescriptionByteLength == 0 {
            String::new()
        } else {
            let bytes = unsafe {
                std::slice::from_raw_parts(message.pDescription, message.DescriptionByteLength)
            };
            String::from_utf8_lossy(bytes)
                .trim_end_matches('\0')
                .to_owned()
        };
        messages.push(format!(
            "{:?}/{:?}/{}: {}",
            message.Category, message.Severity, message.ID.0, description
        ));
    }
    if messages.is_empty() {
        "; D3D11 debug queue was empty".to_owned()
    } else {
        format!("; D3D11 debug: {}", messages.join(" | "))
    }
}

fn d3d11_device_removed_reason(device_context: &ID3D11DeviceContext) -> String {
    let device = match unsafe { device_context.GetDevice() } {
        Ok(device) => device,
        Err(error) => return format!("unable to retrieve D3D11 device: {error}"),
    };
    match unsafe { device.GetDeviceRemovedReason() } {
        Ok(()) => "GetDeviceRemovedReason returned S_OK".to_owned(),
        Err(reason) => format!(
            "GetDeviceRemovedReason: {reason} (HRESULT 0x{:08X})",
            reason.code().0 as u32
        ),
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
        self.prepared_tx.take();
        if let Some(handle) = self.submission_handle.take() {
            match self.submission_done.recv_timeout(WORKER_SHUTDOWN_TIMEOUT) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = handle.join();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.encoder.poison();
                    warn!(
                        "D3D11 conversion/NVENC submission worker did not stop within {} ms; detaching the poisoned driver worker",
                        WORKER_SHUTDOWN_TIMEOUT.as_millis()
                    );
                }
            }
        }
        if let Some(handle) = self.completion_handle.take() {
            match self.completion_done.recv_timeout(WORKER_SHUTDOWN_TIMEOUT) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = handle.join();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.encoder.poison();
                    warn!(
                        "NVENC completion worker did not stop within {} ms; detaching the poisoned driver worker so session shutdown can continue",
                        WORKER_SHUTDOWN_TIMEOUT.as_millis()
                    );
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_nvenc_submission(
    device_context: ID3D11DeviceContext,
    debug_info_queue: Option<ID3D11InfoQueue>,
    gpu_completion_query: ID3D11Query,
    encoder: Arc<NvencEncoder>,
    prepared_frames: mpsc::Receiver<PreparedHardwareFrame>,
    completion_tx: mpsc::SyncSender<PendingNvencFrame>,
    free_slots: mpsc::SyncSender<usize>,
    force_keyframe: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    metrics: Arc<WindowsCaptureMetrics>,
    completion_failure: Arc<Mutex<Option<String>>>,
    frames_per_second: u32,
    submission_done: mpsc::Sender<()>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        raise_current_thread_priority("D3D11/NVENC submission", THREAD_PRIORITY_HIGHEST);
        let mut scalers: Vec<Option<D3dScaler>> = std::iter::repeat_with(|| None)
            .take(NVENC_BUFFER_COUNT)
            .collect();

        while let Ok(prepared) = prepared_frames.recv() {
            if !running.load(Ordering::Relaxed) {
                let _ = free_slots.try_send(prepared.slot);
                continue;
            }
            if prepared.captured_at.elapsed() > VIDEO_SEND_STALE_AGE {
                metrics.stale_frames.fetch_add(1, Ordering::Relaxed);
                force_keyframe.store(true, Ordering::Release);
                let _ = free_slots.try_send(prepared.slot);
                continue;
            }

            let result = (|| {
                let output_texture = encoder
                    .slots
                    .get(prepared.slot)
                    .ok_or_else(|| format!("Invalid NVENC slot {}", prepared.slot))?
                    .texture
                    .clone();
                let gpu_lock_started = Instant::now();
                let gpu_api_guard = encoder.gpu_api_lock.lock().unwrap();
                let mut gpu_lock_us = prepared.gpu_lock_us.saturating_add(
                    gpu_lock_started
                        .elapsed()
                        .as_micros()
                        .min(u128::from(u64::MAX)) as u64,
                );
                let scaler_needs_rebuild = scalers[prepared.slot].as_ref().is_none_or(|scaler| {
                    scaler.source_width != prepared.source_width
                        || scaler.source_height != prepared.source_height
                });
                if scaler_needs_rebuild {
                    scalers[prepared.slot] = Some(D3dScaler::new(
                        &device_context,
                        &output_texture,
                        prepared.source_width,
                        prepared.source_height,
                        encoder.width,
                        encoder.height,
                        frames_per_second,
                    )?);
                }
                scalers[prepared.slot]
                    .as_ref()
                    .expect("video processor was initialized")
                    .scale(&prepared.texture)?;
                unsafe {
                    device_context.End(&gpu_completion_query);
                    device_context.Flush();
                }
                drop(gpu_api_guard);

                let gpu_wait_started = Instant::now();
                wait_for_gpu_completion(
                    &device_context,
                    &gpu_completion_query,
                    &running,
                    &encoder,
                    debug_info_queue.as_ref(),
                    "D3D11 BGRA-to-NV12 video processing",
                )?;
                let gpu_wait_us = prepared.gpu_wait_us.saturating_add(
                    gpu_wait_started
                        .elapsed()
                        .as_micros()
                        .min(u128::from(u64::MAX)) as u64,
                );
                if prepared.captured_at.elapsed() > VIDEO_SEND_STALE_AGE {
                    return Ok(None);
                }

                // Loss and capture-timeline changes request an IDR explicitly.
                // Avoid periodic ultrawide IDRs, whose packet bursts occupy
                // several frame budgets.
                let force_idr = force_keyframe.swap(false, Ordering::Relaxed);
                let capture_us = prepared
                    .captured_at
                    .elapsed()
                    .as_micros()
                    .min(u128::from(u64::MAX)) as u64;
                let encode_started = Instant::now();
                let submit_lock_started = Instant::now();
                let gpu_api_guard = encoder.gpu_api_lock.lock().unwrap();
                gpu_lock_us = gpu_lock_us.saturating_add(
                    submit_lock_started
                        .elapsed()
                        .as_micros()
                        .min(u128::from(u64::MAX)) as u64,
                );
                let mapped_resource = encoder.submit(prepared.slot, force_idr)?;
                drop(gpu_api_guard);

                Ok(Some(PendingNvencFrame {
                    slot: prepared.slot,
                    mapped_resource,
                    frame_id: prepared.frame_id,
                    timestamp_us: prepared.timestamp_us,
                    capture_us,
                    gpu_wait_us,
                    gpu_lock_us,
                    encode_started,
                    captured_at: prepared.captured_at,
                }))
            })();

            match result {
                Ok(Some(pending)) => {
                    if let Err(error) = completion_tx.send(pending) {
                        force_keyframe.store(true, Ordering::Release);
                        let pending = error.0;
                        let _gpu_api_guard = encoder.gpu_api_lock.lock().unwrap();
                        let _ = encoder.unmap(pending.mapped_resource);
                        let _ = free_slots.try_send(pending.slot);
                        *completion_failure.lock().unwrap() =
                            Some("NVENC completion worker disconnected".to_owned());
                        break;
                    }
                }
                Ok(None) => {
                    metrics.stale_frames.fetch_add(1, Ordering::Relaxed);
                    force_keyframe.store(true, Ordering::Release);
                    let _ = free_slots.try_send(prepared.slot);
                }
                Err(error) => {
                    warn!("D3D11 conversion/NVENC submission failed: {error}");
                    force_keyframe.store(true, Ordering::Release);
                    *completion_failure.lock().unwrap() = Some(error);
                    break;
                }
            }
        }
        let _ = submission_done.send(());
    })
}

#[allow(clippy::too_many_arguments)]
fn spawn_nvenc_completion(
    encoder: Arc<NvencEncoder>,
    pending_frames: mpsc::Receiver<PendingNvencFrame>,
    free_slots: mpsc::SyncSender<usize>,
    encoded_frames: mpsc::SyncSender<EncodedHardwareFrame>,
    force_keyframe: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    completion_failure: Arc<Mutex<Option<String>>>,
    hardware_progress: Arc<AtomicU64>,
    events: mpsc::Sender<SessionEvent>,
    completion_done: mpsc::Sender<()>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        raise_current_thread_priority("NVENC completion", THREAD_PRIORITY_ABOVE_NORMAL);
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
                    if accepted && hardware_progress.fetch_add(1, Ordering::AcqRel) == 0 {
                        let _ = events.send(SessionEvent::CaptureReady);
                    }
                }
                Err(error) => {
                    warn!("Asynchronous NVENC completion failed: {error}");
                    force_keyframe.store(true, Ordering::Release);
                    *completion_failure.lock().unwrap() = Some(error);
                    // The failed resource may still be mapped or owned by the
                    // driver. Do not return it to the capture ring, and do not
                    // risk enqueueing the same slot twice while the pipeline
                    // is being torn down.
                    break;
                }
            }
            // Return the texture only after its encoded output is accepted by
            // the sender. This propagates short socket bursts back to capture
            // instead of dropping a reference frame and starting an IDR storm.
            let _ = free_slots.try_send(pending.slot);
        }
        let _ = completion_done.send(());
    })
}

#[allow(clippy::too_many_arguments)]
fn spawn_hardware_sender(
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    pipeline_running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    force_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    encoded_frames: mpsc::Receiver<EncodedHardwareFrame>,
    active_datagram_size: Arc<AtomicUsize>,
    bitrate_bps: u32,
    metrics: Arc<WindowsCaptureMetrics>,
    pending_server_stats: PendingServerStats,
) -> JoinHandle<()> {
    thread::spawn(move || {
        raise_current_thread_priority("video sender", THREAD_PRIORITY_ABOVE_NORMAL);
        let mut pacer = DatagramPacer::new(bitrate_bps);
        let mut packet = Vec::with_capacity(MAX_DATAGRAM_SIZE);
        let mut stats_started = Instant::now();
        let mut stats_frames = 0_u64;
        let mut stats_bytes = 0_u64;
        let mut stats_capture_us = 0_u64;
        let mut stats_gpu_wait_us = 0_u64;
        let mut stats_gpu_lock_us = 0_u64;
        let mut stats_encode_us = 0_u64;
        let mut stats_send_us = 0_u64;
        let mut stats_encode_queue_us = 0_u64;
        let mut last_sent_frame = None::<u64>;
        let mut waiting_for_keyframe = true;

        while running.load(Ordering::Relaxed) && pipeline_running.load(Ordering::Acquire) {
            let frame = match encoded_frames.recv_timeout(Duration::from_millis(20)) {
                Ok(frame) => frame,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    report_sender_stats(
                        &events,
                        &mut stats_started,
                        &mut stats_frames,
                        &mut stats_bytes,
                        &mut stats_capture_us,
                        &mut stats_gpu_wait_us,
                        &mut stats_gpu_lock_us,
                        &mut stats_encode_us,
                        &mut stats_send_us,
                        &mut stats_encode_queue_us,
                        &metrics,
                        &pending_server_stats,
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
                metrics.stale_frames.fetch_add(1, Ordering::Relaxed);
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
            pacer.begin_frame();
            for (fragment_index, fragment) in fragments.into_iter().enumerate() {
                if fragment_index > 0 && fragment_index % VIDEO_PACING_BATCH_DATAGRAMS == 0 {
                    pacer.wait();
                }
                Packet::Video(fragment).encode(&mut packet);
                if let Err(error) = socket.send_to(&packet, peer) {
                    warn!("Video send to {peer} failed: {error}");
                    frame_sent = false;
                    force_keyframe.store(true, Ordering::Release);
                    break;
                }
                pacer.account(packet.len());
                stats_bytes = stats_bytes.saturating_add(packet.len() as u64);
            }
            if frame_sent {
                last_sent_frame = Some(frame.frame_id);
                stats_frames = stats_frames.saturating_add(1);
                stats_capture_us = stats_capture_us.saturating_add(frame.capture_us);
                stats_gpu_wait_us = stats_gpu_wait_us.saturating_add(frame.gpu_wait_us);
                stats_gpu_lock_us = stats_gpu_lock_us.saturating_add(frame.gpu_lock_us);
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
                &mut stats_gpu_wait_us,
                &mut stats_gpu_lock_us,
                &mut stats_encode_us,
                &mut stats_send_us,
                &mut stats_encode_queue_us,
                &metrics,
                &pending_server_stats,
            );
        }
    })
}

struct DatagramPacer {
    bits_per_second: u64,
    next_batch_at: Instant,
}

impl DatagramPacer {
    fn new(bits_per_second: u32) -> Self {
        Self {
            // Smooth a frame over roughly one quarter of its configured
            // frame budget. This prevents a 100+ packet microburst without
            // adding a full frame of transport latency.
            bits_per_second: u64::from(bits_per_second.max(1))
                .saturating_mul(VIDEO_PACING_RATE_MULTIPLIER),
            next_batch_at: Instant::now(),
        }
    }

    fn begin_frame(&mut self) {
        let now = Instant::now();
        if self.next_batch_at < now {
            self.next_batch_at = now;
        }
    }

    fn account(&mut self, bytes: usize) {
        let transmission_nanos = (bytes as u128)
            .saturating_mul(8)
            .saturating_mul(1_000_000_000)
            / u128::from(self.bits_per_second);
        self.next_batch_at +=
            Duration::from_nanos(transmission_nanos.min(u128::from(u64::MAX)) as u64);
    }

    fn wait(&self) {
        loop {
            let now = Instant::now();
            let Some(remaining) = self.next_batch_at.checked_duration_since(now) else {
                return;
            };
            if remaining > VIDEO_PACING_SLEEP_GUARD {
                thread::sleep(remaining - VIDEO_PACING_SLEEP_GUARD);
            } else if remaining > Duration::from_micros(25) {
                thread::yield_now();
            } else {
                while Instant::now() < self.next_batch_at {
                    std::hint::spin_loop();
                }
                return;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn report_sender_stats(
    events: &mpsc::Sender<SessionEvent>,
    stats_started: &mut Instant,
    stats_frames: &mut u64,
    stats_bytes: &mut u64,
    stats_capture_us: &mut u64,
    stats_gpu_wait_us: &mut u64,
    stats_gpu_lock_us: &mut u64,
    stats_encode_us: &mut u64,
    stats_send_us: &mut u64,
    stats_encode_queue_us: &mut u64,
    metrics: &WindowsCaptureMetrics,
    pending_server_stats: &PendingServerStats,
) {
    let elapsed = stats_started.elapsed();
    if elapsed < Duration::from_secs(1) {
        return;
    }
    let seconds = elapsed.as_secs_f32();
    let source_frames = metrics.source_frames.swap(0, Ordering::Relaxed);
    publish_server_stats(
        events,
        pending_server_stats,
        ServerStats {
            fps: *stats_frames as f32 / seconds,
            source_fps: source_frames as f32 / seconds,
            megabits_per_second: *stats_bytes as f32 * 8.0 / seconds / 1_000_000.0,
            capture_ms: average_milliseconds(*stats_capture_us, *stats_frames),
            gpu_wait_ms: average_milliseconds(*stats_gpu_wait_us, *stats_frames),
            gpu_lock_ms: average_milliseconds(*stats_gpu_lock_us, *stats_frames),
            encode_ms: average_milliseconds(*stats_encode_us, *stats_frames),
            send_ms: average_milliseconds(*stats_send_us, *stats_frames),
            encode_queue_ms: average_milliseconds(*stats_encode_queue_us, *stats_frames),
            dxgi_timeouts: metrics.dxgi_timeouts.swap(0, Ordering::Relaxed),
            dxgi_backlog: metrics.dxgi_backlog.swap(0, Ordering::Relaxed),
            dxgi_backlog_max: metrics.dxgi_backlog_max.swap(0, Ordering::Relaxed),
            pacing_skips: metrics.pacing_skips.swap(0, Ordering::Relaxed),
            slot_busy_skips: metrics.slot_busy_skips.swap(0, Ordering::Relaxed),
            cursor_only_frames: metrics.cursor_only_frames.swap(0, Ordering::Relaxed),
            stale_frames: metrics.stale_frames.swap(0, Ordering::Relaxed),
        },
    );
    *stats_started = Instant::now();
    *stats_frames = 0;
    *stats_bytes = 0;
    *stats_capture_us = 0;
    *stats_gpu_wait_us = 0;
    *stats_gpu_lock_us = 0;
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

fn scale_cursor_coordinate(value: i32, output_size: u32, source_size: u32) -> i32 {
    if source_size == 0 {
        return value;
    }
    let scaled = i64::from(value).saturating_mul(i64::from(output_size)) / i64::from(source_size);
    scaled.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn raise_current_thread_priority(role: &str, priority: THREAD_PRIORITY) {
    if let Err(error) = unsafe { SetThreadPriority(GetCurrentThread(), priority) } {
        warn!("Unable to raise {role} thread priority: {error}");
    }
}

fn create_capture_texture(
    device_context: &ID3D11DeviceContext,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, String> {
    let device = unsafe {
        device_context
            .GetDevice()
            .map_err(|error| format!("D3D11 context returned no device: {error}"))?
    };
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
        // The texture is a CopyResource destination on the acquisition thread
        // and a video-processor input on the submission thread.
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe {
        device
            .CreateTexture2D(&description, None, Some(&mut texture))
            .map_err(|error| format!("Unable to create BGRA capture-ring texture: {error}"))?;
    }
    texture.ok_or_else(|| "D3D11 returned no BGRA capture-ring texture".to_owned())
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
        // NV12 is written by the fixed-function D3D11 video processor and
        // registered directly with NVENC. D3D11_BIND_VIDEO_ENCODER belongs to
        // the separate D3D11 video-encoder API; combining it here previously
        // caused NVIDIA driver device removals under sustained load.
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
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumerator = unsafe {
            video_device
                .CreateVideoProcessorEnumerator(&content)
                .map_err(|error| format!("Unable to create D3D11 video scaler: {error}"))?
        };
        let bgra_support = unsafe {
            enumerator
                .CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM)
                .map_err(|error| {
                    format!("Unable to query D3D11 BGRA video-processor support: {error}")
                })?
        };
        if bgra_support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0 {
            return Err("The D3D11 video processor cannot consume BGRA capture frames".to_owned());
        }
        let nv12_output_support = unsafe {
            enumerator
                .CheckVideoProcessorFormat(DXGI_FORMAT_NV12)
                .map_err(|error| {
                    format!("Unable to query D3D11 NV12 video-processor output support: {error}")
                })?
        };
        if nv12_output_support & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0 {
            return Err("The D3D11 video processor cannot produce NV12 encoder frames".to_owned());
        }
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
            // Input is full-range desktop RGB. Output is conventional limited-
            // range BT.709 NV12, matching the H.264 VUI below. Bit 2 selects
            // BT.709 and Nominal_Range=1 occupies bits 4-5.
            let rgb_full_range = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            let nv12_bt709_limited = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0x14 };
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
            gpu_api_lock: Mutex::new(()),
            poisoned: AtomicBool::new(false),
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
        // NVIDIA documents this exact combination for applications that call
        // IDXGIOutputDuplication::AcquireNextFrame on the submission thread
        // and process NVENC output on a second thread. The driver may use the
        // application's DirectX device from NvEncLockBitstream.
        initialize.set_enableOutputInVidmem(0);
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
            completionEvent: completion_event.0,
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
            completionEvent: slot.completion_event.0,
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
            // A timed-out NVENC driver must not be called again here.
            // nvEncUnmapInputResource can itself wait indefinitely on the
            // suspended GPU context, hiding the original error and preventing
            // the owning session from ever releasing its socket.
            self.poison();
            return Err(format!(
                "NVENC completion event timed out or failed ({wait:?})"
            ));
        }
        let completion_lock_started = Instant::now();
        let _gpu_api_guard = self.gpu_api_lock.lock().unwrap();
        let gpu_lock_us = pending.gpu_lock_us.saturating_add(
            completion_lock_started
                .elapsed()
                .as_micros()
                .min(u128::from(u64::MAX)) as u64,
        );
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
        // Even though the completion event has fired, NVIDIA requires a
        // blocking lock for DXGI capture and NVENC output processing on
        // separate threads. A non-blocking lock in this configuration is
        // explicitly documented as potentially undefined behavior.
        lock.set_doNotWait(0);
        if let Err(error) = nvenc_status(
            unsafe { lock_bitstream(self.encoder, &mut lock) },
            "lock completed NVENC bitstream",
        ) {
            self.poison();
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
        if let Err(error) = unlock_result {
            self.poison();
            return Err(error);
        }
        if let Err(error) = self.unmap(pending.mapped_resource) {
            self.poison();
            return Err(error);
        }
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
            gpu_wait_us: pending.gpu_wait_us,
            gpu_lock_us,
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

    fn poison(&self) {
        self.poisoned.store(true, Ordering::Release);
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.encoder.is_null() {
                if self.poisoned.load(Ordering::Acquire) {
                    // The driver has already declared this D3D/NVENC state
                    // unsafe. Even nvEncDestroyEncoder can raise a native SEH
                    // exception rather than return an NVENC status here. Do
                    // not call into the poisoned session again; close only the
                    // process-owned event handles. Hardware recovery is also
                    // disabled for this process after a device-removal error.
                    for slot in &self.slots {
                        if !slot.completion_event.is_invalid() {
                            let _ = CloseHandle(slot.completion_event);
                        }
                    }
                    return;
                }
                for slot in &self.slots {
                    if let Some(unregister_event) = self.api.functions.nvEncUnregisterAsyncEvent {
                        let mut event = NV_ENC_EVENT_PARAMS {
                            version: NV_ENC_EVENT_PARAMS_VER,
                            completionEvent: slot.completion_event.0,
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
