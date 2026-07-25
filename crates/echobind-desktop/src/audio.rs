use super::SessionEvent;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    BufferSize as CpalBufferSize, Device, FromSample, Sample, SampleFormat, SizedSample, Stream,
    StreamConfig,
};
use echobind_core::{
    protocol::{AudioFrame, Packet, MAX_AUDIO_FRAME_PAYLOAD, MAX_DATAGRAM_SIZE},
    AudioConfig, BufferSize,
};
use std::{
    collections::VecDeque,
    net::{SocketAddr, UdpSocket},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender, TrySendError},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const OPUS_FRAME_MS: usize = 10;
const AUDIO_CAPTURE_QUEUE: usize = 4;
const AUDIO_PACKET_QUEUE: usize = 8;
const PLAYBACK_BUFFER_MS: usize = 100;
const PLAYBACK_START_MS: usize = 30;
const OPUS_SAMPLE_RATES: [u32; 5] = [48_000, 24_000, 16_000, 12_000, 8_000];

#[derive(Clone)]
pub(super) struct AudioCaptureSettings {
    device_name: String,
    stream_config: StreamConfig,
    sample_format: SampleFormat,
    encoded_channels: u16,
    buffer_size: BufferSize,
}

struct EncodedAudioFrame {
    sequence: u32,
    payload: Vec<u8>,
}

struct PlaybackBuffer {
    samples: VecDeque<f32>,
    started: bool,
    phase: f64,
}

pub(super) struct AudioPlayback {
    sender: SyncSender<EncodedAudioFrame>,
    running: Arc<AtomicBool>,
    _handle: JoinHandle<()>,
}

impl AudioCaptureSettings {
    pub(super) fn session_config(&self) -> AudioConfig {
        AudioConfig {
            sample_format: "opus_f32".to_owned(),
            sample_rate: self.stream_config.sample_rate.0,
            channels: self.encoded_channels,
            buffer_size: self.buffer_size,
        }
    }
}

pub(super) fn output_device_names() -> Result<Vec<String>, String> {
    let host = cpal::default_host();
    let devices = host
        .output_devices()
        .map_err(|error| format!("Unable to enumerate audio outputs: {error}"))?;
    let mut names: Vec<_> = devices.filter_map(|device| device.name().ok()).collect();
    names.sort_unstable();
    names.dedup();
    Ok(names)
}

pub(super) fn discover_system_audio_capture() -> Result<AudioCaptureSettings, String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "No default audio output device is available".to_owned())?;
    let device_name = device
        .name()
        .unwrap_or_else(|_| "Default output".to_owned());
    let default = device
        .default_output_config()
        .map_err(|error| format!("Unable to read the default output format: {error}"))?;
    let selected = if OPUS_SAMPLE_RATES.contains(&default.sample_rate().0) {
        default
    } else {
        select_opus_capture_config(&device, default.channels(), default.sample_format())?
    };
    let buffer_size = cpal_buffer_range(selected.buffer_size());
    let encoded_channels = if selected.channels() == 1 { 1 } else { 2 };
    let mut stream_config: StreamConfig = selected.clone().into();
    stream_config.buffer_size = CpalBufferSize::Default;

    Ok(AudioCaptureSettings {
        device_name,
        stream_config,
        sample_format: selected.sample_format(),
        encoded_channels,
        buffer_size,
    })
}

fn select_opus_capture_config(
    device: &Device,
    channels: u16,
    preferred_format: SampleFormat,
) -> Result<cpal::SupportedStreamConfig, String> {
    let ranges: Vec<_> = device
        .supported_output_configs()
        .map_err(|error| format!("Unable to query output formats: {error}"))?
        .collect();
    for sample_rate in OPUS_SAMPLE_RATES {
        if let Some(range) = ranges.iter().find(|range| {
            range.channels() == channels
                && range.sample_format() == preferred_format
                && range.min_sample_rate().0 <= sample_rate
                && sample_rate <= range.max_sample_rate().0
        }) {
            return Ok((*range).with_sample_rate(cpal::SampleRate(sample_rate)));
        }
    }
    Err("The default output device has no Opus-compatible loopback format".to_owned())
}

fn cpal_buffer_range(buffer_size: &cpal::SupportedBufferSize) -> BufferSize {
    match buffer_size {
        cpal::SupportedBufferSize::Range { min, max } => BufferSize {
            min: *min,
            max: *max,
        },
        cpal::SupportedBufferSize::Unknown => BufferSize { min: 0, max: 0 },
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_audio_capture(
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    events: mpsc::Sender<SessionEvent>,
    settings: AudioCaptureSettings,
) -> JoinHandle<()> {
    thread::spawn(move || {
        if let Err(error) =
            run_audio_capture(socket, running, active_peer, events.clone(), settings)
        {
            let _ = events.send(SessionEvent::AudioBackend(format!(
                "Audio unavailable: {error}"
            )));
        }
    })
}

fn run_audio_capture(
    socket: Arc<UdpSocket>,
    running: Arc<AtomicBool>,
    active_peer: Arc<Mutex<Option<SocketAddr>>>,
    events: mpsc::Sender<SessionEvent>,
    settings: AudioCaptureSettings,
) -> Result<(), String> {
    let host = cpal::default_host();
    let device = find_output_device(&host, Some(&settings.device_name))?;
    let (capture_tx, capture_rx) = mpsc::sync_channel::<Vec<f32>>(AUDIO_CAPTURE_QUEUE);
    let (recycle_tx, recycle_rx) = mpsc::sync_channel::<Vec<f32>>(AUDIO_CAPTURE_QUEUE + 1);
    let stream = build_capture_stream(
        &device,
        &settings.stream_config,
        settings.sample_format,
        settings.encoded_channels,
        capture_tx,
        recycle_tx.clone(),
        recycle_rx,
        events.clone(),
    )?;
    stream
        .play()
        .map_err(|error| format!("Unable to start system-audio capture: {error}"))?;

    let channel_count = usize::from(settings.encoded_channels);
    let sample_rate = settings.stream_config.sample_rate.0;
    let frame_samples = sample_rate as usize * OPUS_FRAME_MS / 1000;
    let frame_len = frame_samples * channel_count;
    let opus_channels = opus_channels(settings.encoded_channels);
    let mut encoder = opus::Encoder::new(sample_rate, opus_channels, opus::Application::LowDelay)
        .map_err(|error| format!("Unable to initialize Opus encoder: {error}"))?;
    encoder
        .set_bitrate(opus::Bitrate::Bits(if channel_count == 1 {
            64_000
        } else {
            128_000
        }))
        .map_err(|error| format!("Unable to configure Opus bitrate: {error}"))?;
    encoder
        .set_inband_fec(true)
        .map_err(|error| format!("Unable to configure Opus FEC: {error}"))?;
    encoder
        .set_packet_loss_perc(5)
        .map_err(|error| format!("Unable to configure Opus packet loss: {error}"))?;
    let _ = events.send(SessionEvent::AudioBackend(format!(
        "Opus {} kHz {} · system audio ({})",
        sample_rate / 1000,
        channel_label(settings.encoded_channels),
        settings.device_name
    )));

    let mut pending = Vec::<f32>::with_capacity(frame_len * 3);
    let mut encoded = vec![0_u8; MAX_AUDIO_FRAME_PAYLOAD];
    let mut packet = Vec::with_capacity(MAX_DATAGRAM_SIZE);
    let mut sequence = 0_u32;
    let mut timestamp_us = 0_u64;

    while running.load(Ordering::Relaxed) {
        let mut samples = match capture_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(samples) => samples,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("System-audio capture stopped".to_owned());
            }
        };
        let peer = *active_peer.lock().unwrap();
        let Some(peer) = peer else {
            pending.clear();
            samples.clear();
            let _ = recycle_tx.try_send(samples);
            continue;
        };
        pending.extend_from_slice(&samples);
        samples.clear();
        let _ = recycle_tx.try_send(samples);
        while pending.len() >= frame_len {
            let length = encoder
                .encode_float(&pending[..frame_len], &mut encoded)
                .map_err(|error| format!("Opus encoding failed: {error}"))?;
            Packet::Audio(AudioFrame {
                sequence,
                timestamp_us,
                payload: &encoded[..length],
            })
            .encode(&mut packet);
            if let Err(error) = socket.send_to(&packet, peer) {
                tracing::warn!("Audio send to {peer} failed: {error}");
            }
            sequence = sequence.wrapping_add(1);
            timestamp_us = timestamp_us.wrapping_add((OPUS_FRAME_MS * 1000) as u64);
            pending.drain(..frame_len);
        }
    }
    drop(stream);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_capture_stream(
    device: &Device,
    config: &StreamConfig,
    format: SampleFormat,
    encoded_channels: u16,
    sender: SyncSender<Vec<f32>>,
    recycle_sender: SyncSender<Vec<f32>>,
    recycle_receiver: mpsc::Receiver<Vec<f32>>,
    events: mpsc::Sender<SessionEvent>,
) -> Result<Stream, String> {
    macro_rules! build {
        ($sample:ty) => {
            build_capture_stream_for::<$sample>(
                device,
                config,
                encoded_channels,
                sender,
                recycle_sender,
                recycle_receiver,
                events,
            )
        };
    }
    match format {
        SampleFormat::I8 => build!(i8),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I32 => build!(i32),
        SampleFormat::I64 => build!(i64),
        SampleFormat::U8 => build!(u8),
        SampleFormat::U16 => build!(u16),
        SampleFormat::U32 => build!(u32),
        SampleFormat::U64 => build!(u64),
        SampleFormat::F32 => build!(f32),
        SampleFormat::F64 => build!(f64),
        format => Err(format!("Unsupported system-audio sample format: {format}")),
    }
}

fn build_capture_stream_for<T>(
    device: &Device,
    config: &StreamConfig,
    encoded_channels: u16,
    sender: SyncSender<Vec<f32>>,
    recycle_sender: SyncSender<Vec<f32>>,
    recycle_receiver: mpsc::Receiver<Vec<f32>>,
    events: mpsc::Sender<SessionEvent>,
) -> Result<Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let capture_channels = usize::from(config.channels);
    let encoded_channels = usize::from(encoded_channels);
    device
        .build_input_stream(
            config,
            move |input: &[T], _| {
                let frames = input.len() / capture_channels;
                let mut converted = recycle_receiver
                    .try_recv()
                    .unwrap_or_else(|_| Vec::with_capacity(frames * encoded_channels));
                converted.clear();
                converted.reserve(frames * encoded_channels);
                for frame in input.chunks_exact(capture_channels) {
                    if encoded_channels == 1 {
                        converted.push(f32::from_sample(frame[0]));
                    } else {
                        converted.push(f32::from_sample(frame[0]));
                        converted.push(f32::from_sample(frame[1]));
                    }
                }
                match sender.try_send(converted) {
                    Ok(()) => {}
                    Err(TrySendError::Full(mut converted)) => {
                        converted.clear();
                        let _ = recycle_sender.try_send(converted);
                    }
                    Err(TrySendError::Disconnected(_)) => {}
                }
            },
            move |error| {
                let _ = events.send(SessionEvent::AudioBackend(format!(
                    "System-audio capture error: {error}"
                )));
            },
            None,
        )
        .map_err(|error| format!("Unable to open system-audio loopback: {error}"))
}

pub(super) fn spawn_audio_playback(
    config: AudioConfig,
    output_device: Option<String>,
    events: mpsc::Sender<SessionEvent>,
) -> AudioPlayback {
    let (sender, receiver) = mpsc::sync_channel(AUDIO_PACKET_QUEUE);
    let running = Arc::new(AtomicBool::new(true));
    let thread_running = running.clone();
    let handle = thread::spawn(move || {
        if let Err(error) = run_audio_playback(
            config,
            output_device,
            events.clone(),
            thread_running,
            receiver,
        ) {
            let _ = events.send(SessionEvent::AudioBackend(format!(
                "Audio unavailable: {error}"
            )));
        }
    });
    AudioPlayback {
        sender,
        running,
        _handle: handle,
    }
}

impl AudioPlayback {
    pub(super) fn push(&self, frame: AudioFrame<'_>) {
        let encoded = EncodedAudioFrame {
            sequence: frame.sequence,
            payload: frame.payload.to_vec(),
        };
        match self.sender.try_send(encoded) {
            Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

impl Drop for AudioPlayback {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

fn run_audio_playback(
    config: AudioConfig,
    output_device: Option<String>,
    events: mpsc::Sender<SessionEvent>,
    running: Arc<AtomicBool>,
    receiver: mpsc::Receiver<EncodedAudioFrame>,
) -> Result<(), String> {
    if !matches!(config.channels, 1 | 2) {
        return Err(format!(
            "Unsupported Opus channel count: {}",
            config.channels
        ));
    }
    let host = cpal::default_host();
    let device = find_output_device(&host, output_device.as_deref())?;
    let device_name = device
        .name()
        .unwrap_or_else(|_| "Selected output".to_owned());
    let supported = device
        .default_output_config()
        .map_err(|error| format!("Unable to read output format: {error}"))?;
    let sample_format = supported.sample_format();
    let mut output_config: StreamConfig = supported.into();
    output_config.buffer_size = CpalBufferSize::Default;
    let output_rate = output_config.sample_rate.0;

    let max_samples =
        config.sample_rate as usize * PLAYBACK_BUFFER_MS / 1000 * usize::from(config.channels);
    let start_frames = config.sample_rate as usize * PLAYBACK_START_MS / 1000;
    let buffer = Arc::new(Mutex::new(PlaybackBuffer {
        samples: VecDeque::with_capacity(max_samples),
        started: false,
        phase: 0.0,
    }));
    let stream = build_output_stream(
        &device,
        &output_config,
        sample_format,
        buffer.clone(),
        config.sample_rate,
        config.channels,
        start_frames,
        events.clone(),
    )?;
    stream
        .play()
        .map_err(|error| format!("Unable to start audio output: {error}"))?;
    let _ = events.send(SessionEvent::AudioBackend(format!(
        "Opus {} kHz {} → {} ({} kHz)",
        config.sample_rate / 1000,
        channel_label(config.channels),
        device_name,
        output_rate / 1000
    )));

    let mut decoder = opus::Decoder::new(config.sample_rate, opus_channels(config.channels))
        .map_err(|error| format!("Unable to initialize Opus decoder: {error}"))?;
    let mut decoded = vec![0.0_f32; config.sample_rate as usize / 5 * usize::from(config.channels)];
    let mut expected_sequence = None::<u32>;

    while running.load(Ordering::Relaxed) {
        let frame = match receiver.recv_timeout(Duration::from_millis(20)) {
            Ok(frame) => frame,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if let Some(expected) = expected_sequence {
            let distance = frame.sequence.wrapping_sub(expected);
            if distance > u32::MAX / 2 {
                continue;
            }
        }
        let frames = match decoder.decode_float(&frame.payload, &mut decoded, false) {
            Ok(frames) => frames,
            Err(error) => {
                tracing::warn!("Opus decode failed: {error}");
                continue;
            }
        };
        expected_sequence = Some(frame.sequence.wrapping_add(1));
        let sample_count = frames * usize::from(config.channels);
        let mut playback = buffer.lock().unwrap();
        while playback.samples.len() + sample_count > max_samples {
            for _ in 0..usize::from(config.channels) {
                playback.samples.pop_front();
            }
        }
        playback.samples.extend(&decoded[..sample_count]);
    }
    drop(stream);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_output_stream(
    device: &Device,
    output_config: &StreamConfig,
    sample_format: SampleFormat,
    buffer: Arc<Mutex<PlaybackBuffer>>,
    source_rate: u32,
    source_channels: u16,
    start_frames: usize,
    events: mpsc::Sender<SessionEvent>,
) -> Result<Stream, String> {
    macro_rules! build {
        ($sample:ty) => {
            build_output_stream_for::<$sample>(
                device,
                output_config,
                buffer,
                source_rate,
                source_channels,
                start_frames,
                events,
            )
        };
    }
    match sample_format {
        SampleFormat::I8 => build!(i8),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I32 => build!(i32),
        SampleFormat::I64 => build!(i64),
        SampleFormat::U8 => build!(u8),
        SampleFormat::U16 => build!(u16),
        SampleFormat::U32 => build!(u32),
        SampleFormat::U64 => build!(u64),
        SampleFormat::F32 => build!(f32),
        SampleFormat::F64 => build!(f64),
        format => Err(format!("Unsupported output sample format: {format}")),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_output_stream_for<T>(
    device: &Device,
    output_config: &StreamConfig,
    buffer: Arc<Mutex<PlaybackBuffer>>,
    source_rate: u32,
    source_channels: u16,
    start_frames: usize,
    events: mpsc::Sender<SessionEvent>,
) -> Result<Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let output_rate = output_config.sample_rate.0;
    let output_channels = output_config.channels;
    device
        .build_output_stream(
            output_config,
            move |output: &mut [T], _| {
                write_output(
                    output,
                    output_rate,
                    output_channels,
                    source_rate,
                    source_channels,
                    start_frames,
                    &buffer,
                );
            },
            move |error| {
                let _ = events.send(SessionEvent::AudioBackend(format!(
                    "Audio output error: {error}"
                )));
            },
            None,
        )
        .map_err(|error| format!("Unable to open audio output: {error}"))
}

#[allow(clippy::too_many_arguments)]
fn write_output<T>(
    output: &mut [T],
    output_rate: u32,
    output_channels: u16,
    source_rate: u32,
    source_channels: u16,
    start_frames: usize,
    buffer: &Arc<Mutex<PlaybackBuffer>>,
) where
    T: Sample + FromSample<f32>,
{
    output.fill(T::from_sample(0.0));
    let output_channels = usize::from(output_channels);
    let source_channels = usize::from(source_channels);
    let mut playback = buffer.lock().unwrap();
    if !playback.started {
        if playback.samples.len() / source_channels < start_frames {
            return;
        }
        playback.started = true;
        playback.phase = 0.0;
    }

    let ratio = f64::from(source_rate) / f64::from(output_rate);
    for output_frame in output.chunks_exact_mut(output_channels) {
        if playback.samples.len() < source_channels * 2 {
            playback.started = false;
            playback.phase = 0.0;
            break;
        }
        let fraction = playback.phase as f32;
        for (channel, destination) in output_frame.iter_mut().enumerate() {
            let (first, second) = interpolated_source_pair(
                &playback.samples,
                source_channels,
                output_channels,
                channel,
            );
            *destination = T::from_sample((first + (second - first) * fraction).clamp(-1.0, 1.0));
        }
        playback.phase += ratio;
        let consumed_frames = playback.phase.floor() as usize;
        playback.phase -= consumed_frames as f64;
        for _ in 0..consumed_frames * source_channels {
            playback.samples.pop_front();
        }
    }
}

fn interpolated_source_pair(
    samples: &VecDeque<f32>,
    source_channels: usize,
    output_channels: usize,
    output_channel: usize,
) -> (f32, f32) {
    if source_channels == 1 {
        return (samples[0], samples[source_channels]);
    }
    if output_channels == 1 {
        return (
            (samples[0] + samples[1]) * 0.5,
            (samples[source_channels] + samples[source_channels + 1]) * 0.5,
        );
    }
    if output_channel < 2 {
        return (
            samples[output_channel],
            samples[source_channels + output_channel],
        );
    }
    (0.0, 0.0)
}

fn find_output_device(host: &cpal::Host, name: Option<&str>) -> Result<Device, String> {
    if let Some(name) = name {
        return host
            .output_devices()
            .map_err(|error| format!("Unable to enumerate audio outputs: {error}"))?
            .find(|device| device.name().is_ok_and(|candidate| candidate == name))
            .ok_or_else(|| format!("Audio output device is no longer available: {name}"));
    }
    host.default_output_device()
        .ok_or_else(|| "No default audio output device is available".to_owned())
}

const fn opus_channels(channels: u16) -> opus::Channels {
    if channels == 1 {
        opus::Channels::Mono
    } else {
        opus::Channels::Stereo
    }
}

const fn channel_label(channels: u16) -> &'static str {
    if channels == 1 {
        "mono"
    } else {
        "stereo"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_stereo_to_mono() {
        let samples = VecDeque::from([1.0, -1.0, 0.5, 0.5]);
        assert_eq!(interpolated_source_pair(&samples, 2, 1, 0), (0.0, 0.5));
    }

    #[test]
    fn maps_mono_to_each_output_channel() {
        let samples = VecDeque::from([0.25, 0.75]);
        assert_eq!(interpolated_source_pair(&samples, 1, 2, 1), (0.25, 0.75));
    }

    #[test]
    fn opus_frame_fits_audio_datagram_and_decodes() {
        let frame_samples = 48_000 * OPUS_FRAME_MS / 1000;
        let input = vec![0.0_f32; frame_samples * 2];
        let mut encoded = vec![0_u8; MAX_AUDIO_FRAME_PAYLOAD];
        let mut encoder =
            opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::LowDelay)
                .unwrap();
        encoder.set_bitrate(opus::Bitrate::Bits(128_000)).unwrap();
        let length = encoder.encode_float(&input, &mut encoded).unwrap();
        assert!(length <= MAX_AUDIO_FRAME_PAYLOAD);

        let mut decoder = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
        let mut decoded = vec![0.0_f32; frame_samples * 2];
        assert_eq!(
            decoder
                .decode_float(&encoded[..length], &mut decoded, false)
                .unwrap(),
            frame_samples
        );
    }
}
