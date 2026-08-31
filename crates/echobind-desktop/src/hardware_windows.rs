use super::{
    publish_server_stats, video_pacing_batch_datagrams, DatagramPacer, PendingServerStats,
    PreciseSleeper, SessionEvent,
};
use echobind_core::{
    protocol::{CursorPosition, Packet, ServerStats, MAX_DATAGRAM_SIZE},
    video::fragment_video_frame_with_datagram_size,
};
use libloading::Library;
use moq_nvenc::sys::nvEncodeAPI::{
    NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION, NVENCAPI_VERSION, NVENCSTATUS,
    NV_ENCODE_API_FUNCTION_LIST, NV_ENCODE_API_FUNCTION_LIST_VER, NV_ENC_BUFFER_FORMAT,
    NV_ENC_BUFFER_USAGE, NV_ENC_CODEC_H264_GUID, NV_ENC_CONFIG_H264, NV_ENC_CONFIG_VER,
    NV_ENC_CREATE_BITSTREAM_BUFFER, NV_ENC_CREATE_BITSTREAM_BUFFER_VER, NV_ENC_DEVICE_TYPE,
    NV_ENC_EVENT_PARAMS, NV_ENC_EVENT_PARAMS_VER, NV_ENC_H264_PROFILE_HIGH_GUID,
    NV_ENC_INITIALIZE_PARAMS, NV_ENC_INITIALIZE_PARAMS_VER, NV_ENC_INPUT_RESOURCE_TYPE,
    NV_ENC_LOCK_BITSTREAM, NV_ENC_LOCK_BITSTREAM_VER, NV_ENC_MAP_INPUT_RESOURCE,
    NV_ENC_MAP_INPUT_RESOURCE_VER, NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS,
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER, NV_ENC_PARAMS_RC_MODE, NV_ENC_PIC_FLAGS,
    NV_ENC_PIC_PARAMS, NV_ENC_PIC_PARAMS_VER, NV_ENC_PIC_STRUCT, NV_ENC_PIC_TYPE,
    NV_ENC_PRESET_CONFIG, NV_ENC_PRESET_CONFIG_VER, NV_ENC_PRESET_P1_GUID,
    NV_ENC_REGISTER_RESOURCE, NV_ENC_REGISTER_RESOURCE_VER, NV_ENC_TUNING_INFO,
    NV_ENC_VUI_COLOR_PRIMARIES, NV_ENC_VUI_MATRIX_COEFFS, NV_ENC_VUI_TRANSFER_CHARACTERISTIC,
    NV_ENC_VUI_VIDEO_FORMAT,
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
        Foundation::{
            CloseHandle, E_ACCESSDENIED, FALSE, HANDLE, HMODULE, LUID, RECT, TRUE, WAIT_OBJECT_0,
            WAIT_TIMEOUT,
        },
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
                ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
                ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
                D3D11_BIND_RENDER_TARGET, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_CREATE_DEVICE_DEBUG, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
                D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
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

const MIN_NVENC_BUFFER_COUNT: usize = 4;
const MAX_NVENC_BUFFER_COUNT: usize = 12;
const MAX_PRE_NVENC_IN_FLIGHT: usize = 2;
// Reserve roughly 100 ms of slots at the requested presentation rate. Four
// buffers are sufficient when the GPU is idle, but they impose a hard sub-50
// FPS ceiling as soon as loaded encode residency reaches ~80 ms. Pre-NVENC
// admission remains separately limited to two frames and is coalesced and
// age-limited below, so additional slots increase encoder overlap without
// flooding the graphics queue or turning into an unbounded latency queue.
const NVENC_BUFFER_BUDGET_MS: u32 = 100;
const NVENC_H264_NUM_SLICES_MODE: u32 = 3;
const MAX_PREPARED_FRAME_AGE_INTERVALS: f64 = 2.0;
const NVENC_BUSY_RETRY_DELAY: Duration = Duration::from_millis(1);
const NVENC_BUSY_RETRY_TIMEOUT: Duration = Duration::from_millis(50);
const NVENC_COMPLETION_STALL_TIMEOUT: Duration = Duration::from_secs(2);
// Normal teardown must allow the completion worker to exhaust the same
// documented two-second NVENC completion window used while streaming. The
// extra margin covers the worker's bounded polling and channel handoff.
const WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(2_250);
const DXGI_CAPTURE_STALL_TIMEOUT: Duration = Duration::from_secs(2);
const ACTIVE_ENCODE_STALL_WARNING: Duration = Duration::from_millis(250);
const ACTIVE_ENCODE_STALL_TIMEOUT: Duration = Duration::from_secs(2);
const NVENC_SUSTAINED_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const NVENC_BUSY_ACTIVITY_GRACE: Duration = Duration::from_millis(250);
const NVENC_OVERLOAD_RETRY_DELAY_MS: u64 = 250;
const NVENC_OVERLOAD_MAX_RETRY_DELAY_MS: u64 = 2_000;
const MAX_ENCODER_STALL_REBUILDS: u32 = 2;
const MAX_DEVICE_LOSS_REBUILDS: u32 = 5;
const DEVICE_LOSS_RETRY_DELAY: Duration = Duration::from_secs(1);
const CAPTURE_ACTIVITY_WINDOW: Duration = Duration::from_secs(1);
const CAPTURE_ACTIVITY_THRESHOLD: u64 = 3;
const ACCESS_LOST_RETRY_DELAY_MS: u64 = 25;
const ACCESS_LOST_MAX_RETRY_DELAY_MS: u64 = 200;
const INACTIVE_PEER_POLL_INTERVAL: Duration = Duration::from_millis(20);
const CURSOR_UPDATE_INTERVAL: Duration = Duration::from_millis(4);
const CURSOR_REFRESH_INTERVAL: Duration = Duration::from_millis(100);
// Keep capture/conversion work ahead of a saturated game without using DXGI's
// absolute realtime priorities, which Microsoft reserves for privileged work
// and which can make a HAGS system unresponsive when VRAM is exhausted.
const CAPTURE_GPU_THREAD_PRIORITY: i32 = 7;
// D3DKMT_SCHEDULINGPRIORITYCLASS_HIGH. Realtime is intentionally not used:
// NVIDIA documents no contract for it, and production streamers avoid it on
// HAGS because it can freeze encoding or reset the driver near the VRAM limit.
const D3DKMT_PROCESS_GPU_PRIORITY_HIGH: i32 = 4;
const D3DKMT_PROCESS_GPU_PRIORITY_ABOVE_NORMAL: i32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessGpuPriority {
    High,
    AboveNormal,
    Normal,
}

impl ProcessGpuPriority {
    fn label(self) -> &'static str {
        match self {
            Self::High => "process GPU high",
            Self::AboveNormal => "process GPU above-normal",
            Self::Normal => "process GPU normal",
        }
    }

    fn class(self) -> u64 {
        match self {
            Self::High => D3DKMT_PROCESS_GPU_PRIORITY_HIGH as u64,
            Self::AboveNormal => D3DKMT_PROCESS_GPU_PRIORITY_ABOVE_NORMAL as u64,
            Self::Normal => 2,
        }
    }
}

struct CaptureFlags {
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    cursor_updates: mpsc::SyncSender<CursorPosition>,
    force_keyframe: Arc<AtomicBool>,
    encoded_frames: mpsc::SyncSender<EncodedHardwareFrame>,
    metrics: Arc<WindowsCaptureMetrics>,
    nvenc_activity: Arc<Mutex<NvencActivityState>>,
    hardware_progress: Arc<AtomicU64>,
    next_frame_id: Arc<AtomicU64>,
    stream_started: Instant,
    events: mpsc::Sender<SessionEvent>,
    frames_per_second: u32,
    bitrate_bps: u32,
    width: u32,
    height: u32,
}

#[derive(Clone, Copy, Debug, Default)]
struct NvencActivityState {
    last_activity: Option<Instant>,
    busy_episode_started: Option<Instant>,
    last_busy_activity: Option<Instant>,
}

impl NvencActivityState {
    fn record_busy(&mut self, now: Instant) {
        let continues_episode = self
            .last_busy_activity
            .is_some_and(|last| now.saturating_duration_since(last) <= NVENC_BUSY_ACTIVITY_GRACE);
        if !continues_episode {
            self.busy_episode_started = Some(now);
        }
        self.last_busy_activity = Some(now);
        self.last_activity = Some(now);
    }

    fn record_completion(&mut self, now: Instant) {
        self.last_activity = Some(now);
        self.busy_episode_started = None;
        self.last_busy_activity = None;
    }

    fn record_submission(&mut self, now: Instant) {
        // Acceptance proves that NVENC is no longer continuously BUSY and
        // starts this frame's independent completion budget. Capture's
        // recovery clock must advance too, or a long pre-NVENC map can leave
        // a newly accepted frame only a few milliseconds before teardown.
        self.last_activity = Some(now);
        self.busy_episode_started = None;
        self.last_busy_activity = None;
    }

    fn is_sustained_busy(&self, now: Instant) -> bool {
        self.busy_episode_started
            .zip(self.last_busy_activity)
            .is_some_and(|(started, last)| {
                last.saturating_duration_since(started) >= NVENC_SUSTAINED_BUSY_TIMEOUT
                    && now.saturating_duration_since(last) <= NVENC_BUSY_ACTIVITY_GRACE
            })
    }
}

#[derive(Default)]
struct WindowsCaptureMetrics {
    source_frames: AtomicU64,
    dxgi_timeouts: AtomicU64,
    dxgi_backlog: AtomicU64,
    dxgi_backlog_max: AtomicU64,
    pacing_skips: AtomicU64,
    slot_busy_skips: AtomicU64,
    preprocess_busy_skips: AtomicU64,
    no_free_slot_skips: AtomicU64,
    cursor_only_frames: AtomicU64,
    stale_frames: AtomicU64,
    nvenc_submissions: AtomicU64,
    nvenc_completions: AtomicU64,
    // Unlike the per-second stats above, this never resets. Watchdog warnings
    // and recovery diagnostics use it to quantify BUSY pressure; recovery
    // timing comes from the session-scoped producer timestamps above.
    nvenc_busy_activity: AtomicU64,
    process_gpu_priority: AtomicU64,
    nvenc_buffer_count: AtomicU64,
    video_send_failures: AtomicU64,
}

struct HardwareCapture {
    device_context: ID3D11DeviceContext,
    encoder: Arc<NvencEncoder>,
    cursor_position: Option<CursorPosition>,
    cursor_peer: Option<SocketAddr>,
    cursor_queued_at: Option<Instant>,
    capture_textures: Vec<Option<CaptureTexture>>,
    latest_source_generation: Arc<AtomicU64>,
    completed_source_generation: Arc<AtomicU64>,
    preprocess_in_flight: Arc<AtomicUsize>,
    nvenc_pending: Arc<AtomicUsize>,
    free_slots_tx: mpsc::SyncSender<usize>,
    free_slots: mpsc::Receiver<usize>,
    prepared_tx: Option<mpsc::SyncSender<PreparedHardwareFrame>>,
    conversion_handle: Option<JoinHandle<()>>,
    conversion_done: mpsc::Receiver<()>,
    submission_handle: Option<JoinHandle<()>>,
    submission_done: mpsc::Receiver<()>,
    completion_handle: Option<JoinHandle<()>>,
    completion_done: mpsc::Receiver<()>,
    worker_failure: Arc<Mutex<Option<String>>>,
    workers_running: Arc<AtomicBool>,
    flags: CaptureFlags,
    frame_interval: Duration,
    next_frame_at: Instant,
}

struct CaptureTexture {
    width: u32,
    height: u32,
    texture: ID3D11Texture2D,
}

struct PreparedHardwareFrame {
    slot: usize,
    source_generation: u64,
    texture: ID3D11Texture2D,
    source_width: u32,
    source_height: u32,
    timestamp_us: u64,
    captured_at: Instant,
    copy_issue_us: u64,
}

struct ConvertedHardwareFrame {
    slot: usize,
    source_generation: u64,
    timestamp_us: u64,
    captured_at: Instant,
    copy_issue_us: u64,
    convert_issue_us: u64,
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
    copy_wait_us: u64,
    convert_wait_us: u64,
    map_us: u64,
    submit_us: u64,
    submit_api_us: u64,
    submit_busy_wait_us: u64,
    submit_busy_retries: u64,
    completion_wait_us: u64,
    bitstream_us: u64,
    bitstream_lock_us: u64,
    bitstream_copy_us: u64,
    bitstream_unlock_us: u64,
    unmap_us: u64,
    encoded_at: Instant,
}

struct PendingNvencFrame {
    slot: usize,
    source_generation: u64,
    mapped_resource: *mut c_void,
    frame_id: u64,
    timestamp_us: u64,
    capture_us: u64,
    gpu_wait_us: u64,
    gpu_lock_us: u64,
    encode_started: Instant,
    copy_wait_us: u64,
    convert_wait_us: u64,
    map_us: u64,
    submit_us: u64,
    submit_api_us: u64,
    submit_busy_wait_us: u64,
    submit_busy_retries: u64,
}

enum NvencSubmitResult {
    Submitted {
        mapped_resource: *mut c_void,
        map_us: u64,
        submit_us: u64,
        submit_api_us: u64,
        submit_busy_wait_us: u64,
        submit_busy_retries: u64,
    },
    Busy,
}

struct NvencSubmitAttempt {
    status: NVENCSTATUS,
    api_us: u64,
    busy_wait_us: u64,
    busy_retries: u64,
}

// NVENC asynchronous mode explicitly hands submitted samples from the
// submission thread to an output thread. The mapped resource is an opaque
// driver handle whose lifetime remains owned by the encoder until completion.
unsafe impl Send for PendingNvencFrame {}

struct D3dScaler {
    source_width: u32,
    source_height: u32,
    video_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    input_view: ID3D11VideoProcessorInputView,
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
    poisoned: AtomicBool,
}

struct NvencSlot {
    texture: ID3D11Texture2D,
    registered_resource: *mut c_void,
    bitstream: *mut c_void,
    completion_event: HANDLE,
}

// NVENC asynchronous mode uses a submission thread and a completion/output
// thread. Calls are synchronized by the driver; D3D11 immediate-context access
// is separately protected by ID3D11Multithread.
unsafe impl Send for NvencEncoder {}
unsafe impl Sync for NvencEncoder {}

fn nvenc_buffer_count(frames_per_second: u32) -> usize {
    let requested = frames_per_second
        .saturating_mul(NVENC_BUFFER_BUDGET_MS)
        .div_ceil(1_000) as usize;
    requested.clamp(MIN_NVENC_BUFFER_COUNT, MAX_NVENC_BUFFER_COUNT)
}

fn retain_newest_queued<T>(
    receiver: &mpsc::Receiver<T>,
    mut newest: T,
    mut discard: impl FnMut(T),
) -> T {
    while let Ok(next) = receiver.try_recv() {
        discard(newest);
        newest = next;
    }
    newest
}

fn update_capture_activity(
    window_started: &mut Instant,
    frames: &mut u64,
    now: Instant,
    desktop_updated: bool,
) -> bool {
    if now.saturating_duration_since(*window_started) >= CAPTURE_ACTIVITY_WINDOW {
        *window_started = now;
        *frames = 0;
    }
    if desktop_updated {
        *frames = frames.saturating_add(1);
    }
    *frames >= CAPTURE_ACTIVITY_THRESHOLD
}

fn encoder_progress_required(
    session_has_completion: bool,
    active_source: bool,
    source_delivery_pending: bool,
    preprocess_in_flight: usize,
    nvenc_pending: usize,
) -> bool {
    !session_has_completion
        || active_source
        || source_delivery_pending
        || preprocess_in_flight > 0
        || nvenc_pending > 0
}

fn current_surface_refresh_required(
    source_delivery_pending: bool,
    keyframe_refresh_pending: bool,
    preprocess_in_flight: usize,
    nvenc_pending: usize,
) -> bool {
    (source_delivery_pending || keyframe_refresh_pending)
        && preprocess_in_flight == 0
        && nvenc_pending == 0
}

fn update_required_progress_clock(
    progress_at: &mut Option<Instant>,
    now: Instant,
    progress_required: bool,
    completion_advanced: bool,
) -> Duration {
    if !progress_required {
        *progress_at = None;
        return Duration::ZERO;
    }
    if completion_advanced || progress_at.is_none() {
        *progress_at = Some(now);
    }
    now.saturating_duration_since(progress_at.expect("required progress clock was initialized"))
}

fn publish_first_worker_failure(
    running: &AtomicBool,
    workers_running: &AtomicBool,
    worker_failure: &Mutex<Option<String>>,
    error: String,
) -> bool {
    if !running.load(Ordering::Relaxed) {
        return false;
    }
    // Hold the error slot while stopping admission. Capture cannot observe
    // workers_running=false without also seeing the first concrete failure,
    // and a later completion timeout cannot overwrite a device-loss status.
    let mut failure = worker_failure.lock().unwrap();
    if !workers_running.swap(false, Ordering::AcqRel) {
        return false;
    }
    failure.get_or_insert(error);
    true
}

fn configure_single_h264_slice(config: &mut NV_ENC_CONFIG_H264) {
    // Mode 3 interprets sliceModeData as the number of slices in the whole
    // picture. Mode 2 instead means macroblock rows per slice; pairing mode 2
    // with data=1 silently creates roughly height/16 slices per frame.
    config.sliceMode = NVENC_H264_NUM_SLICES_MODE;
    config.sliceModeData = 1;
}

#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_hardware_pipeline(
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    peer_epoch: Arc<AtomicU64>,
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
            peer_epoch,
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
                // A media-pipeline failure must never take down the desktop
                // application. Device removal still retires this session,
                // but the UI and network control plane remain alive so the
                // user can stop/reconnect after the driver recovers.
                let message = format!("Windows D3D11/NVENC pipeline stopped: {error}");
                warn!("{message}");
                let _ = events.send(SessionEvent::Error(message));
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
    PeerInactive,
    AccessLost(String),
    DeviceLost(String),
    SourceUndelivered(String),
    EncoderOverloaded(String),
    EncoderStalled(String),
    Fatal(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NvencRecoveryDecision {
    Overloaded,
    Stalled,
}

#[allow(clippy::too_many_arguments)]
fn run_dxgi_pipeline(
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    peer_epoch: Arc<AtomicU64>,
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
    let (cursor_tx, cursor_rx) = mpsc::sync_channel(1);
    let metrics = Arc::new(WindowsCaptureMetrics::default());
    let sender_running = Arc::new(AtomicBool::new(true));
    let sender_handle = spawn_hardware_sender(
        socket.clone(),
        running.clone(),
        sender_running.clone(),
        active_peer.clone(),
        peer_epoch.clone(),
        force_keyframe.clone(),
        events.clone(),
        encoded_rx,
        cursor_rx,
        active_datagram_size,
        bitrate_bps,
        metrics.clone(),
        pending_server_stats,
    );
    let mut consecutive_access_losses = 0_u32;
    let mut encoder_overload_rebuilds = 0_u32;
    let mut encoder_stall_rebuilds = 0_u32;
    let mut device_loss_rebuilds = 0_u32;
    let mut source_refresh_rebuilds = 0_u32;
    // The transport stream outlives individual D3D/NVENC sessions. Preserve
    // its identity and timestamp epoch across every recovery so a rebuilt IDR
    // cannot collide with the receiver's recent-frame duplicate window.
    let next_frame_id = Arc::new(AtomicU64::new(0));
    let stream_started = Instant::now();
    let result = loop {
        if !running.load(Ordering::Relaxed) {
            break Ok(());
        }
        while running.load(Ordering::Relaxed) && active_peer.lock().unwrap().is_none() {
            thread::sleep(INACTIVE_PEER_POLL_INTERVAL);
        }
        if !running.load(Ordering::Relaxed) {
            break Ok(());
        }
        let progress_before = hardware_progress.load(Ordering::Acquire);
        let session = run_dxgi_capture_session(
            running.clone(),
            active_peer.clone(),
            peer_epoch.clone(),
            cursor_tx.clone(),
            force_keyframe.clone(),
            events.clone(),
            encoded_tx.clone(),
            frames_per_second,
            bitrate_bps,
            width,
            height,
            metrics.clone(),
            hardware_progress.clone(),
            next_frame_id.clone(),
            stream_started,
        );
        if hardware_progress.load(Ordering::Acquire) > progress_before {
            consecutive_access_losses = 0;
            encoder_overload_rebuilds = 0;
            encoder_stall_rebuilds = 0;
            device_loss_rebuilds = 0;
            source_refresh_rebuilds = 0;
        }
        // Worker cancellation and a simultaneous DXGI invalidation are normal
        // during an explicit Stop. Do not rebuild the capture device or turn
        // the teardown race into a user-visible error.
        if !running.load(Ordering::Relaxed) {
            break Ok(());
        }
        match session {
            Ok(()) => break Ok(()),
            Err(DxgiCaptureFailure::PeerInactive) => {
                force_keyframe.store(true, Ordering::Release);
            }
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
                // A TDR or driver reset invalidates every resource owned by
                // this session, but the next loop iteration creates a fresh
                // D3D device and all dependent NVENC resources. Give the old
                // workers time to retire before retrying so detached native
                // calls cannot rapidly exhaust the NVENC session limit.
                force_keyframe.store(true, Ordering::Release);
                device_loss_rebuilds = device_loss_rebuilds.saturating_add(1);
                if device_loss_rebuilds > MAX_DEVICE_LOSS_REBUILDS {
                    break Err(format!(
                        "The GPU did not recover after {MAX_DEVICE_LOSS_REBUILDS} device-loss rebuild attempts: {error}"
                    ));
                }
                let _ = events.send(SessionEvent::VideoBackend(format!(
                    "GPU device reset · rebuilding D3D11/NVENC ({device_loss_rebuilds}/{MAX_DEVICE_LOSS_REBUILDS})"
                )));
                warn!(
                    "Recovering the D3D11/NVENC pipeline after device loss (attempt {device_loss_rebuilds}/{MAX_DEVICE_LOSS_REBUILDS}): {error}"
                );
                wait_for_pipeline_retry(&running, DEVICE_LOSS_RETRY_DELAY);
            }
            Err(DxgiCaptureFailure::SourceUndelivered(error)) => {
                // DDA does not repeat the final bitmap update after the
                // desktop becomes static. Recreate the session until its
                // current-surface bootstrap reaches NVENC completion; unlike
                // a native encoder stall, this recoverable sampling debt must
                // not exhaust a fatal retry budget.
                force_keyframe.store(true, Ordering::Release);
                source_refresh_rebuilds = source_refresh_rebuilds.saturating_add(1);
                let retry_delay = nvenc_overload_retry_delay(source_refresh_rebuilds);
                if source_refresh_rebuilds == 1 {
                    let _ = events.send(SessionEvent::VideoBackend(
                        "Final desktop update was not delivered · refreshing the current surface"
                            .to_owned(),
                    ));
                }
                warn!(
                    "Refreshing the D3D11/NVENC session for an undelivered desktop update (attempt {}, retry in {} ms): {error}",
                    source_refresh_rebuilds,
                    retry_delay.as_millis(),
                );
                wait_for_pipeline_retry(&running, retry_delay);
            }
            Err(DxgiCaptureFailure::EncoderOverloaded(error)) => {
                // Continuous ENCODER_BUSY is recoverable driver backpressure,
                // not proof that the completion path is poisoned. Recreate a
                // clean session indefinitely while the host is running, but
                // exponentially back off to avoid churning NVENC sessions
                // while the GPU remains saturated.
                force_keyframe.store(true, Ordering::Release);
                encoder_overload_rebuilds = encoder_overload_rebuilds.saturating_add(1);
                let retry_delay = nvenc_overload_retry_delay(encoder_overload_rebuilds);
                if encoder_overload_rebuilds == 1 {
                    let _ = events.send(SessionEvent::VideoBackend(
                        "NVENC remains BUSY under GPU load · rebuilding with overload backoff"
                            .to_owned(),
                    ));
                }
                warn!(
                    "Recovering the D3D11/NVENC pipeline after sustained BUSY (attempt {}, retry in {} ms): {error}",
                    encoder_overload_rebuilds,
                    retry_delay.as_millis(),
                );
                wait_for_pipeline_retry(&running, retry_delay);
            }
            Err(DxgiCaptureFailure::EncoderStalled(error)) => {
                force_keyframe.store(true, Ordering::Release);
                encoder_stall_rebuilds = encoder_stall_rebuilds.saturating_add(1);
                if encoder_stall_rebuilds > MAX_ENCODER_STALL_REBUILDS {
                    break Err(format!(
                        "NVENC failed to make forward progress after {MAX_ENCODER_STALL_REBUILDS} rebuild attempts: {error}"
                    ));
                }
                warn!("Recovering the D3D11/NVENC pipeline after a stall: {error}");
                thread::sleep(Duration::from_millis(ACCESS_LOST_RETRY_DELAY_MS));
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
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    peer_epoch: Arc<AtomicU64>,
    cursor_updates: mpsc::SyncSender<CursorPosition>,
    force_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    encoded_frames: mpsc::SyncSender<EncodedHardwareFrame>,
    frames_per_second: u32,
    bitrate_bps: u32,
    width: u32,
    height: u32,
    metrics: Arc<WindowsCaptureMetrics>,
    hardware_progress: Arc<AtomicU64>,
    next_frame_id: Arc<AtomicU64>,
    stream_started: Instant,
) -> Result<(), DxgiCaptureFailure> {
    let session_peer = active_peer
        .lock()
        .unwrap()
        .ok_or(DxgiCaptureFailure::PeerInactive)?;
    let session_peer_epoch = peer_epoch.load(Ordering::Acquire);
    let process_gpu_priority = configure_process_gpu_priority();
    metrics
        .process_gpu_priority
        .store(process_gpu_priority.class(), Ordering::Release);
    let (device, device_context, output) =
        create_dxgi_device_for_primary_display().map_err(classify_dxgi_capture_error)?;
    let duplication = unsafe { output.DuplicateOutput(&device) }.map_err(|error| {
        if error.code() == DXGI_ERROR_ACCESS_LOST || error.code() == E_ACCESSDENIED {
            DxgiCaptureFailure::AccessLost(format!("Unable to duplicate primary display: {error}"))
        } else {
            classify_dxgi_capture_error(format!("Unable to duplicate primary display: {error}"))
        }
    })?;
    let duplication_description = unsafe { duplication.GetDesc() };
    let source_width = duplication_description.ModeDesc.Width;
    let source_height = duplication_description.ModeDesc.Height;
    if source_width == 0 || source_height == 0 {
        return Err(DxgiCaptureFailure::Fatal(
            "Desktop Duplication reported an empty source surface".to_owned(),
        ));
    }
    let peer_state = active_peer.clone();
    let process_priority_label = process_gpu_priority.label();
    let backend = format!(
        "NVIDIA NVENC H.264 P1 · DXGI Desktop Duplication · {process_priority_label} / device +7 · ordered async NVENC"
    );
    // Unlike cumulative diagnostics, watchdog activity is session-scoped. A
    // poisoned old driver worker may be detached after the bounded teardown;
    // giving each rebuild its own state prevents that worker from perturbing
    // the new session's stall clock.
    let nvenc_activity = Arc::new(Mutex::new(NvencActivityState::default()));
    let mut capture = HardwareCapture::new_with_device(
        &device,
        device_context,
        source_width,
        source_height,
        CaptureFlags {
            running: running.clone(),
            active_peer,
            cursor_updates,
            force_keyframe,
            encoded_frames,
            metrics,
            nvenc_activity,
            hardware_progress: hardware_progress.clone(),
            next_frame_id,
            stream_started,
            events: events.clone(),
            frames_per_second,
            bitrate_bps,
            width,
            height,
        },
        &backend,
    )
    .map_err(classify_dxgi_capture_error)?;
    let active_since = Instant::now();
    let session_progress_started = hardware_progress.load(Ordering::Acquire);
    let mut desktop_frame_seen = false;
    let mut activity_window_started = Instant::now();
    let mut activity_frames = 0_u64;
    let mut observed_encoded_progress = session_progress_started;
    let mut busy_activity_at_last_completion = capture
        .flags
        .metrics
        .nvenc_busy_activity
        .load(Ordering::Acquire);
    // Start timing only while the pipeline owes output. This is cleared on a
    // healthy static desktop and restarted by its next isolated update, so an
    // hours-old completion can never make that new work look instantly stuck.
    // Only an actual NVENC completion advances the delivery-lag clock.
    // Repeated BUSY responses still count as recovery activity until the
    // separate sustained-BUSY threshold expires, but cannot hide delivery
    // lag in warnings.
    let mut required_progress_at = Some(active_since);
    let mut encode_stall_reported = false;

    while running.load(Ordering::Relaxed) {
        // Do not consume the duplication object's current desktop image while
        // nobody is watching or after the accepted UDP endpoint migrates.
        // The outer pipeline creates a fresh duplication session for the next
        // exact peer, which guarantees that a static desktop still has a
        // bootstrap image to encode.
        if *peer_state.lock().unwrap() != Some(session_peer)
            || peer_epoch.load(Ordering::Acquire) != session_peer_epoch
        {
            drop(capture);
            return Err(DxgiCaptureFailure::PeerInactive);
        }
        match acquire_dxgi_frame(&duplication, &mut capture) {
            Ok(acquired) => {
                let now = Instant::now();
                // AcquireNextFrame timeouts are the normal steady state for a
                // static desktop. Retry a coalesced cursor position here so a
                // full one-element channel cannot strand the final pointer
                // coordinate after DDA stops producing pointer updates.
                capture.queue_cursor_update(false);
                // `AcquireNextFrame` has no heartbeat on a static image. Keep
                // activity classification local to a rolling window instead
                // of permanently treating any later silence as capture loss.
                let active_video_seen = update_capture_activity(
                    &mut activity_window_started,
                    &mut activity_frames,
                    now,
                    acquired,
                );
                if capture.latest_source_generation.load(Ordering::Acquire) > 0 {
                    desktop_frame_seen = true;
                }
                // The submission/completion producers publish their actual
                // event times under one lock. Sampling only a cumulative BUSY
                // counter here can turn a capture-thread scheduling pause into
                // a fresh episode and indefinitely postpone recovery.
                let (
                    nvenc_activity,
                    busy_activity,
                    encoded_progress,
                    latest_source_generation,
                    completed_source_generation,
                    keyframe_refresh_pending,
                ) = {
                    // Keep this guard through both atomic loads. Completion
                    // clears the episode and increments hardware progress
                    // while holding the same mutex; releasing it before the
                    // loads could combine a stale sustained-BUSY episode with
                    // fresh completion progress and trigger a false rebuild.
                    let activity = capture.flags.nvenc_activity.lock().unwrap();
                    let busy_activity = capture
                        .flags
                        .metrics
                        .nvenc_busy_activity
                        .load(Ordering::Acquire);
                    let encoded_progress = hardware_progress.load(Ordering::Acquire);
                    let latest_source_generation =
                        capture.latest_source_generation.load(Ordering::Acquire);
                    let completed_source_generation =
                        capture.completed_source_generation.load(Ordering::Acquire);
                    let keyframe_refresh_pending =
                        capture.flags.force_keyframe.load(Ordering::Acquire);
                    (
                        *activity,
                        busy_activity,
                        encoded_progress,
                        latest_source_generation,
                        completed_source_generation,
                        keyframe_refresh_pending,
                    )
                };
                let encoded_progress_advanced = encoded_progress > observed_encoded_progress;
                if encoded_progress_advanced {
                    observed_encoded_progress = encoded_progress;
                    busy_activity_at_last_completion = busy_activity;
                    encode_stall_reported = false;
                }
                let session_has_completion = encoded_progress > session_progress_started;
                // A fresh duplication created for an active peer should yield
                // its current desktop. If it cannot do so, rebuild only the
                // session; once one image has been seen, silence is a valid
                // static desktop and is never inferred to be ACCESS_LOST.
                if !desktop_frame_seen && active_since.elapsed() >= DXGI_CAPTURE_STALL_TIMEOUT {
                    drop(capture);
                    return Err(DxgiCaptureFailure::AccessLost(format!(
                        "DXGI produced no desktop frames for {} ms while a viewer was connected",
                        DXGI_CAPTURE_STALL_TIMEOUT.as_millis()
                    )));
                }
                let preprocess_in_flight = capture.preprocess_in_flight.load(Ordering::Acquire);
                let nvenc_pending = capture.nvenc_pending.load(Ordering::Acquire);
                let source_delivery_pending =
                    latest_source_generation > completed_source_generation;
                let source_refresh_pending = source_delivery_pending || keyframe_refresh_pending;
                // Before the first completion, even one desktop image must
                // eventually produce an IDR. This arms recovery for the
                // initial-frame BUSY/drop case. Every acquired desktop update
                // also carries a generation through NVENC completion. If the
                // last update before a static interval was discarded for
                // pacing, freshness, admission, or BUSY, that delivery debt
                // keeps recovery armed until a later/current surface reaches
                // post-NVENC completion. A healthy static desktop has no debt
                // and remains quiet indefinitely.
                let encoder_progress_required = encoder_progress_required(
                    session_has_completion,
                    active_video_seen,
                    source_refresh_pending,
                    preprocess_in_flight,
                    nvenc_pending,
                );
                let no_required_progress_for = update_required_progress_clock(
                    &mut required_progress_at,
                    now,
                    encoder_progress_required,
                    encoded_progress_advanced,
                );
                let no_recovery_activity_for =
                    required_progress_at.map_or(Duration::ZERO, |required_since| {
                        let recovery_activity = nvenc_activity
                            .last_activity
                            .filter(|activity| *activity >= required_since)
                            .unwrap_or(required_since);
                        now.saturating_duration_since(recovery_activity)
                    });
                if !encoder_progress_required {
                    encode_stall_reported = false;
                }
                if encoder_progress_required
                    && !encode_stall_reported
                    && no_required_progress_for >= ACTIVE_ENCODE_STALL_WARNING
                {
                    encode_stall_reported = true;
                    // The rolling activity window deliberately lingers after
                    // motion stops. Do not turn that harmless active->static
                    // tail into a new IDR debt; only preserve recovery when a
                    // captured generation or real pipeline work is pending.
                    if source_delivery_pending || preprocess_in_flight > 0 || nvenc_pending > 0 {
                        capture.flags.force_keyframe.store(true, Ordering::Release);
                    }
                    let idr_refresh_pending = capture.flags.force_keyframe.load(Ordering::Acquire);
                    let submitted = capture
                        .flags
                        .metrics
                        .nvenc_submissions
                        .load(Ordering::Relaxed);
                    let completed = capture
                        .flags
                        .metrics
                        .nvenc_completions
                        .load(Ordering::Relaxed);
                    let overload_skips = capture
                        .flags
                        .metrics
                        .slot_busy_skips
                        .load(Ordering::Relaxed);
                    let stale = capture.flags.metrics.stale_frames.load(Ordering::Relaxed);
                    let busy_since_completion =
                        busy_activity.saturating_sub(busy_activity_at_last_completion);
                    warn!(
                        "Video pipeline has produced no frame for {} ms while output was required: NVENC submitted {submitted}, completed {completed}, source generations {completed_source_generation}/{latest_source_generation}, IDR refresh pending {idr_refresh_pending}, BUSY windows since completion {busy_since_completion} (cumulative {busy_activity}), preprocess in flight {}, NVENC pending {}, overload skips {overload_skips}, stale {stale}",
                        ACTIVE_ENCODE_STALL_WARNING.as_millis(),
                        preprocess_in_flight,
                        nvenc_pending,
                    );
                }
                let recovery_decision = if encoder_progress_required {
                    nvenc_recovery_decision(
                        no_recovery_activity_for,
                        no_required_progress_for,
                        nvenc_activity.is_sustained_busy(now),
                        nvenc_activity.last_busy_activity.is_some(),
                    )
                } else {
                    None
                };
                if recovery_decision.is_some() {
                    // Completion publishes its generation and global progress
                    // while holding this same activity lock. Recheck at the
                    // recovery boundary so a just-finished frame observed
                    // after the earlier snapshot does not cause an obsolete
                    // session rebuild.
                    let snapshot_advanced = {
                        let activity = capture.flags.nvenc_activity.lock().unwrap();
                        hardware_progress.load(Ordering::Acquire) > encoded_progress
                            || capture.completed_source_generation.load(Ordering::Acquire)
                                > completed_source_generation
                            || activity.last_activity.is_some_and(|current| {
                                nvenc_activity
                                    .last_activity
                                    .is_none_or(|snapshot| current > snapshot)
                            })
                            || capture
                                .flags
                                .metrics
                                .nvenc_busy_activity
                                .load(Ordering::Acquire)
                                > busy_activity
                            || (keyframe_refresh_pending
                                && !capture.flags.force_keyframe.load(Ordering::Acquire))
                    };
                    if snapshot_advanced {
                        continue;
                    }
                    // A conversion, submission, or completion worker can
                    // publish its concrete failure while AcquireNextFrame is
                    // waiting or while the watchdog snapshot is assembled.
                    // Prefer that failure (especially DEVICE_NOT_EXIST /
                    // INVALID_DEVICE) over a generic watchdog diagnosis so
                    // the outer loop takes the correct device-reset path.
                    if let Some(error) = capture.take_worker_failure() {
                        let failure = classify_worker_failure(&capture, error);
                        drop(capture);
                        return Err(failure);
                    }
                }
                if recovery_decision == Some(NvencRecoveryDecision::Overloaded) {
                    let submitted = capture
                        .flags
                        .metrics
                        .nvenc_submissions
                        .load(Ordering::Relaxed);
                    let completed = capture
                        .flags
                        .metrics
                        .nvenc_completions
                        .load(Ordering::Relaxed);
                    let busy_since_completion =
                        busy_activity.saturating_sub(busy_activity_at_last_completion);
                    drop(capture);
                    return Err(DxgiCaptureFailure::EncoderOverloaded(format!(
                        "NVENC remained BUSY without completing the required frame (submitted {submitted}, completed {completed}, BUSY windows {busy_since_completion})",
                    )));
                }
                if recovery_decision == Some(NvencRecoveryDecision::Stalled) {
                    let submitted = capture
                        .flags
                        .metrics
                        .nvenc_submissions
                        .load(Ordering::Relaxed);
                    let completed = capture
                        .flags
                        .metrics
                        .nvenc_completions
                        .load(Ordering::Relaxed);
                    let busy_since_completion =
                        busy_activity.saturating_sub(busy_activity_at_last_completion);
                    if current_surface_refresh_required(
                        source_delivery_pending,
                        keyframe_refresh_pending,
                        preprocess_in_flight,
                        nvenc_pending,
                    ) {
                        let reason = if source_delivery_pending {
                            format!(
                                "desktop generation {latest_source_generation} remained undelivered after generation {completed_source_generation}"
                            )
                        } else {
                            "an IDR refresh remained pending on an unchanged desktop".to_owned()
                        };
                        drop(capture);
                        return Err(DxgiCaptureFailure::SourceUndelivered(format!(
                            "{reason}; no copy/convert/submit or NVENC work remained, so the current DDA surface must be reacquired"
                        )));
                    }
                    drop(capture);
                    return Err(DxgiCaptureFailure::EncoderStalled(format!(
                        "NVENC made no completion or BUSY progress for {} ms while output was required (submitted {submitted}, completed {completed}, BUSY windows since completion {busy_since_completion})",
                        ACTIVE_ENCODE_STALL_TIMEOUT.as_millis(),
                    )));
                }
            }
            Err(error) => {
                // AcquireNextFrame can fail in the same interval in which an
                // asynchronous worker reports a more specific D3D/NVENC
                // failure. Consume the worker report before retiring the
                // session so a device reset is not mislabeled or lost.
                let error = if let Some(worker_error) = capture.take_worker_failure() {
                    let worker_failure = classify_worker_failure(&capture, worker_error);
                    // DEVICE_LOST has the strongest recovery requirement.
                    // A simultaneous generic worker shutdown must not
                    // downgrade an already-specific DXGI device failure.
                    if matches!(&worker_failure, DxgiCaptureFailure::DeviceLost(_))
                        || !matches!(&error, DxgiCaptureFailure::DeviceLost(_))
                    {
                        worker_failure
                    } else {
                        error
                    }
                } else {
                    error
                };
                drop(capture);
                if matches!(error, DxgiCaptureFailure::PeerInactive) {
                    unreachable!();
                }
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
    // DirectX device. D3D11 immediate-context protection is off by default,
    // so leaving this disabled permits that internal use to race
    // CopyResource and video-processor work on the other threads.
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
    // milliseconds. Capture no longer waits for that work, but a higher
    // relative priority still shortens the interval in which frames must be
    // dropped under contention. +7 applies only to this D3D11 device.
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

fn configure_process_gpu_priority() -> ProcessGpuPriority {
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
            return ProcessGpuPriority::Normal;
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
            return ProcessGpuPriority::Normal;
        }
    };
    let high_status =
        unsafe { set_priority(GetCurrentProcess(), D3DKMT_PROCESS_GPU_PRIORITY_HIGH) };
    if high_status >= 0 {
        return ProcessGpuPriority::High;
    }

    // HIGH normally requires SeIncreaseBasePriorityPrivilege. A desktop host
    // should not silently fall all the way back to NORMAL when it is launched
    // without elevation: ABOVE_NORMAL is the strongest unprivileged WDDM
    // class and materially reduces starvation by a game that saturates the
    // graphics queue feeding the map synchronization point.
    let above_normal_status = unsafe {
        set_priority(
            GetCurrentProcess(),
            D3DKMT_PROCESS_GPU_PRIORITY_ABOVE_NORMAL,
        )
    };
    if above_normal_status >= 0 {
        warn!(
            "High capture GPU scheduling priority was unavailable (NTSTATUS 0x{:08X}); using above-normal without elevation",
            high_status as u32
        );
        ProcessGpuPriority::AboveNormal
    } else {
        warn!(
            "Unable to raise capture GPU scheduling priority (high NTSTATUS 0x{:08X}, above-normal NTSTATUS 0x{:08X}); GPU-saturated applications may starve capture",
            high_status as u32,
            above_normal_status as u32,
        );
        ProcessGpuPriority::Normal
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
    if let Some(error) = capture.take_worker_failure() {
        return Err(classify_worker_failure(capture, error));
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
    // A pointer-only acquisition still carries a desktop surface. DDA may
    // produce that before any bitmap update on a newly created duplication,
    // and a static desktop will not provide a later heartbeat. Bootstrap from
    // the first successful surface even when LastPresentTime is zero.
    let bootstrap_surface = capture.latest_source_generation.load(Ordering::Acquire) == 0;
    let source_generation = (desktop_updated || bootstrap_surface).then(|| {
        capture
            .latest_source_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1)
    });

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
        capture.update_cursor_position(&info, description.Width, description.Height);
        if let Some(source_generation) = source_generation {
            capture.process_texture(
                &texture,
                description.Width,
                description.Height,
                source_generation,
            )
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

fn classify_worker_failure(capture: &HardwareCapture, error: String) -> DxgiCaptureFailure {
    let removed_reason = d3d_device_removed_reason(&capture.device_context);
    let message = if let Some(reason) = removed_reason {
        format!("D3D11/NVENC worker stopped: {error}; device removal reason: {reason}")
    } else {
        format!("D3D11/NVENC worker stopped: {error}")
    };
    if is_d3d_device_lost_error(&message) || is_nvenc_device_lost_error(&message) {
        DxgiCaptureFailure::DeviceLost(message)
    } else {
        DxgiCaptureFailure::EncoderStalled(message)
    }
}

fn classify_dxgi_capture_error(error: String) -> DxgiCaptureFailure {
    if is_dxgi_access_lost_error(&error) {
        DxgiCaptureFailure::AccessLost(error)
    } else if is_d3d_device_lost_error(&error) || is_nvenc_device_lost_error(&error) {
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
    let error = error.to_ascii_uppercase();
    ["0X887A0005", "0X887A0006", "0X887A0007", "0X887A0020"]
        .iter()
        .any(|code| error.contains(code))
}

fn is_nvenc_device_lost_error(error: &str) -> bool {
    let error = error.to_ascii_uppercase();
    error.contains("NV_ENC_ERR_DEVICE_NOT_EXIST") || error.contains("NV_ENC_ERR_INVALID_DEVICE")
}

fn d3d_device_removed_reason(context: &ID3D11DeviceContext) -> Option<String> {
    let device = unsafe { context.GetDevice().ok()? };
    unsafe { device.GetDeviceRemovedReason() }
        .err()
        .map(|error| error.to_string())
}

impl HardwareCapture {
    fn new_with_device(
        device: &ID3D11Device,
        device_context: ID3D11DeviceContext,
        source_width: u32,
        source_height: u32,
        flags: CaptureFlags,
        backend: &str,
    ) -> Result<Self, String> {
        raise_current_thread_priority("capture", THREAD_PRIORITY_HIGHEST);
        let buffer_count = nvenc_buffer_count(flags.frames_per_second);
        flags
            .metrics
            .nvenc_buffer_count
            .store(buffer_count as u64, Ordering::Release);
        // Feed NVENC NV12 rather than RGB. RGB input is supported, but the
        // driver performs additional preprocessing before the dedicated
        // encoder. On Ampere at ultrawide resolutions that work serialized
        // NvEncMapInputResource/NvEncEncodePicture and cut an idle RTX 3060
        // to less than 20 FPS.
        let encoder = Arc::new(NvencEncoder::new(
            device,
            flags.width,
            flags.height,
            flags.frames_per_second,
            flags.bitrate_bps,
            buffer_count,
        )?);
        // Allocate every app-owned capture texture and video-processor view
        // before capture pacing and stall detection begin. Lazy first use can
        // otherwise spend the two-frame freshness budget initializing a new
        // slot under load, drop it, rotate to the next uninitialized slot, and
        // repeat without ever reaching the first NVENC completion.
        let mut capture_textures = Vec::with_capacity(buffer_count);
        let mut scalers = Vec::with_capacity(buffer_count);
        for slot in 0..buffer_count {
            let texture = create_capture_texture(&device_context, source_width, source_height)?;
            let scaler = D3dScaler::new(
                &device_context,
                &texture,
                &encoder.slots[slot].texture,
                source_width,
                source_height,
                flags.width,
                flags.height,
                flags.frames_per_second,
            )?;
            capture_textures.push(Some(CaptureTexture {
                width: source_width,
                height: source_height,
                texture,
            }));
            scalers.push(Some(scaler));
        }
        let (free_slots_tx, free_slots) = mpsc::sync_channel(buffer_count);
        for slot in 0..buffer_count {
            free_slots_tx
                .send(slot)
                .map_err(|_| "Unable to initialize the NVENC buffer ring".to_owned())?;
        }
        let worker_failure = Arc::new(Mutex::new(None));
        let workers_running = Arc::new(AtomicBool::new(true));
        // These FIFO handoffs are the slot state machine. A unique slot moves
        // free -> prepared -> converted -> submitted -> completed -> free;
        // no stage clones or independently recycles its ownership token.
        let (prepared_tx, prepared_rx) = mpsc::sync_channel(buffer_count);
        // Keep exactly one converted sample ahead of NVENC. A ring-sized
        // converted queue lets the video processor issue many blits while
        // MapInputResource is blocked, only for submission to discard them as
        // stale later. The gate reserves the single queue position before a
        // blit is issued, preserving conversion/submission overlap without
        // feeding redundant work into a saturated GPU command stream.
        let (converted_tx, converted_rx) = mpsc::sync_channel(1);
        let (conversion_permit_tx, conversion_permit_rx) = mpsc::sync_channel(1);
        conversion_permit_tx
            .send(())
            .map_err(|_| "Unable to initialize the conversion-ahead permit".to_owned())?;
        let preprocess_in_flight = Arc::new(AtomicUsize::new(0));
        let nvenc_pending = Arc::new(AtomicUsize::new(0));
        // These watermarks are scoped to one D3D/NVENC session. Detached
        // workers from a poisoned session therefore cannot acknowledge a
        // desktop generation acquired by its replacement.
        let latest_source_generation = Arc::new(AtomicU64::new(0));
        let completed_source_generation = Arc::new(AtomicU64::new(0));
        let (completion_tx, completion_rx) = mpsc::sync_channel(buffer_count);
        let (completion_done_tx, completion_done) = mpsc::channel();
        let completion_handle = spawn_nvenc_completion(
            encoder.clone(),
            completion_rx,
            free_slots_tx.clone(),
            flags.encoded_frames.clone(),
            flags.force_keyframe.clone(),
            flags.running.clone(),
            workers_running.clone(),
            nvenc_pending.clone(),
            completed_source_generation.clone(),
            flags.metrics.clone(),
            flags.nvenc_activity.clone(),
            worker_failure.clone(),
            flags.hardware_progress.clone(),
            flags.events.clone(),
            completion_done_tx,
        );
        let (submission_done_tx, submission_done) = mpsc::channel();
        let submission_handle = spawn_nvenc_submission(
            encoder.clone(),
            converted_rx,
            completion_tx,
            free_slots_tx.clone(),
            flags.force_keyframe.clone(),
            flags.running.clone(),
            workers_running.clone(),
            preprocess_in_flight.clone(),
            nvenc_pending.clone(),
            flags.metrics.clone(),
            flags.nvenc_activity.clone(),
            worker_failure.clone(),
            flags.next_frame_id.clone(),
            flags.frames_per_second,
            conversion_permit_tx,
            submission_done_tx,
        );
        let (conversion_done_tx, conversion_done) = mpsc::channel();
        let conversion_handle = spawn_d3d_conversion(
            device_context.clone(),
            encoder.clone(),
            prepared_rx,
            converted_tx,
            free_slots_tx.clone(),
            flags.force_keyframe.clone(),
            flags.running.clone(),
            workers_running.clone(),
            preprocess_in_flight.clone(),
            flags.metrics.clone(),
            worker_failure.clone(),
            flags.frames_per_second,
            scalers,
            conversion_permit_rx,
            conversion_done_tx,
        );
        let _ = flags.events.send(SessionEvent::VideoBackend(format!(
            "{backend} · {buffer_count}-slot D3D11 BGRA→NV12 · split convert/submit/output NVENC"
        )));
        let frame_interval =
            Duration::from_secs_f64(1.0 / f64::from(flags.frames_per_second.max(1)));

        Ok(Self {
            device_context,
            encoder,
            cursor_position: None,
            cursor_peer: None,
            cursor_queued_at: None,
            capture_textures,
            latest_source_generation,
            completed_source_generation,
            preprocess_in_flight,
            nvenc_pending,
            free_slots_tx,
            free_slots,
            prepared_tx: Some(prepared_tx),
            conversion_handle: Some(conversion_handle),
            conversion_done,
            submission_handle: Some(submission_handle),
            submission_done,
            completion_handle: Some(completion_handle),
            completion_done,
            worker_failure,
            workers_running,
            flags,
            frame_interval,
            next_frame_at: Instant::now(),
        })
    }

    fn update_cursor_position(
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

        self.queue_cursor_update(pointer_updated);
    }

    fn queue_cursor_update(&mut self, pointer_updated: bool) {
        let peer = *self.flags.active_peer.lock().unwrap();
        let peer_changed = peer != self.cursor_peer;
        self.cursor_peer = peer;
        if peer.is_none() {
            self.cursor_queued_at = None;
            return;
        }
        let update_due = pointer_updated
            && self
                .cursor_queued_at
                .is_none_or(|queued| queued.elapsed() >= CURSOR_UPDATE_INTERVAL);
        let refresh_due = self
            .cursor_queued_at
            .is_none_or(|queued| queued.elapsed() >= CURSOR_REFRESH_INTERVAL);
        if !peer_changed && !update_due && !refresh_due {
            return;
        }
        let Some(position) = self.cursor_position else {
            return;
        };

        // Never perform a potentially blocking UDP send on the highest-
        // priority capture thread. A one-element channel coalesces a 500/1000
        // Hz hardware pointer stream; the video sender transmits it alongside
        // the next network-service iteration.
        match self.flags.cursor_updates.try_send(position) {
            Ok(()) => {
                self.cursor_queued_at = Some(Instant::now());
            }
            Err(mpsc::TrySendError::Full(_)) => {
                // The queued item is older than self.cursor_position. Leave
                // this due so the timeout path retries the newest coordinate
                // after the sender drains the single coalescing slot.
                self.cursor_queued_at = None;
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.cursor_queued_at = Some(Instant::now());
            }
        }
    }

    fn process_texture(
        &mut self,
        source_texture: &ID3D11Texture2D,
        source_width: u32,
        source_height: u32,
        source_generation: u64,
    ) -> Result<(), String> {
        if !self.workers_running.load(Ordering::Acquire) {
            // Leave the first concrete failure in the shared slot. The
            // capture-loop arbitration point classifies it together with
            // GetDeviceRemovedReason; consuming it here would reduce a
            // generic process-texture String to Fatal and could hide a GPU
            // reset reported concurrently by the worker.
            return Err("D3D11/NVENC worker stopped".to_owned());
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
        if self
            .preprocess_in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_PRE_NVENC_IN_FLIGHT).then_some(count + 1)
            })
            .is_err()
        {
            // Keep only two frames in the copy/convert/submit stages even
            // though the deeper slot ring can retain completed submissions.
            // Otherwise a loaded MapInputResource lets capture enqueue many
            // redundant full-screen copies that compete with the game.
            self.flags
                .metrics
                .slot_busy_skips
                .fetch_add(1, Ordering::Relaxed);
            self.flags
                .metrics
                .preprocess_busy_skips
                .fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let Ok(slot) = self.free_slots.try_recv() else {
            // Every slot is still being processed. Never block desktop
            // acquisition; sampling a newer capture is lower latency.
            self.flags
                .metrics
                .slot_busy_skips
                .fetch_add(1, Ordering::Relaxed);
            self.flags
                .metrics
                .no_free_slot_skips
                .fetch_add(1, Ordering::Relaxed);
            self.preprocess_in_flight.fetch_sub(1, Ordering::AcqRel);
            return Ok(());
        };
        // Copy the DXGI-owned surface into an app-owned BGRA slot before
        // releasing the duplication frame. The submission worker converts it
        // to NV12; NvEncMapInputResource provides the D3D11 synchronization
        // guarantee for both GPU commands.
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
                    self.preprocess_in_flight.fetch_sub(1, Ordering::AcqRel);
                    return Err(error);
                }
            }
        }
        let capture_texture = self.capture_textures[slot]
            .as_ref()
            .expect("capture texture was initialized")
            .texture
            .clone();
        let copy_issue_started = Instant::now();
        unsafe {
            self.device_context
                .CopyResource(&capture_texture, source_texture);
        }
        // CopyResource is asynchronous, and the D3D runtime retains command
        // resources until execution. The protected immediate-context order
        // keeps the later video-processor blit behind this copy, while
        // NvEncMapInputResource supplies the eventual completion wait. A
        // per-frame Flush adds driver overhead but is not a completion fence.
        let copy_issue_us = elapsed_microseconds(copy_issue_started);

        let timestamp_us = self
            .flags
            .stream_started
            .elapsed()
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        let prepared = PreparedHardwareFrame {
            slot,
            source_generation,
            texture: capture_texture,
            source_width,
            source_height,
            timestamp_us,
            captured_at: capture_started,
            copy_issue_us,
        };
        match self
            .prepared_tx
            .as_ref()
            .map(|sender| sender.try_send(prepared))
        {
            Some(Ok(())) => Ok(()),
            Some(Err(mpsc::TrySendError::Full(_))) => {
                // The bounded preprocessing admission gate makes this
                // unreachable. Do not return the slot if the invariant is
                // ever broken: its asynchronous copy may still be in flight.
                self.flags.force_keyframe.store(true, Ordering::Release);
                self.preprocess_in_flight.fetch_sub(1, Ordering::AcqRel);
                Err("D3D11 preprocessing queue invariant was violated".to_owned())
            }
            Some(Err(mpsc::TrySendError::Disconnected(_))) | None => {
                self.flags.force_keyframe.store(true, Ordering::Release);
                self.preprocess_in_flight.fetch_sub(1, Ordering::AcqRel);
                Err("D3D11 conversion/NVENC submission worker disconnected".to_owned())
            }
        }
    }

    fn take_worker_failure(&self) -> Option<String> {
        self.worker_failure.lock().unwrap().take()
    }
}

impl Drop for HardwareCapture {
    fn drop(&mut self) {
        {
            // Serialize retirement with the completion worker's final
            // progress/send commit. Once this store becomes visible, a late
            // detached completion from this session cannot be counted or
            // emitted into its replacement.
            let _activity = self.flags.nvenc_activity.lock().unwrap();
            self.workers_running.store(false, Ordering::Release);
        }
        self.prepared_tx.take();
        if let Some(handle) = self.conversion_handle.take() {
            match self.conversion_done.recv_timeout(WORKER_SHUTDOWN_TIMEOUT) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = handle.join();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.encoder.poison();
                    warn!(
                        "D3D11 conversion worker did not stop within {} ms; detaching the poisoned driver worker",
                        WORKER_SHUTDOWN_TIMEOUT.as_millis()
                    );
                }
            }
        }
        if let Some(handle) = self.submission_handle.take() {
            match self.submission_done.recv_timeout(WORKER_SHUTDOWN_TIMEOUT) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = handle.join();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.encoder.poison();
                    warn!(
                        "D3D11/NVENC submission worker did not stop within {} ms; detaching the poisoned driver worker",
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
fn spawn_d3d_conversion(
    device_context: ID3D11DeviceContext,
    encoder: Arc<NvencEncoder>,
    prepared_frames: mpsc::Receiver<PreparedHardwareFrame>,
    converted_frames: mpsc::SyncSender<ConvertedHardwareFrame>,
    free_slots: mpsc::SyncSender<usize>,
    force_keyframe: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    workers_running: Arc<AtomicBool>,
    preprocess_in_flight: Arc<AtomicUsize>,
    metrics: Arc<WindowsCaptureMetrics>,
    worker_failure: Arc<Mutex<Option<String>>>,
    frames_per_second: u32,
    mut scalers: Vec<Option<D3dScaler>>,
    conversion_permits: mpsc::Receiver<()>,
    conversion_done: mpsc::Sender<()>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        raise_current_thread_priority("D3D11 conversion", THREAD_PRIORITY_HIGHEST);
        let max_prepared_frame_age = Duration::from_secs_f64(
            MAX_PREPARED_FRAME_AGE_INTERVALS / f64::from(frames_per_second.max(1)),
        );

        'worker: loop {
            if !running.load(Ordering::Relaxed) || !workers_running.load(Ordering::Acquire) {
                break;
            }
            match conversion_permits.recv_timeout(Duration::from_millis(10)) {
                Ok(()) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }

            // The permit remains owned until one converted frame is handed
            // off. While submission is busy, prepared desktop copies collect
            // here without issuing video-processor work. Once the permit is
            // returned, discard those older samples and convert only the most
            // recent one. Reusing their slots is safe because every copy and
            // later use is ordered through the protected immediate context.
            loop {
                if !running.load(Ordering::Relaxed) || !workers_running.load(Ordering::Acquire) {
                    break 'worker;
                }
                let prepared = match prepared_frames.recv_timeout(Duration::from_millis(10)) {
                    Ok(prepared) => prepared,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break 'worker,
                };
                let prepared = retain_newest_queued(&prepared_frames, prepared, |discarded| {
                    metrics.stale_frames.fetch_add(1, Ordering::Relaxed);
                    let _ = free_slots.try_send(discarded.slot);
                    preprocess_in_flight.fetch_sub(1, Ordering::AcqRel);
                });
                let slot = prepared.slot;
                if !running.load(Ordering::Relaxed) || !workers_running.load(Ordering::Acquire) {
                    let _ = free_slots.try_send(slot);
                    preprocess_in_flight.fetch_sub(1, Ordering::AcqRel);
                    break 'worker;
                }
                if prepared.captured_at.elapsed() > max_prepared_frame_age {
                    // Keep the permit and wait for a fresh sample rather than
                    // waking submission with work that already missed its
                    // two-frame latency budget.
                    metrics.stale_frames.fetch_add(1, Ordering::Relaxed);
                    let _ = free_slots.try_send(slot);
                    preprocess_in_flight.fetch_sub(1, Ordering::AcqRel);
                    continue;
                }
                let converted = match convert_prepared_frame(
                    &device_context,
                    &encoder,
                    &mut scalers,
                    prepared,
                    frames_per_second,
                ) {
                    Ok(converted) => converted,
                    Err(error) => {
                        let report_failure = publish_first_worker_failure(
                            &running,
                            &workers_running,
                            &worker_failure,
                            error.clone(),
                        );
                        let _ = free_slots.try_send(slot);
                        preprocess_in_flight.fetch_sub(1, Ordering::AcqRel);
                        if report_failure {
                            warn!("D3D11 conversion failed: {error}");
                            force_keyframe.store(true, Ordering::Release);
                        }
                        break 'worker;
                    }
                };
                match converted_frames.try_send(converted) {
                    Ok(()) => break,
                    Err(mpsc::TrySendError::Full(_)) => {
                        let report_failure = publish_first_worker_failure(
                            &running,
                            &workers_running,
                            &worker_failure,
                            "D3D11 conversion-ahead queue invariant was violated".to_owned(),
                        );
                        let _ = free_slots.try_send(slot);
                        preprocess_in_flight.fetch_sub(1, Ordering::AcqRel);
                        if report_failure {
                            force_keyframe.store(true, Ordering::Release);
                        }
                        break 'worker;
                    }
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        let report_failure = publish_first_worker_failure(
                            &running,
                            &workers_running,
                            &worker_failure,
                            "NVENC submission worker disconnected".to_owned(),
                        );
                        let _ = free_slots.try_send(slot);
                        preprocess_in_flight.fetch_sub(1, Ordering::AcqRel);
                        if report_failure {
                            force_keyframe.store(true, Ordering::Release);
                        }
                        break 'worker;
                    }
                }
            }
        }
        let _ = conversion_done.send(());
    })
}

#[allow(clippy::too_many_arguments)]
fn spawn_nvenc_submission(
    encoder: Arc<NvencEncoder>,
    converted_frames: mpsc::Receiver<ConvertedHardwareFrame>,
    completion_tx: mpsc::SyncSender<PendingNvencFrame>,
    free_slots: mpsc::SyncSender<usize>,
    force_keyframe: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    workers_running: Arc<AtomicBool>,
    preprocess_in_flight: Arc<AtomicUsize>,
    nvenc_pending: Arc<AtomicUsize>,
    metrics: Arc<WindowsCaptureMetrics>,
    nvenc_activity: Arc<Mutex<NvencActivityState>>,
    worker_failure: Arc<Mutex<Option<String>>>,
    next_frame_id: Arc<AtomicU64>,
    frames_per_second: u32,
    conversion_permits: mpsc::SyncSender<()>,
    submission_done: mpsc::Sender<()>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        raise_current_thread_priority("NVENC submission", THREAD_PRIORITY_HIGHEST);
        // This is the encoded-stream sequence, not the capture-attempt
        // sequence. Frames discarded before NVENC must not create a false
        // reference gap at the sender.
        let busy_sleeper = PreciseSleeper::new();
        let max_converted_frame_age = Duration::from_secs_f64(
            MAX_PREPARED_FRAME_AGE_INTERVALS / f64::from(frames_per_second.max(1)),
        );

        loop {
            if !running.load(Ordering::Relaxed) || !workers_running.load(Ordering::Acquire) {
                break;
            }
            let converted = match converted_frames.recv_timeout(Duration::from_millis(10)) {
                Ok(converted) => converted,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            // Dequeuing empties the sole conversion-ahead slot. Return its
            // permit before MapInputResource so the next newest desktop sample
            // can be converted in parallel with this submission, but no more
            // than one video-processor blit can queue ahead under GPU load.
            if let Err(error) = conversion_permits.try_send(()) {
                let error = match error {
                    mpsc::TrySendError::Full(()) => {
                        "D3D11 conversion-ahead permit invariant was violated".to_owned()
                    }
                    mpsc::TrySendError::Disconnected(()) => {
                        "D3D11 conversion worker disconnected".to_owned()
                    }
                };
                let report_failure = publish_first_worker_failure(
                    &running,
                    &workers_running,
                    &worker_failure,
                    error,
                );
                let _ = free_slots.try_send(converted.slot);
                preprocess_in_flight.fetch_sub(1, Ordering::AcqRel);
                if report_failure {
                    force_keyframe.store(true, Ordering::Release);
                }
                break;
            }
            let slot = converted.slot;
            if !running.load(Ordering::Relaxed) || !workers_running.load(Ordering::Acquire) {
                let _ = free_slots.try_send(slot);
                preprocess_in_flight.fetch_sub(1, Ordering::AcqRel);
                break;
            }
            if converted.captured_at.elapsed() > max_converted_frame_age {
                metrics.stale_frames.fetch_add(1, Ordering::Relaxed);
                let _ = free_slots.try_send(slot);
                preprocess_in_flight.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let pending = match submit_converted_frame(
                &encoder,
                converted,
                &force_keyframe,
                &metrics,
                &next_frame_id,
                &busy_sleeper,
            ) {
                Ok(Some(pending)) => pending,
                Ok(None) => {
                    let mut activity = nvenc_activity.lock().unwrap();
                    activity.record_busy(Instant::now());
                    let _ = metrics.nvenc_busy_activity.fetch_update(
                        Ordering::AcqRel,
                        Ordering::Acquire,
                        |count| Some(count.saturating_add(1)),
                    );
                    drop(activity);
                    metrics.slot_busy_skips.fetch_add(1, Ordering::Relaxed);
                    let _ = free_slots.try_send(slot);
                    preprocess_in_flight.fetch_sub(1, Ordering::AcqRel);
                    continue;
                }
                Err(error) => {
                    // A fatal submission error may have happened after mapping
                    // the resource. Stop admission and retire this ownership
                    // interval instead of racing capture with an unproven
                    // unmap. Clean BUSY rejection is the only submit path that
                    // returns a slot to the ring.
                    preprocess_in_flight.fetch_sub(1, Ordering::AcqRel);
                    if publish_first_worker_failure(
                        &running,
                        &workers_running,
                        &worker_failure,
                        error.clone(),
                    ) {
                        warn!("NVENC submission failed: {error}");
                        force_keyframe.store(true, Ordering::Release);
                    }
                    break;
                }
            };
            // NVENC now owns the mapped texture until completion. Advance the
            // recovery clock at acceptance and publish the pending state
            // before ending the pre-NVENC interval. Holding the activity lock
            // through both counters gives the watchdog one coherent ownership
            // transition and the full documented completion budget.
            {
                let mut activity = nvenc_activity.lock().unwrap();
                activity.record_submission(Instant::now());
                nvenc_pending.fetch_add(1, Ordering::AcqRel);
                preprocess_in_flight.fetch_sub(1, Ordering::AcqRel);
            }
            if completion_tx.send(pending).is_err() {
                nvenc_pending.fetch_sub(1, Ordering::AcqRel);
                // A submitted resource remains driver-owned until completion;
                // do not unmap or recycle its slot after the output worker dies.
                encoder.poison();
                if publish_first_worker_failure(
                    &running,
                    &workers_running,
                    &worker_failure,
                    "NVENC completion worker disconnected".to_owned(),
                ) {
                    force_keyframe.store(true, Ordering::Release);
                }
                break;
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
    workers_running: Arc<AtomicBool>,
    nvenc_pending: Arc<AtomicUsize>,
    completed_source_generation: Arc<AtomicU64>,
    metrics: Arc<WindowsCaptureMetrics>,
    nvenc_activity: Arc<Mutex<NvencActivityState>>,
    worker_failure: Arc<Mutex<Option<String>>>,
    hardware_progress: Arc<AtomicU64>,
    events: mpsc::Sender<SessionEvent>,
    completion_done: mpsc::Sender<()>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        raise_current_thread_priority("NVENC completion", THREAD_PRIORITY_HIGHEST);
        'worker: loop {
            let pending = match pending_frames.recv_timeout(Duration::from_millis(10)) {
                Ok(pending) => pending,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let frame = loop {
                // Drain every submitted sample even during normal shutdown.
                // NVENC owns the mapped resource until its event fires, so
                // abandoning queued samples would make clean destruction
                // unsafe. Wait for the remaining stall budget in one kernel
                // wait instead of waking this highest-priority thread every
                // two milliseconds while the GPU is busy.
                let Some(remaining) =
                    NVENC_COMPLETION_STALL_TIMEOUT.checked_sub(pending.encode_started.elapsed())
                else {
                    let error = format!(
                        "NVENC frame {} did not complete within {} ms",
                        pending.frame_id,
                        NVENC_COMPLETION_STALL_TIMEOUT.as_millis()
                    );
                    encoder.poison();
                    force_keyframe.store(true, Ordering::Release);
                    let _ = publish_first_worker_failure(
                        &running,
                        &workers_running,
                        &worker_failure,
                        error,
                    );
                    break 'worker;
                };
                match encoder.try_complete(&pending, wait_timeout_millis(remaining)) {
                    Ok(Some(frame)) => break frame,
                    Ok(None) => {}
                    Err(error) => {
                        encoder.poison();
                        if publish_first_worker_failure(
                            &running,
                            &workers_running,
                            &worker_failure,
                            error.clone(),
                        ) {
                            warn!("Asynchronous NVENC completion failed: {error}");
                            force_keyframe.store(true, Ordering::Release);
                        }
                        break 'worker;
                    }
                }
            };
            // Serialize the progress/generation commit with session
            // retirement. Never hold this mutex across the blocking sender
            // handoff: capture samples it every Acquire cycle, and transport
            // backpressure must not stall DDA acquisition.
            let (pipeline_active, first_hardware_frame) = {
                let mut activity = nvenc_activity.lock().unwrap();
                let pipeline_active =
                    running.load(Ordering::Relaxed) && workers_running.load(Ordering::Acquire);
                let first_hardware_frame = if pipeline_active {
                    activity.record_completion(Instant::now());
                    metrics.nvenc_completions.fetch_add(1, Ordering::Relaxed);
                    completed_source_generation
                        .fetch_max(pending.source_generation, Ordering::AcqRel);
                    hardware_progress.fetch_add(1, Ordering::AcqRel) == 0
                } else {
                    false
                };
                (pipeline_active, first_hardware_frame)
            };
            if first_hardware_frame {
                let _ = events.send(SessionEvent::CaptureReady);
            }
            nvenc_pending.fetch_sub(1, Ordering::AcqRel);
            // Completion also ends this slot's ownership interval. Recycle it
            // before waiting for sender capacity; `frame` now owns its copied
            // bitstream and no longer references either per-slot NVENC buffer.
            let _ = free_slots.try_send(pending.slot);
            // A blocking channel handoff wakes as soon as the sender consumes
            // capacity. Polling with `sleep(1 ms)` can oversleep by a full
            // Windows scheduler quantum and cap an otherwise healthy stream
            // near the reported sub-50-FPS range whenever this queue fills.
            // The slot is already safe and recycled, and normal shutdown
            // keeps draining the sender or disconnects this receiver, so this
            // bounded channel wait cannot retain an NVENC resource or prevent
            // teardown.
            // The activity-locked progress commit is the linearization point:
            // once pipeline_active is true, always attempt the matching
            // sender handoff. A worker failure published just after the
            // commit must not leave completed progress/generation accounting
            // for a frame that was silently suppressed here.
            let accepted = pipeline_active && encoded_frames.send(frame).is_ok();
            if pipeline_active
                && !accepted
                && publish_first_worker_failure(
                    &running,
                    &workers_running,
                    &worker_failure,
                    "video sender disconnected".to_owned(),
                )
            {
                force_keyframe.store(true, Ordering::Release);
            }
        }
        let _ = completion_done.send(());
    })
}

fn convert_prepared_frame(
    device_context: &ID3D11DeviceContext,
    encoder: &NvencEncoder,
    scalers: &mut [Option<D3dScaler>],
    prepared: PreparedHardwareFrame,
    frames_per_second: u32,
) -> Result<ConvertedHardwareFrame, String> {
    let output_texture = encoder
        .slots
        .get(prepared.slot)
        .ok_or_else(|| format!("Invalid NVENC slot {}", prepared.slot))?
        .texture
        .clone();
    let scaler_needs_rebuild = scalers[prepared.slot].as_ref().is_none_or(|scaler| {
        scaler.source_width != prepared.source_width
            || scaler.source_height != prepared.source_height
    });
    if scaler_needs_rebuild {
        scalers[prepared.slot] = Some(D3dScaler::new(
            device_context,
            &prepared.texture,
            &output_texture,
            prepared.source_width,
            prepared.source_height,
            encoder.width,
            encoder.height,
            frames_per_second,
        )?);
    }
    let convert_issue_started = Instant::now();
    scalers[prepared.slot]
        .as_ref()
        .expect("video processor was initialized")
        .scale()?;
    // Do not force-submit the whole immediate context here. NVIDIA's D3D11
    // interop path orders preceding graphics/video work when the registered
    // texture is mapped, and a per-frame Flush adds significant driver work
    // while a game is already saturating the GPU.
    let convert_issue_us = elapsed_microseconds(convert_issue_started);

    Ok(ConvertedHardwareFrame {
        slot: prepared.slot,
        source_generation: prepared.source_generation,
        timestamp_us: prepared.timestamp_us,
        captured_at: prepared.captured_at,
        copy_issue_us: prepared.copy_issue_us,
        convert_issue_us,
    })
}

fn submit_converted_frame(
    encoder: &NvencEncoder,
    converted: ConvertedHardwareFrame,
    force_keyframe: &AtomicBool,
    metrics: &WindowsCaptureMetrics,
    next_frame_id: &AtomicU64,
    busy_sleeper: &PreciseSleeper,
) -> Result<Option<PendingNvencFrame>, String> {
    let force_idr = force_keyframe.swap(false, Ordering::Relaxed);
    let capture_us = elapsed_microseconds(converted.captured_at);
    let submit_result = match encoder.submit(
        converted.slot,
        force_idr,
        NVENC_BUSY_RETRY_TIMEOUT,
        busy_sleeper,
    ) {
        Ok(result) => result,
        Err(error) => {
            restore_force_keyframe_request(force_keyframe, force_idr);
            return Err(error);
        }
    };
    let NvencSubmitResult::Submitted {
        mapped_resource,
        map_us,
        submit_us,
        submit_api_us,
        submit_busy_wait_us,
        submit_busy_retries,
    } = submit_result
    else {
        restore_force_keyframe_request(force_keyframe, force_idr);
        return Ok(None);
    };
    // The documented completion-stall window begins only after NVENC accepts
    // the picture. A loaded NvEncMapInputResource and the independent BUSY
    // retry window must not consume any part of those two seconds.
    let encode_started = Instant::now();
    metrics.nvenc_submissions.fetch_add(1, Ordering::Relaxed);
    let frame_id = allocate_submitted_frame_id(next_frame_id);

    Ok(Some(PendingNvencFrame {
        slot: converted.slot,
        source_generation: converted.source_generation,
        mapped_resource,
        frame_id,
        timestamp_us: converted.timestamp_us,
        capture_us,
        gpu_wait_us: map_us,
        gpu_lock_us: 0,
        encode_started,
        copy_wait_us: converted.copy_issue_us,
        convert_wait_us: converted.convert_issue_us,
        map_us,
        submit_us,
        submit_api_us,
        submit_busy_wait_us,
        submit_busy_retries,
    }))
}

#[allow(clippy::too_many_arguments)]
fn spawn_hardware_sender(
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    pipeline_running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    peer_epoch: Arc<AtomicU64>,
    force_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    encoded_frames: mpsc::Receiver<EncodedHardwareFrame>,
    cursor_updates: mpsc::Receiver<CursorPosition>,
    active_datagram_size: Arc<AtomicUsize>,
    bitrate_bps: u32,
    metrics: Arc<WindowsCaptureMetrics>,
    pending_server_stats: PendingServerStats,
) -> JoinHandle<()> {
    thread::spawn(move || {
        raise_current_thread_priority("video sender", THREAD_PRIORITY_ABOVE_NORMAL);
        let mut pacer = DatagramPacer::new(bitrate_bps);
        let mut packet = Vec::with_capacity(MAX_DATAGRAM_SIZE);
        let mut cursor_packet = Vec::with_capacity(14);
        let mut stats = HardwareSenderStats::new();
        let mut last_sent_frame = None::<u64>;
        let mut waiting_for_keyframe = true;
        let mut sender_peer = None::<SocketAddr>;
        let mut sender_peer_epoch = u64::MAX;

        while running.load(Ordering::Relaxed) && pipeline_running.load(Ordering::Acquire) {
            send_pending_cursor(&socket, &active_peer, &cursor_updates, &mut cursor_packet);
            let frame = match encoded_frames.recv_timeout(Duration::from_millis(20)) {
                Ok(frame) => frame,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    report_sender_stats(&events, &mut stats, &metrics, &pending_server_stats);
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let encode_queue_elapsed = frame.encoded_at.elapsed();
            let peer = *active_peer.lock().unwrap();
            let current_peer_epoch = peer_epoch.load(Ordering::Acquire);
            if peer != sender_peer || current_peer_epoch != sender_peer_epoch {
                sender_peer = peer;
                sender_peer_epoch = current_peer_epoch;
                waiting_for_keyframe = true;
                last_sent_frame = None;
                // The frame already in hand can satisfy the new peer when it
                // is an IDR. Do not leave a redundant static-surface refresh
                // armed after successfully sending that keyframe.
                if !frame.is_keyframe {
                    force_keyframe.store(true, Ordering::Release);
                }
            }
            if last_sent_frame.is_some_and(|last| frame.frame_id != last.wrapping_add(1)) {
                waiting_for_keyframe = true;
                if !frame.is_keyframe {
                    force_keyframe.store(true, Ordering::Release);
                }
            }
            // Once NVENC has accepted a frame it participates in the H.264
            // reference chain. Dropping it here makes every following P-frame
            // undecodable and previously caused a self-sustaining IDR storm.
            // The finite slot ring bounds this post-encode backlog. Pre-NVENC
            // age checks and newest-frame coalescing prevent the adaptive ring
            // from turning GPU pressure into an unbounded capture queue.
            if waiting_for_keyframe && !frame.is_keyframe {
                force_keyframe.store(true, Ordering::Release);
                continue;
            }
            if frame.is_keyframe {
                waiting_for_keyframe = false;
            }
            let Some(peer) = peer else {
                waiting_for_keyframe = true;
                last_sent_frame = None;
                continue;
            };
            let datagram_size = active_datagram_size.load(Ordering::Acquire);
            let fragments = match fragment_video_frame_with_datagram_size(
                frame.frame_id,
                frame.timestamp_us,
                frame.is_keyframe,
                &frame.data,
                datagram_size,
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
            let pacing_batch_datagrams = video_pacing_batch_datagrams(datagram_size);
            pacer.begin_frame();
            for (fragment_index, fragment) in fragments.into_iter().enumerate() {
                if fragment_index > 0 && fragment_index % pacing_batch_datagrams == 0 {
                    pacer.wait();
                }
                Packet::Video(fragment).encode(&mut packet);
                if let Err(error) = socket.send_to(&packet, peer) {
                    warn!("Video send to {peer} failed: {error}");
                    metrics.video_send_failures.fetch_add(1, Ordering::Relaxed);
                    frame_sent = false;
                    force_keyframe.store(true, Ordering::Release);
                    break;
                }
                pacer.account(packet.len());
                stats.bytes = stats.bytes.saturating_add(packet.len() as u64);
            }
            if frame_sent {
                last_sent_frame = Some(frame.frame_id);
                stats.frames = stats.frames.saturating_add(1);
                stats.capture_us = stats.capture_us.saturating_add(frame.capture_us);
                stats.gpu_wait_us = stats.gpu_wait_us.saturating_add(frame.gpu_wait_us);
                stats.gpu_lock_us = stats.gpu_lock_us.saturating_add(frame.gpu_lock_us);
                stats.encode_us = stats.encode_us.saturating_add(frame.encode_us);
                stats.copy_wait_us = stats.copy_wait_us.saturating_add(frame.copy_wait_us);
                stats.convert_wait_us = stats.convert_wait_us.saturating_add(frame.convert_wait_us);
                stats.map_us = stats.map_us.saturating_add(frame.map_us);
                stats.submit_us = stats.submit_us.saturating_add(frame.submit_us);
                stats.submit_api_us = stats.submit_api_us.saturating_add(frame.submit_api_us);
                stats.submit_busy_wait_us = stats
                    .submit_busy_wait_us
                    .saturating_add(frame.submit_busy_wait_us);
                stats.submit_busy_retries = stats
                    .submit_busy_retries
                    .saturating_add(frame.submit_busy_retries);
                stats.completion_wait_us = stats
                    .completion_wait_us
                    .saturating_add(frame.completion_wait_us);
                stats.bitstream_us = stats.bitstream_us.saturating_add(frame.bitstream_us);
                stats.bitstream_lock_us = stats
                    .bitstream_lock_us
                    .saturating_add(frame.bitstream_lock_us);
                stats.bitstream_copy_us = stats
                    .bitstream_copy_us
                    .saturating_add(frame.bitstream_copy_us);
                stats.bitstream_unlock_us = stats
                    .bitstream_unlock_us
                    .saturating_add(frame.bitstream_unlock_us);
                stats.unmap_us = stats.unmap_us.saturating_add(frame.unmap_us);
                stats.send_us = stats.send_us.saturating_add(
                    send_started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64,
                );
                stats.encode_queue_us = stats.encode_queue_us.saturating_add(
                    encode_queue_elapsed.as_micros().min(u128::from(u64::MAX)) as u64,
                );
            } else {
                waiting_for_keyframe = true;
                last_sent_frame = None;
            }
            report_sender_stats(&events, &mut stats, &metrics, &pending_server_stats);
        }
    })
}

fn send_pending_cursor(
    socket: &UdpSocket,
    active_peer: &Mutex<Option<SocketAddr>>,
    cursor_updates: &mpsc::Receiver<CursorPosition>,
    packet: &mut Vec<u8>,
) {
    let Ok(mut position) = cursor_updates.try_recv() else {
        return;
    };
    while let Ok(newer) = cursor_updates.try_recv() {
        position = newer;
    }
    let Some(peer) = *active_peer.lock().unwrap() else {
        return;
    };
    Packet::CursorPosition(position).encode(packet);
    if let Err(error) = socket.send_to(packet, peer) {
        warn!("Cursor position send to {peer} failed: {error}");
    }
}

struct HardwareSenderStats {
    started: Instant,
    frames: u64,
    bytes: u64,
    capture_us: u64,
    gpu_wait_us: u64,
    gpu_lock_us: u64,
    encode_us: u64,
    send_us: u64,
    encode_queue_us: u64,
    copy_wait_us: u64,
    convert_wait_us: u64,
    map_us: u64,
    submit_us: u64,
    submit_api_us: u64,
    submit_busy_wait_us: u64,
    submit_busy_retries: u64,
    completion_wait_us: u64,
    bitstream_us: u64,
    bitstream_lock_us: u64,
    bitstream_copy_us: u64,
    bitstream_unlock_us: u64,
    unmap_us: u64,
}

impl HardwareSenderStats {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            frames: 0,
            bytes: 0,
            capture_us: 0,
            gpu_wait_us: 0,
            gpu_lock_us: 0,
            encode_us: 0,
            send_us: 0,
            encode_queue_us: 0,
            copy_wait_us: 0,
            convert_wait_us: 0,
            map_us: 0,
            submit_us: 0,
            submit_api_us: 0,
            submit_busy_wait_us: 0,
            submit_busy_retries: 0,
            completion_wait_us: 0,
            bitstream_us: 0,
            bitstream_lock_us: 0,
            bitstream_copy_us: 0,
            bitstream_unlock_us: 0,
            unmap_us: 0,
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }
}

fn report_sender_stats(
    events: &mpsc::Sender<SessionEvent>,
    stats: &mut HardwareSenderStats,
    metrics: &WindowsCaptureMetrics,
    pending_server_stats: &PendingServerStats,
) {
    let elapsed = stats.started.elapsed();
    if elapsed < Duration::from_secs(1) {
        return;
    }
    let seconds = elapsed.as_secs_f32();
    let source_frames = metrics.source_frames.swap(0, Ordering::Relaxed);
    publish_server_stats(
        events,
        pending_server_stats,
        ServerStats {
            fps: stats.frames as f32 / seconds,
            source_fps: source_frames as f32 / seconds,
            megabits_per_second: stats.bytes as f32 * 8.0 / seconds / 1_000_000.0,
            capture_ms: average_milliseconds(stats.capture_us, stats.frames),
            gpu_wait_ms: average_milliseconds(stats.gpu_wait_us, stats.frames),
            gpu_lock_ms: average_milliseconds(stats.gpu_lock_us, stats.frames),
            encode_ms: average_milliseconds(stats.encode_us, stats.frames),
            send_ms: average_milliseconds(stats.send_us, stats.frames),
            encode_queue_ms: average_milliseconds(stats.encode_queue_us, stats.frames),
            copy_wait_ms: average_milliseconds(stats.copy_wait_us, stats.frames),
            convert_wait_ms: average_milliseconds(stats.convert_wait_us, stats.frames),
            map_ms: average_milliseconds(stats.map_us, stats.frames),
            submit_ms: average_milliseconds(stats.submit_us, stats.frames),
            submit_api_ms: average_milliseconds(stats.submit_api_us, stats.frames),
            submit_busy_wait_ms: average_milliseconds(stats.submit_busy_wait_us, stats.frames),
            submit_busy_retries: stats.submit_busy_retries,
            completion_wait_ms: average_milliseconds(stats.completion_wait_us, stats.frames),
            bitstream_ms: average_milliseconds(stats.bitstream_us, stats.frames),
            bitstream_lock_ms: average_milliseconds(stats.bitstream_lock_us, stats.frames),
            bitstream_copy_ms: average_milliseconds(stats.bitstream_copy_us, stats.frames),
            bitstream_unlock_ms: average_milliseconds(stats.bitstream_unlock_us, stats.frames),
            unmap_ms: average_milliseconds(stats.unmap_us, stats.frames),
            gpu_priority_class: metrics.process_gpu_priority.load(Ordering::Acquire),
            nvenc_buffer_count: metrics.nvenc_buffer_count.load(Ordering::Acquire),
            video_send_failures: metrics.video_send_failures.swap(0, Ordering::Relaxed),
            dxgi_timeouts: metrics.dxgi_timeouts.swap(0, Ordering::Relaxed),
            dxgi_backlog: metrics.dxgi_backlog.swap(0, Ordering::Relaxed),
            dxgi_backlog_max: metrics.dxgi_backlog_max.swap(0, Ordering::Relaxed),
            pacing_skips: metrics.pacing_skips.swap(0, Ordering::Relaxed),
            slot_busy_skips: metrics.slot_busy_skips.swap(0, Ordering::Relaxed),
            cursor_only_frames: metrics.cursor_only_frames.swap(0, Ordering::Relaxed),
            stale_frames: metrics.stale_frames.swap(0, Ordering::Relaxed),
            preprocess_busy_skips: metrics.preprocess_busy_skips.swap(0, Ordering::Relaxed),
            no_free_slot_skips: metrics.no_free_slot_skips.swap(0, Ordering::Relaxed),
            nvenc_submissions: metrics.nvenc_submissions.swap(0, Ordering::Relaxed),
            nvenc_completions: metrics.nvenc_completions.swap(0, Ordering::Relaxed),
        },
    );
    stats.reset();
}

fn average_milliseconds(total_us: u64, samples: u64) -> f32 {
    if samples == 0 {
        0.0
    } else {
        total_us as f32 / samples as f32 / 1_000.0
    }
}

fn elapsed_microseconds(started: Instant) -> u64 {
    started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64
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
        // and a video-processor input on the encoder worker.
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
        // NV12 is written by the fixed-function video processor and registered
        // directly with NVENC. D3D11_BIND_VIDEO_ENCODER belongs to the separate
        // D3D11 video-encoder API and must not be combined here.
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
        source_texture: &ID3D11Texture2D,
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
            video_device
                .CreateVideoProcessorInputView(
                    source_texture,
                    &enumerator,
                    &input_description,
                    Some(&mut input_view),
                )
                .map_err(|error| format!("Unable to create D3D11 scaler input view: {error}"))?;
        }
        let input_view =
            input_view.ok_or_else(|| "D3D11 returned no scaler input view".to_owned())?;

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
            // Desktop frames are already progressive, clean RGB. Drivers may
            // otherwise enable denoise, edge enhancement, stabilization, or
            // other automatic processing that adds hidden GPU work to every
            // BGRA→NV12 conversion and competes with a loaded game.
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, FALSE);
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
            video_context,
            processor,
            input_view,
            output_view,
        })
    }

    fn scale(&self) -> Result<(), String> {
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: TRUE,
            pInputSurface: ManuallyDrop::new(Some(self.input_view.clone())),
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
        buffer_count: usize,
    ) -> Result<Self, String> {
        let api = NvencApi::load()?;
        let mut result = Self {
            api,
            encoder: ptr::null_mut(),
            slots: Vec::with_capacity(buffer_count),
            width,
            height,
            poisoned: AtomicBool::new(false),
        };
        result.open(device)?;
        result.initialize(frames_per_second, bitrate_bps)?;
        for _ in 0..buffer_count {
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
        configure_single_h264_slice(&mut h264);
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
        // Keep bitstream output in system memory for asynchronous DirectX
        // encoding. The driver may use the application's DirectX device from
        // the output thread, so ID3D11Multithread protects it against capture
        // and submission context access.
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

    fn submit(
        &self,
        slot_index: usize,
        force_idr: bool,
        busy_timeout: Duration,
        busy_sleeper: &PreciseSleeper,
    ) -> Result<NvencSubmitResult, String> {
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
        let map_started = Instant::now();
        nvenc_status(
            unsafe { map_input(self.encoder, &mut mapped) },
            "map D3D11 texture for NVENC",
        )?;
        let map_us = elapsed_microseconds(map_started);

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
        let submit_started = Instant::now();
        let submit_clock = Instant::now();
        let attempt = submit_nvenc_picture_with_retry(
            || unsafe { encode_picture(self.encoder, &mut picture) },
            |delay| busy_sleeper.sleep(delay),
            || submit_clock.elapsed(),
            busy_timeout,
        );
        let submit_us = elapsed_microseconds(submit_started);
        if attempt.status == NVENCSTATUS::NV_ENC_ERR_ENCODER_BUSY {
            if let Err(error) = self.unmap(mapped.mappedResource) {
                self.poison();
                return Err(error);
            }
            return Ok(NvencSubmitResult::Busy);
        }
        if let Err(submit_error) = nvenc_status(attempt.status, "submit asynchronous NVENC frame") {
            if let Err(unmap_error) = self.unmap(mapped.mappedResource) {
                self.poison();
                return Err(format!(
                    "{submit_error}; the rejected NVENC input also could not be released: {unmap_error}"
                ));
            }
            return Err(submit_error);
        }
        Ok(NvencSubmitResult::Submitted {
            mapped_resource: mapped.mappedResource,
            map_us,
            submit_us,
            submit_api_us: attempt.api_us,
            submit_busy_wait_us: attempt.busy_wait_us,
            submit_busy_retries: attempt.busy_retries,
        })
    }

    fn try_complete(
        &self,
        pending: &PendingNvencFrame,
        wait_ms: u32,
    ) -> Result<Option<EncodedHardwareFrame>, String> {
        let slot = self
            .slots
            .get(pending.slot)
            .ok_or_else(|| format!("Invalid NVENC completion slot {}", pending.slot))?;
        let wait = unsafe { WaitForSingleObject(slot.completion_event, wait_ms) };
        if wait == WAIT_TIMEOUT {
            return Ok(None);
        }
        if wait != WAIT_OBJECT_0 {
            self.poison();
            return Err(format!("NVENC completion-event wait failed ({wait:?})"));
        }
        let completion_wait_us = elapsed_microseconds(pending.encode_started);
        let gpu_lock_us = pending.gpu_lock_us;
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
        let bitstream_started = Instant::now();
        let bitstream_lock_started = Instant::now();
        if let Err(error) = nvenc_status(
            unsafe { lock_bitstream(self.encoder, &mut lock) },
            "lock completed NVENC bitstream",
        ) {
            self.poison();
            return Err(error);
        }
        let bitstream_lock_us = elapsed_microseconds(bitstream_lock_started);
        let bitstream_copy_started = Instant::now();
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
        let bitstream_copy_us = elapsed_microseconds(bitstream_copy_started);
        let picture_type = lock.pictureType;
        let bitstream_unlock_started = Instant::now();
        let unlock_result = nvenc_status(
            unsafe { unlock(self.encoder, slot.bitstream) },
            "unlock NVENC bitstream",
        );
        if let Err(error) = unlock_result {
            self.poison();
            return Err(error);
        }
        let bitstream_unlock_us = elapsed_microseconds(bitstream_unlock_started);
        let unmap_started = Instant::now();
        if let Err(error) = self.unmap(pending.mapped_resource) {
            self.poison();
            return Err(error);
        }
        let unmap_us = elapsed_microseconds(unmap_started);
        let bitstream_us = elapsed_microseconds(bitstream_started);
        if data.is_empty() {
            return Err("NVENC completed an empty frame".to_owned());
        }
        let is_keyframe = matches!(
            picture_type,
            NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR | NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I
        );
        Ok(Some(EncodedHardwareFrame {
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
            copy_wait_us: pending.copy_wait_us,
            convert_wait_us: pending.convert_wait_us,
            map_us: pending.map_us,
            submit_us: pending.submit_us,
            submit_api_us: pending.submit_api_us,
            submit_busy_wait_us: pending.submit_busy_wait_us,
            submit_busy_retries: pending.submit_busy_retries,
            completion_wait_us,
            bitstream_us,
            bitstream_lock_us,
            bitstream_copy_us,
            bitstream_unlock_us,
            unmap_us,
            encoded_at: Instant::now(),
        }))
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
                    // A native call timed out or mapped-resource ownership
                    // could not be released, so further driver calls may hang
                    // on the same suspended context. Prompt DEVICE_NOT_EXIST
                    // statuses are not poisoned: NVIDIA requires normal
                    // resource/session cleanup for those. Here, close only
                    // process-owned event handles and let a later session use
                    // a fresh D3D/NVENC device.
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

fn submit_nvenc_picture_with_retry<Encode, Sleep, Now>(
    mut encode: Encode,
    mut sleep: Sleep,
    mut now: Now,
    timeout: Duration,
) -> NvencSubmitAttempt
where
    Encode: FnMut() -> NVENCSTATUS,
    Sleep: FnMut(Duration),
    Now: FnMut() -> Duration,
{
    let busy_started = now();
    let mut api_us = 0_u64;
    let mut busy_wait_us = 0_u64;
    let mut busy_retries = 0_u64;
    loop {
        let api_started = now();
        let status = encode();
        api_us = api_us.saturating_add(duration_microseconds(now().saturating_sub(api_started)));
        if status != NVENCSTATUS::NV_ENC_ERR_ENCODER_BUSY {
            return NvencSubmitAttempt {
                status,
                api_us,
                busy_wait_us,
                busy_retries,
            };
        }

        // NVIDIA documents ENCODER_BUSY as transient under load and requires
        // retrying the same picture after a short delay. Keep the actual API
        // and sleep costs separate: an eventual success previously made the
        // UI report a long submit with "NVENC busy 0".
        let Some(retry_delay) = nvenc_busy_retry_delay(now().saturating_sub(busy_started), timeout)
        else {
            return NvencSubmitAttempt {
                status,
                api_us,
                busy_wait_us,
                busy_retries,
            };
        };
        let wait_started = now();
        sleep(retry_delay);
        busy_wait_us =
            busy_wait_us.saturating_add(duration_microseconds(now().saturating_sub(wait_started)));
        busy_retries = busy_retries.saturating_add(1);
    }
}

fn duration_microseconds(duration: Duration) -> u64 {
    duration.as_micros().min(u128::from(u64::MAX)) as u64
}

fn wait_timeout_millis(duration: Duration) -> u32 {
    duration
        .as_nanos()
        .div_ceil(1_000_000)
        .clamp(1, u128::from(u32::MAX)) as u32
}

fn nvenc_busy_retry_delay(elapsed: Duration, timeout: Duration) -> Option<Duration> {
    timeout
        .checked_sub(elapsed)
        .filter(|remaining| !remaining.is_zero())
        .map(|remaining| remaining.min(NVENC_BUSY_RETRY_DELAY))
}

fn restore_force_keyframe_request(force_keyframe: &AtomicBool, consumed: bool) {
    if consumed {
        force_keyframe.store(true, Ordering::Release);
    }
}

fn allocate_submitted_frame_id(next_frame_id: &AtomicU64) -> u64 {
    next_frame_id.fetch_add(1, Ordering::Relaxed)
}

fn nvenc_recovery_decision(
    no_activity_for: Duration,
    no_required_progress_for: Duration,
    sustained_busy: bool,
    unresolved_busy: bool,
) -> Option<NvencRecoveryDecision> {
    if sustained_busy
        || (unresolved_busy && no_required_progress_for >= NVENC_SUSTAINED_BUSY_TIMEOUT)
    {
        Some(NvencRecoveryDecision::Overloaded)
    } else if no_activity_for >= ACTIVE_ENCODE_STALL_TIMEOUT {
        Some(if unresolved_busy {
            NvencRecoveryDecision::Overloaded
        } else {
            NvencRecoveryDecision::Stalled
        })
    } else {
        None
    }
}

fn nvenc_overload_retry_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(3);
    Duration::from_millis(
        (NVENC_OVERLOAD_RETRY_DELAY_MS << shift).min(NVENC_OVERLOAD_MAX_RETRY_DELAY_MS),
    )
}

fn wait_for_pipeline_retry(running: &AtomicBool, delay: Duration) {
    let started = Instant::now();
    while running.load(Ordering::Relaxed) {
        let Some(remaining) = delay.checked_sub(started.elapsed()) else {
            break;
        };
        thread::sleep(remaining.min(Duration::from_millis(50)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn nvenc_ring_tracks_the_requested_frame_rate_with_bounds() {
        assert_eq!(nvenc_buffer_count(15), MIN_NVENC_BUFFER_COUNT);
        assert_eq!(nvenc_buffer_count(60), 6);
        assert_eq!(nvenc_buffer_count(90), 9);
        assert_eq!(nvenc_buffer_count(120), MAX_NVENC_BUFFER_COUNT);
        assert_eq!(nvenc_buffer_count(u32::MAX), MAX_NVENC_BUFFER_COUNT);
    }

    #[test]
    fn h264_configuration_uses_one_slice_per_picture() {
        let mut config = NV_ENC_CONFIG_H264::default();

        configure_single_h264_slice(&mut config);

        assert_eq!(config.sliceMode, NVENC_H264_NUM_SLICES_MODE);
        assert_eq!(config.sliceModeData, 1);
    }

    #[test]
    fn queued_preprocessing_keeps_only_the_newest_sample() {
        let (sender, receiver) = mpsc::channel();
        sender.send(2_u32).unwrap();
        sender.send(3_u32).unwrap();
        drop(sender);
        let mut discarded = Vec::new();

        let newest = retain_newest_queued(&receiver, 1_u32, |value| discarded.push(value));

        assert_eq!(newest, 3);
        assert_eq!(discarded, [1, 2]);
    }

    #[test]
    fn capture_activity_expires_after_a_static_window() {
        let started = Instant::now();
        let mut window_started = started;
        let mut frames = 0;

        assert!(!update_capture_activity(
            &mut window_started,
            &mut frames,
            started,
            true,
        ));
        assert!(!update_capture_activity(
            &mut window_started,
            &mut frames,
            started + Duration::from_millis(10),
            true,
        ));
        assert!(update_capture_activity(
            &mut window_started,
            &mut frames,
            started + Duration::from_millis(20),
            true,
        ));
        assert!(!update_capture_activity(
            &mut window_started,
            &mut frames,
            started + CAPTURE_ACTIVITY_WINDOW,
            false,
        ));
        assert_eq!(frames, 0);
    }

    #[test]
    fn encoder_watchdog_requires_bootstrap_but_ignores_a_healthy_static_source() {
        assert!(encoder_progress_required(false, false, false, 0, 0));
        assert!(!encoder_progress_required(true, false, false, 0, 0));
        assert!(encoder_progress_required(true, true, false, 0, 0));
        assert!(encoder_progress_required(true, false, true, 0, 0));
        assert!(encoder_progress_required(true, false, false, 1, 0));
        assert!(encoder_progress_required(true, false, false, 0, 1));
    }

    #[test]
    fn pending_static_idr_reacquires_only_after_in_flight_work_is_gone() {
        assert!(current_surface_refresh_required(false, true, 0, 0));
        assert!(current_surface_refresh_required(true, false, 0, 0));
        assert!(!current_surface_refresh_required(false, false, 0, 0));
        assert!(!current_surface_refresh_required(false, true, 1, 0));
        assert!(!current_surface_refresh_required(false, true, 0, 1));
    }

    #[test]
    fn isolated_post_static_work_gets_a_fresh_watchdog_budget() {
        let started = Instant::now();
        let isolated_update = started + Duration::from_secs(60);
        let mut progress_at = None;

        assert_eq!(
            update_required_progress_clock(&mut progress_at, isolated_update, true, false),
            Duration::ZERO
        );
        assert_eq!(progress_at, Some(isolated_update));
        assert_eq!(
            update_required_progress_clock(
                &mut progress_at,
                isolated_update + ACTIVE_ENCODE_STALL_WARNING,
                true,
                false,
            ),
            ACTIVE_ENCODE_STALL_WARNING
        );

        let completion = isolated_update + Duration::from_millis(300);
        assert_eq!(
            update_required_progress_clock(&mut progress_at, completion, true, true),
            Duration::ZERO
        );
        assert_eq!(progress_at, Some(completion));

        assert_eq!(
            update_required_progress_clock(&mut progress_at, completion, false, false),
            Duration::ZERO
        );
        assert_eq!(progress_at, None);
    }

    #[test]
    fn completion_wait_uses_a_ceil_millisecond_deadline() {
        assert_eq!(wait_timeout_millis(Duration::from_nanos(1)), 1);
        assert_eq!(wait_timeout_millis(Duration::from_millis(1)), 1);
        assert_eq!(
            wait_timeout_millis(Duration::from_millis(1) + Duration::from_nanos(1)),
            2
        );
        assert_eq!(wait_timeout_millis(Duration::from_secs(2)), 2_000);
    }

    #[test]
    fn device_loss_classification_covers_dxgi_and_nvenc_statuses() {
        assert!(is_d3d_device_lost_error("HRESULT 0x887a0007"));
        assert!(is_nvenc_device_lost_error(
            "submit failed with NV_ENC_ERR_DEVICE_NOT_EXIST"
        ));
        assert!(is_nvenc_device_lost_error(
            "map failed with nv_enc_err_invalid_device"
        ));
        assert!(matches!(
            classify_dxgi_capture_error(
                "initialize failed with NV_ENC_ERR_DEVICE_NOT_EXIST".to_owned()
            ),
            DxgiCaptureFailure::DeviceLost(_)
        ));
        assert!(!is_nvenc_device_lost_error(
            "submit failed with NV_ENC_ERR_ENCODER_BUSY"
        ));
    }

    #[test]
    fn busy_retry_metrics_separate_api_calls_from_waits() {
        let clock = Cell::new(Duration::ZERO);
        let call = Cell::new(0_usize);
        let statuses = [
            NVENCSTATUS::NV_ENC_ERR_ENCODER_BUSY,
            NVENCSTATUS::NV_ENC_ERR_ENCODER_BUSY,
            NVENCSTATUS::NV_ENC_SUCCESS,
        ];

        let attempt = submit_nvenc_picture_with_retry(
            || {
                let index = call.get();
                call.set(index + 1);
                clock.set(clock.get() + Duration::from_micros(200));
                statuses[index]
            },
            |delay| clock.set(clock.get() + delay),
            || clock.get(),
            Duration::from_millis(10),
        );

        assert_eq!(attempt.status, NVENCSTATUS::NV_ENC_SUCCESS);
        assert_eq!(attempt.api_us, 600);
        assert_eq!(attempt.busy_wait_us, 2_000);
        assert_eq!(attempt.busy_retries, 2);
    }

    #[test]
    fn slow_success_is_not_misreported_as_nvenc_busy_wait() {
        let clock = Cell::new(Duration::ZERO);
        let attempt = submit_nvenc_picture_with_retry(
            || {
                clock.set(clock.get() + Duration::from_millis(30));
                NVENCSTATUS::NV_ENC_SUCCESS
            },
            |_| unreachable!("a successful submit must not sleep"),
            || clock.get(),
            NVENC_BUSY_RETRY_TIMEOUT,
        );

        assert_eq!(attempt.status, NVENCSTATUS::NV_ENC_SUCCESS);
        assert_eq!(attempt.api_us, 30_000);
        assert_eq!(attempt.busy_wait_us, 0);
        assert_eq!(attempt.busy_retries, 0);
    }

    #[test]
    fn busy_retry_stops_at_its_independent_timeout() {
        let clock = Cell::new(Duration::ZERO);
        let calls = Cell::new(0_u64);
        let attempt = submit_nvenc_picture_with_retry(
            || {
                calls.set(calls.get() + 1);
                NVENCSTATUS::NV_ENC_ERR_ENCODER_BUSY
            },
            |delay| clock.set(clock.get() + delay),
            || clock.get(),
            Duration::from_millis(2),
        );

        assert_eq!(attempt.status, NVENCSTATUS::NV_ENC_ERR_ENCODER_BUSY);
        assert_eq!(attempt.api_us, 0);
        assert_eq!(attempt.busy_wait_us, 2_000);
        assert_eq!(attempt.busy_retries, 2);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn busy_retry_uses_the_full_independent_window() {
        assert_eq!(
            nvenc_busy_retry_delay(Duration::from_millis(16), NVENC_BUSY_RETRY_TIMEOUT),
            Some(NVENC_BUSY_RETRY_DELAY)
        );
        assert_eq!(
            nvenc_busy_retry_delay(Duration::from_millis(49), NVENC_BUSY_RETRY_TIMEOUT),
            Some(Duration::from_millis(1))
        );
        assert_eq!(
            nvenc_busy_retry_delay(NVENC_BUSY_RETRY_TIMEOUT, NVENC_BUSY_RETRY_TIMEOUT),
            None
        );
    }

    #[test]
    fn rejected_submission_restores_a_consumed_idr_request() {
        let force_keyframe = AtomicBool::new(false);
        restore_force_keyframe_request(&force_keyframe, true);
        assert!(force_keyframe.load(Ordering::Acquire));

        force_keyframe.store(false, Ordering::Release);
        restore_force_keyframe_request(&force_keyframe, false);
        assert!(!force_keyframe.load(Ordering::Acquire));
    }

    #[test]
    fn submitted_frame_ids_continue_across_worker_rebuilds() {
        let next_frame_id = Arc::new(AtomicU64::new(41));
        let first_worker = next_frame_id.clone();
        assert_eq!(allocate_submitted_frame_id(&first_worker), 41);
        drop(first_worker);

        let rebuilt_worker = next_frame_id.clone();
        assert_eq!(allocate_submitted_frame_id(&rebuilt_worker), 42);
        assert_eq!(next_frame_id.load(Ordering::Relaxed), 43);
    }

    #[test]
    fn busy_episode_uses_actual_producer_times_across_a_watchdog_pause() {
        let started = Instant::now();
        let mut activity = NvencActivityState::default();

        // The watchdog need not sample between these producer events. Their
        // real timestamps still form one uninterrupted five-second episode.
        for step in 0..=20 {
            activity.record_busy(started + Duration::from_millis(step * 250));
        }

        assert_eq!(activity.busy_episode_started, Some(started));
        assert_eq!(
            activity.last_busy_activity,
            Some(started + NVENC_SUSTAINED_BUSY_TIMEOUT)
        );
        assert!(activity.is_sustained_busy(started + NVENC_SUSTAINED_BUSY_TIMEOUT));
    }

    #[test]
    fn busy_episode_resets_after_activity_grace_expires() {
        let started = Instant::now();
        let continued = started + NVENC_BUSY_ACTIVITY_GRACE;
        let restarted = continued + NVENC_BUSY_ACTIVITY_GRACE + Duration::from_millis(1);
        let mut activity = NvencActivityState::default();

        activity.record_busy(started);
        activity.record_busy(continued);
        assert_eq!(activity.busy_episode_started, Some(started));

        activity.record_busy(restarted);
        assert_eq!(activity.busy_episode_started, Some(restarted));
        assert_eq!(activity.last_busy_activity, Some(restarted));
        assert!(!activity.is_sustained_busy(restarted));
    }

    #[test]
    fn stale_busy_activity_and_completion_end_the_overload_episode() {
        let started = Instant::now();
        let last_busy = started + NVENC_SUSTAINED_BUSY_TIMEOUT;
        let mut activity = NvencActivityState::default();
        for step in 0..=20 {
            activity.record_busy(started + Duration::from_millis(step * 250));
        }

        assert!(!activity
            .is_sustained_busy(last_busy + NVENC_BUSY_ACTIVITY_GRACE + Duration::from_millis(1)));

        let completed = last_busy + Duration::from_millis(10);
        activity.record_completion(completed);
        assert_eq!(activity.last_activity, Some(completed));
        assert_eq!(activity.busy_episode_started, None);
        assert_eq!(activity.last_busy_activity, None);
        assert!(!activity.is_sustained_busy(completed));
    }

    #[test]
    fn successful_submission_restarts_recovery_and_ends_busy_episode() {
        let started = Instant::now();
        let accepted = started + Duration::from_secs(1);
        let mut activity = NvencActivityState::default();

        activity.record_busy(started);
        activity.record_submission(accepted);

        assert_eq!(activity.last_activity, Some(accepted));
        assert_eq!(activity.busy_episode_started, None);
        assert_eq!(activity.last_busy_activity, None);
    }

    #[test]
    fn detached_worker_activity_cannot_mutate_a_rebuilt_session() {
        let retired_session = Arc::new(Mutex::new(NvencActivityState::default()));
        let rebuilt_session = Arc::new(Mutex::new(NvencActivityState::default()));
        let stale_worker_state = retired_session.clone();
        let now = Instant::now();

        stale_worker_state.lock().unwrap().record_busy(now);

        assert_eq!(rebuilt_session.lock().unwrap().last_activity, None);
        assert_eq!(retired_session.lock().unwrap().last_activity, Some(now));
    }

    #[test]
    fn watchdog_separates_sustained_busy_from_a_true_stall() {
        assert_eq!(
            nvenc_recovery_decision(Duration::ZERO, Duration::ZERO, false, false),
            None,
        );
        assert_eq!(
            nvenc_recovery_decision(Duration::ZERO, Duration::ZERO, true, true),
            Some(NvencRecoveryDecision::Overloaded),
        );
        assert_eq!(
            nvenc_recovery_decision(
                ACTIVE_ENCODE_STALL_TIMEOUT,
                ACTIVE_ENCODE_STALL_TIMEOUT,
                true,
                true,
            ),
            Some(NvencRecoveryDecision::Overloaded),
        );
        assert_eq!(
            nvenc_recovery_decision(
                ACTIVE_ENCODE_STALL_TIMEOUT,
                ACTIVE_ENCODE_STALL_TIMEOUT,
                false,
                false,
            ),
            Some(NvencRecoveryDecision::Stalled),
        );
        assert_eq!(
            nvenc_recovery_decision(
                ACTIVE_ENCODE_STALL_TIMEOUT,
                ACTIVE_ENCODE_STALL_TIMEOUT,
                false,
                true,
            ),
            Some(NvencRecoveryDecision::Overloaded),
        );
        assert_eq!(
            nvenc_recovery_decision(Duration::ZERO, NVENC_SUSTAINED_BUSY_TIMEOUT, false, true,),
            Some(NvencRecoveryDecision::Overloaded),
        );
    }

    #[test]
    fn overload_retry_backoff_is_bounded_but_never_exhausted() {
        assert_eq!(nvenc_overload_retry_delay(1), Duration::from_millis(250));
        assert_eq!(nvenc_overload_retry_delay(2), Duration::from_millis(500));
        assert_eq!(nvenc_overload_retry_delay(3), Duration::from_secs(1));
        assert_eq!(nvenc_overload_retry_delay(4), Duration::from_secs(2));
        assert_eq!(nvenc_overload_retry_delay(100), Duration::from_secs(2));
        assert_eq!(nvenc_overload_retry_delay(u32::MAX), Duration::from_secs(2));
    }
}
