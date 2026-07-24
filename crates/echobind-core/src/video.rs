use crate::protocol::{
    VideoFragment, MAX_VIDEO_FRAGMENTS, MAX_VIDEO_FRAGMENT_PAYLOAD, MAX_VIDEO_FRAME_SIZE,
};
use std::{
    collections::HashMap,
    fmt,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoFrame {
    pub frame_id: u64,
    pub timestamp_us: u64,
    pub is_keyframe: bool,
    pub payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoFragmentationError {
    EmptyFrame,
    FrameTooLarge,
}

impl fmt::Display for VideoFragmentationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VideoFragmentationError::EmptyFrame => write!(f, "video frame is empty"),
            VideoFragmentationError::FrameTooLarge => {
                write!(f, "video frame exceeds the protocol limit")
            }
        }
    }
}

impl std::error::Error for VideoFragmentationError {}

pub fn fragment_video_frame(
    frame_id: u64,
    timestamp_us: u64,
    is_keyframe: bool,
    payload: &[u8],
) -> Result<Vec<VideoFragment<'_>>, VideoFragmentationError> {
    if payload.is_empty() {
        return Err(VideoFragmentationError::EmptyFrame);
    }
    if payload.len() > MAX_VIDEO_FRAME_SIZE {
        return Err(VideoFragmentationError::FrameTooLarge);
    }

    let total = payload.len().div_ceil(MAX_VIDEO_FRAGMENT_PAYLOAD);
    if total > MAX_VIDEO_FRAGMENTS || total > u16::MAX as usize {
        return Err(VideoFragmentationError::FrameTooLarge);
    }

    Ok(payload
        .chunks(MAX_VIDEO_FRAGMENT_PAYLOAD)
        .enumerate()
        .map(|(index, chunk)| VideoFragment {
            frame_id,
            timestamp_us,
            index: index as u16,
            total: total as u16,
            is_keyframe,
            payload: chunk,
        })
        .collect())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoReassemblyError {
    InvalidFragment,
    FrameMetadataChanged,
    FrameTooLarge,
}

impl fmt::Display for VideoReassemblyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VideoReassemblyError::InvalidFragment => write!(f, "invalid video fragment"),
            VideoReassemblyError::FrameMetadataChanged => {
                write!(f, "video frame metadata changed during reassembly")
            }
            VideoReassemblyError::FrameTooLarge => {
                write!(f, "reassembled video frame exceeds the protocol limit")
            }
        }
    }
}

impl std::error::Error for VideoReassemblyError {}

pub struct VideoReassembler {
    frames: HashMap<u64, PendingVideoFrame>,
    max_pending_frames: usize,
    max_age: Duration,
}

impl VideoReassembler {
    pub fn new(max_pending_frames: usize, max_age: Duration) -> Self {
        assert!(
            max_pending_frames > 0,
            "at least one pending frame is required"
        );
        Self {
            frames: HashMap::new(),
            max_pending_frames,
            max_age,
        }
    }

    pub fn push(
        &mut self,
        fragment: VideoFragment<'_>,
    ) -> Result<Option<VideoFrame>, VideoReassemblyError> {
        self.drop_expired();
        self.validate_fragment(fragment)?;

        if !self.frames.contains_key(&fragment.frame_id)
            && self.frames.len() >= self.max_pending_frames
        {
            self.drop_oldest();
        }

        let frame = self
            .frames
            .entry(fragment.frame_id)
            .or_insert_with(|| PendingVideoFrame::new(fragment));

        if !frame.matches(fragment) {
            self.frames.remove(&fragment.frame_id);
            return Err(VideoReassemblyError::FrameMetadataChanged);
        }

        frame.push(fragment);
        if !frame.is_complete() {
            return Ok(None);
        }

        let frame = self.frames.remove(&fragment.frame_id).unwrap();
        frame.finish().map(Some)
    }

    pub fn pending_frames(&self) -> usize {
        self.frames.len()
    }

    fn validate_fragment(&self, fragment: VideoFragment<'_>) -> Result<(), VideoReassemblyError> {
        if fragment.total == 0
            || fragment.index >= fragment.total
            || fragment.total as usize > MAX_VIDEO_FRAGMENTS
            || fragment.payload.is_empty()
            || fragment.payload.len() > MAX_VIDEO_FRAGMENT_PAYLOAD
        {
            return Err(VideoReassemblyError::InvalidFragment);
        }
        Ok(())
    }

    fn drop_expired(&mut self) {
        let max_age = self.max_age;
        self.frames
            .retain(|_, frame| frame.last_updated.elapsed() <= max_age);
    }

    fn drop_oldest(&mut self) {
        let oldest = self
            .frames
            .iter()
            .min_by_key(|(_, frame)| frame.last_updated)
            .map(|(frame_id, _)| *frame_id);
        if let Some(frame_id) = oldest {
            self.frames.remove(&frame_id);
        }
    }
}

struct PendingVideoFrame {
    frame_id: u64,
    timestamp_us: u64,
    is_keyframe: bool,
    total: u16,
    chunks: Vec<Option<Vec<u8>>>,
    received: usize,
    encoded_size: usize,
    last_updated: Instant,
}

impl PendingVideoFrame {
    fn new(fragment: VideoFragment<'_>) -> Self {
        Self {
            frame_id: fragment.frame_id,
            timestamp_us: fragment.timestamp_us,
            is_keyframe: fragment.is_keyframe,
            total: fragment.total,
            chunks: vec![None; fragment.total as usize],
            received: 0,
            encoded_size: 0,
            last_updated: Instant::now(),
        }
    }

    fn matches(&self, fragment: VideoFragment<'_>) -> bool {
        self.timestamp_us == fragment.timestamp_us
            && self.is_keyframe == fragment.is_keyframe
            && self.total == fragment.total
    }

    fn push(&mut self, fragment: VideoFragment<'_>) {
        self.last_updated = Instant::now();
        let slot = &mut self.chunks[fragment.index as usize];
        if slot.is_none() {
            self.encoded_size += fragment.payload.len();
            *slot = Some(fragment.payload.to_vec());
            self.received += 1;
        }
    }

    fn is_complete(&self) -> bool {
        self.received == self.total as usize
    }

    fn finish(self) -> Result<VideoFrame, VideoReassemblyError> {
        if self.encoded_size > MAX_VIDEO_FRAME_SIZE {
            return Err(VideoReassemblyError::FrameTooLarge);
        }

        let mut payload = Vec::with_capacity(self.encoded_size);
        for chunk in self.chunks.into_iter().flatten() {
            payload.extend_from_slice(&chunk);
        }

        Ok(VideoFrame {
            frame_id: self.frame_id,
            timestamp_us: self.timestamp_us,
            is_keyframe: self.is_keyframe,
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragments_and_reassembles_out_of_order() {
        let payload = vec![7; MAX_VIDEO_FRAGMENT_PAYLOAD * 2 + 17];
        let fragments = fragment_video_frame(42, 123_456, true, &payload).unwrap();
        assert_eq!(fragments.len(), 3);

        let mut reassembler = VideoReassembler::new(3, Duration::from_secs(1));
        assert_eq!(reassembler.push(fragments[2]).unwrap(), None);
        assert_eq!(reassembler.push(fragments[0]).unwrap(), None);
        let frame = reassembler.push(fragments[1]).unwrap().unwrap();

        assert_eq!(frame.frame_id, 42);
        assert_eq!(frame.timestamp_us, 123_456);
        assert!(frame.is_keyframe);
        assert_eq!(frame.payload, payload);
    }

    #[test]
    fn duplicate_fragment_is_ignored() {
        let payload = vec![1; MAX_VIDEO_FRAGMENT_PAYLOAD + 1];
        let fragments = fragment_video_frame(8, 99, false, &payload).unwrap();
        let mut reassembler = VideoReassembler::new(2, Duration::from_secs(1));

        assert_eq!(reassembler.push(fragments[0]).unwrap(), None);
        assert_eq!(reassembler.push(fragments[0]).unwrap(), None);
        assert_eq!(
            reassembler.push(fragments[1]).unwrap().unwrap().payload,
            payload
        );
    }

    #[test]
    fn pending_frames_are_bounded() {
        let payload = vec![0; MAX_VIDEO_FRAGMENT_PAYLOAD + 1];
        let first = fragment_video_frame(1, 0, false, &payload).unwrap();
        let second = fragment_video_frame(2, 1, false, &payload).unwrap();
        let mut reassembler = VideoReassembler::new(1, Duration::from_secs(1));

        reassembler.push(first[0]).unwrap();
        reassembler.push(second[0]).unwrap();
        assert_eq!(reassembler.pending_frames(), 1);
    }
}
