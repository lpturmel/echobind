use super::{
    ClientMetrics, DecodeQueue, DisplayFrame, DisplayFrameData, FrameNotifier, LatestFrame,
    SessionEvent, VIDEO_STALE_AGE,
};
use apple_cf::{
    cf::{CFDictionary, CFNumber, CFType},
    cm::{CMBlockBuffer, CMFormatDescription, CMSampleBuffer},
    raw,
};
use std::{
    collections::BTreeMap,
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};
use videotoolbox::{
    ffi::{kVTDecodeFrame_1xRealTimePlayback, kVTDecodeFrame_EnableAsynchronousDecompression},
    session::Codec,
    DecompressionSession,
};

const NAL_LENGTH_BYTES: usize = 4;

pub(super) fn run_decoder(
    running: Arc<AtomicBool>,
    decode_queue: DecodeQueue,
    latest_frame: LatestFrame,
    metrics: Arc<ClientMetrics>,
    needs_keyframe: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    frame_notifier: FrameNotifier,
) -> Result<(), String> {
    if !DecompressionSession::is_hardware_decode_supported(Codec::H264) {
        return Err("Apple hardware H.264 decoding is not supported".to_owned());
    }

    let _ = events.send(SessionEvent::VideoBackend(
        "VideoToolbox hardware decoder · asynchronous".to_owned(),
    ));
    let mut decoder = None::<DecompressionSession>;
    let mut format_description = None::<CMFormatDescription>;
    let mut parameter_sets = None::<(Vec<u8>, Vec<u8>)>;
    let decode_started = Arc::new(Mutex::new(BTreeMap::<i64, Instant>::new()));

    while running.load(Ordering::Relaxed) {
        let Some(received) = decode_queue.pop_timeout(Duration::from_millis(20)) else {
            continue;
        };
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
        let frame = received.frame;
        let nals = match annex_b_nals(&frame.payload) {
            Ok(nals) => nals,
            Err(error) => {
                mark_corrupt_frame(&metrics, &needs_keyframe, &error);
                continue;
            }
        };
        if let Some((sps, pps)) = h264_parameter_sets(&nals) {
            let changed = parameter_sets
                .as_ref()
                .is_none_or(|current| current.0 != sps || current.1 != pps);
            if changed {
                if !frame.is_keyframe {
                    needs_keyframe.store(true, Ordering::Release);
                    continue;
                }
                let description = match create_format_description(&sps, &pps) {
                    Ok(description) => description,
                    Err(error) => {
                        mark_corrupt_frame(&metrics, &needs_keyframe, &error);
                        continue;
                    }
                };
                let session = match create_decoder(
                    &description,
                    latest_frame.clone(),
                    metrics.clone(),
                    needs_keyframe.clone(),
                    decode_started.clone(),
                    frame_notifier.clone(),
                ) {
                    Ok(session) => session,
                    Err(error) => {
                        mark_corrupt_frame(&metrics, &needs_keyframe, &error);
                        continue;
                    }
                };
                decoder = Some(session);
                format_description = Some(description);
                parameter_sets = Some((sps, pps));
            }
        }

        let (Some(session), Some(description)) = (decoder.as_ref(), format_description.as_ref())
        else {
            needs_keyframe.store(true, Ordering::Release);
            continue;
        };
        let avcc = match annex_b_to_avcc(&nals) {
            Ok(avcc) => avcc,
            Err(error) => {
                mark_corrupt_frame(&metrics, &needs_keyframe, &error);
                continue;
            }
        };
        let sample = match create_sample_buffer(&avcc, frame.timestamp_us, description) {
            Ok(sample) => sample,
            Err(error) => {
                mark_corrupt_frame(&metrics, &needs_keyframe, &error);
                continue;
            }
        };
        let timestamp_key = frame.timestamp_us.min(i64::MAX as u64) as i64;
        {
            let mut pending = decode_started.lock().unwrap();
            pending.insert(timestamp_key, Instant::now());
            while pending.len() > 16 {
                pending.pop_first();
            }
        }
        if let Err(error) = session.decode_with_options(
            &sample,
            kVTDecodeFrame_EnableAsynchronousDecompression | kVTDecodeFrame_1xRealTimePlayback,
            None,
        ) {
            decode_started.lock().unwrap().remove(&timestamp_key);
            metrics.dropped_frames.fetch_add(1, Ordering::Relaxed);
            needs_keyframe.store(true, Ordering::Release);
            decoder = None;
            format_description = None;
            parameter_sets = None;
            tracing::warn!("VideoToolbox rejected an H.264 frame: {error}");
        }
    }

    if let Some(session) = decoder {
        let _ = session.wait_for_async_frames();
    }
    Ok(())
}

fn mark_corrupt_frame(metrics: &ClientMetrics, needs_keyframe: &AtomicBool, error: &str) {
    metrics.dropped_frames.fetch_add(1, Ordering::Relaxed);
    needs_keyframe.store(true, Ordering::Release);
    tracing::warn!("Discarding invalid H.264 frame: {error}");
}

fn create_decoder(
    description: &CMFormatDescription,
    latest_frame: LatestFrame,
    metrics: Arc<ClientMetrics>,
    needs_keyframe: Arc<AtomicBool>,
    decode_started: Arc<Mutex<BTreeMap<i64, Instant>>>,
    frame_notifier: FrameNotifier,
) -> Result<DecompressionSession, String> {
    let pixel_format_key = unsafe {
        CFType::from_raw_retained(raw::kCVPixelBufferPixelFormatTypeKey.cast_mut().cast())
            .ok_or_else(|| "CoreVideo returned no pixel-format key".to_owned())?
    };
    let io_surface_key = unsafe {
        CFType::from_raw_retained(raw::kCVPixelBufferIOSurfacePropertiesKey.cast_mut().cast())
            .ok_or_else(|| "CoreVideo returned no IOSurface key".to_owned())?
    };
    let metal_compatibility_key = unsafe {
        CFType::from_raw_retained(raw::kCVPixelBufferMetalCompatibilityKey.cast_mut().cast())
            .ok_or_else(|| "CoreVideo returned no Metal-compatibility key".to_owned())?
    };
    let boolean_true = unsafe {
        CFType::from_raw_retained(raw::kCFBooleanTrue.cast_mut().cast())
            .ok_or_else(|| "CoreFoundation returned no true value".to_owned())?
    };
    let pixel_format = CFNumber::from_u64(u64::from(
        raw::kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    ));
    let io_surface_properties = CFDictionary::from_pairs(&[]);
    let attributes = CFDictionary::from_pairs(&[
        (&pixel_format_key, &pixel_format),
        (&io_surface_key, &io_surface_properties),
        (&metal_compatibility_key, &boolean_true),
    ]);

    let decoder = DecompressionSession::new_with_image_buffer_attributes(
        description,
        Some(&attributes),
        move |decoded| {
            let decode_elapsed = decode_started
                .lock()
                .unwrap()
                .remove(&decoded.presentation_time.0)
                .map(|started| started.elapsed());
            if decoded.status != 0 {
                tracing::warn!(
                    status = decoded.status,
                    flags = decoded.info_flags,
                    "VideoToolbox decode callback failed"
                );
                metrics.dropped_frames.fetch_add(1, Ordering::Relaxed);
                needs_keyframe.store(true, Ordering::Release);
                return;
            }
            let Some(pixel_buffer) = decoded.image_buffer else {
                tracing::warn!(
                    flags = decoded.info_flags,
                    "VideoToolbox returned no pixel buffer"
                );
                metrics.dropped_frames.fetch_add(1, Ordering::Relaxed);
                return;
            };
            let width = pixel_buffer.width();
            let height = pixel_buffer.height();
            if pixel_buffer.pixel_format() != raw::kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
                || pixel_buffer.plane_count() != 2
                || pixel_buffer.io_surface().is_none()
            {
                tracing::warn!(
                    format = pixel_buffer.pixel_format(),
                    planes = pixel_buffer.plane_count(),
                    "VideoToolbox returned an incompatible pixel buffer"
                );
                metrics.dropped_frames.fetch_add(1, Ordering::Relaxed);
                needs_keyframe.store(true, Ordering::Release);
                return;
            }
            *latest_frame.lock().unwrap() = Some(DisplayFrame {
                width,
                height,
                data: DisplayFrameData::Nv12(pixel_buffer),
                published_at: std::time::Instant::now(),
            });
            frame_notifier();
            metrics.decoded_frames.fetch_add(1, Ordering::Relaxed);
            if let Some(elapsed) = decode_elapsed {
                metrics.decode_us.fetch_add(
                    elapsed.as_micros().min(u128::from(u64::MAX)) as u64,
                    Ordering::Relaxed,
                );
                metrics.decode_samples.fetch_add(1, Ordering::Relaxed);
            }
        },
    )
    .map_err(|error| format!("Unable to create VideoToolbox decoder: {error}"))?;
    decoder
        .set_real_time(true)
        .map_err(|error| format!("Unable to enable real-time VideoToolbox decode: {error}"))?;
    Ok(decoder)
}

fn create_format_description(sps: &[u8], pps: &[u8]) -> Result<CMFormatDescription, String> {
    let pointers = [sps.as_ptr(), pps.as_ptr()];
    let sizes = [sps.len(), pps.len()];
    let mut description: raw::CMFormatDescriptionRef = ptr::null();
    let status = unsafe {
        raw::CMVideoFormatDescriptionCreateFromH264ParameterSets(
            raw::kCFAllocatorDefault,
            pointers.len(),
            pointers.as_ptr(),
            sizes.as_ptr(),
            NAL_LENGTH_BYTES as i32,
            &mut description,
        )
    };
    if status != 0 || description.is_null() {
        return Err(format!(
            "Unable to create H.264 format description (status {status})"
        ));
    }
    CMFormatDescription::from_raw(description.cast_mut().cast())
        .ok_or_else(|| "CoreMedia returned no H.264 format description".to_owned())
}

fn create_sample_buffer(
    avcc: &[u8],
    timestamp_us: u64,
    description: &CMFormatDescription,
) -> Result<CMSampleBuffer, String> {
    let block = CMBlockBuffer::create(avcc)
        .ok_or_else(|| "Unable to allocate compressed H.264 buffer".to_owned())?;
    let timestamp_value = timestamp_us.min(i64::MAX as u64) as i64;
    let timing = raw::CMSampleTimingInfo {
        duration: unsafe { raw::kCMTimeInvalid },
        presentationTimeStamp: unsafe { raw::CMTimeMake(timestamp_value, 1_000_000) },
        decodeTimeStamp: unsafe { raw::kCMTimeInvalid },
    };
    let size = avcc.len();
    let mut sample: raw::CMSampleBufferRef = ptr::null_mut();
    let status = unsafe {
        raw::CMSampleBufferCreateReady(
            raw::kCFAllocatorDefault,
            block.as_ptr().cast(),
            description.as_ptr().cast(),
            1,
            1,
            &timing,
            1,
            &size,
            &mut sample,
        )
    };
    if status != 0 || sample.is_null() {
        return Err(format!(
            "Unable to create compressed H.264 sample (status {status})"
        ));
    }
    CMSampleBuffer::from_raw(sample.cast())
        .ok_or_else(|| "CoreMedia returned no H.264 sample buffer".to_owned())
}

fn h264_parameter_sets(nals: &[&[u8]]) -> Option<(Vec<u8>, Vec<u8>)> {
    let sps = nals
        .iter()
        .find(|nal| nal.first().is_some_and(|header| header & 0x1f == 7))?;
    let pps = nals
        .iter()
        .find(|nal| nal.first().is_some_and(|header| header & 0x1f == 8))?;
    Some((sps.to_vec(), pps.to_vec()))
}

fn annex_b_to_avcc(nals: &[&[u8]]) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    for nal in nals {
        let nal_type = nal[0] & 0x1f;
        if matches!(nal_type, 7..=9) {
            continue;
        }
        let length = u32::try_from(nal.len())
            .map_err(|_| "H.264 NAL unit exceeds AVCC limits".to_owned())?;
        output.extend_from_slice(&length.to_be_bytes());
        output.extend_from_slice(nal);
    }
    if output.is_empty() {
        return Err("H.264 frame contains no decodable NAL units".to_owned());
    }
    Ok(output)
}

fn annex_b_nals(data: &[u8]) -> Result<Vec<&[u8]>, String> {
    let mut nals = Vec::new();
    let Some((mut position, mut prefix_len)) = find_start_code(data, 0) else {
        return Err("H.264 frame is not Annex B".to_owned());
    };
    loop {
        let nal_start = position + prefix_len;
        let next = find_start_code(data, nal_start);
        let nal_end = next.map_or(data.len(), |(next_position, _)| next_position);
        if nal_end > nal_start {
            nals.push(&data[nal_start..nal_end]);
        }
        let Some((next_position, next_prefix_len)) = next else {
            break;
        };
        position = next_position;
        prefix_len = next_prefix_len;
    }
    if nals.is_empty() {
        Err("H.264 frame contains no NAL units".to_owned())
    } else {
        Ok(nals)
    }
}

fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut position = from;
    while position + 3 <= data.len() {
        if data[position..].starts_with(&[0, 0, 1]) {
            return Some((position, 3));
        }
        if position + 4 <= data.len() && data[position..].starts_with(&[0, 0, 0, 1]) {
            return Some((position, 4));
        }
        position += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_annex_b_and_builds_avcc() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5,
        ];
        let nals = annex_b_nals(&data).unwrap();
        assert_eq!(nals.len(), 3);
        assert_eq!(
            h264_parameter_sets(&nals).unwrap(),
            (vec![0x67, 1, 2], vec![0x68, 3])
        );
        assert_eq!(
            annex_b_to_avcc(&nals).unwrap(),
            vec![0, 0, 0, 3, 0x65, 4, 5]
        );
    }
}
