use crate::protocol::{
    VideoFragment, MAX_DATAGRAM_SIZE, MAX_VIDEO_FRAGMENTS, MAX_VIDEO_FRAGMENT_PAYLOAD,
    MAX_VIDEO_FRAME_SIZE, STANDARD_DATAGRAM_SIZE, VIDEO_RECOVERY_HEADER_LEN,
};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet, VecDeque},
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
    InvalidDatagramSize,
}

impl fmt::Display for VideoFragmentationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VideoFragmentationError::EmptyFrame => write!(f, "video frame is empty"),
            VideoFragmentationError::FrameTooLarge => {
                write!(f, "video frame exceeds the protocol limit")
            }
            VideoFragmentationError::InvalidDatagramSize => {
                write!(f, "invalid negotiated video datagram size")
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
    fragment_video_frame_with_datagram_size(
        frame_id,
        timestamp_us,
        is_keyframe,
        payload,
        STANDARD_DATAGRAM_SIZE,
    )
}

pub fn fragment_video_frame_with_datagram_size(
    frame_id: u64,
    timestamp_us: u64,
    is_keyframe: bool,
    payload: &[u8],
    datagram_size: usize,
) -> Result<Vec<VideoFragment<'_>>, VideoFragmentationError> {
    if payload.is_empty() {
        return Err(VideoFragmentationError::EmptyFrame);
    }
    if payload.len() > MAX_VIDEO_FRAME_SIZE {
        return Err(VideoFragmentationError::FrameTooLarge);
    }

    if !(STANDARD_DATAGRAM_SIZE..=MAX_DATAGRAM_SIZE).contains(&datagram_size) {
        return Err(VideoFragmentationError::InvalidDatagramSize);
    }
    // Reserve four payload bytes in every datagram so the parity packet can
    // carry the exact encoded-frame length without exceeding the negotiated
    // MTU. One XOR packet per frame repairs any single lost UDP fragment.
    let fragment_payload = datagram_size
        - (MAX_DATAGRAM_SIZE - MAX_VIDEO_FRAGMENT_PAYLOAD)
        - VIDEO_RECOVERY_HEADER_LEN;
    let total = payload.len().div_ceil(fragment_payload);
    if total > MAX_VIDEO_FRAGMENTS || total > u16::MAX as usize {
        return Err(VideoFragmentationError::FrameTooLarge);
    }

    let mut fragments: Vec<_> = payload
        .chunks(fragment_payload)
        .enumerate()
        .map(|(index, chunk)| VideoFragment {
            frame_id,
            timestamp_us,
            index: index as u16,
            total: total as u16,
            is_keyframe,
            is_recovery: false,
            payload: Cow::Borrowed(chunk),
        })
        .collect();
    if total > 1 {
        let mut recovery = vec![0_u8; VIDEO_RECOVERY_HEADER_LEN + fragment_payload];
        recovery[..VIDEO_RECOVERY_HEADER_LEN]
            .copy_from_slice(&(payload.len() as u32).to_be_bytes());
        for chunk in payload.chunks(fragment_payload) {
            for (parity, byte) in recovery[VIDEO_RECOVERY_HEADER_LEN..].iter_mut().zip(chunk) {
                *parity ^= *byte;
            }
        }
        fragments.push(VideoFragment {
            frame_id,
            timestamp_us,
            index: total as u16,
            total: total as u16,
            is_keyframe,
            is_recovery: true,
            payload: Cow::Owned(recovery),
        });
    }
    Ok(fragments)
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
    completed: HashSet<u64>,
    completed_order: VecDeque<u64>,
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
            completed: HashSet::new(),
            completed_order: VecDeque::new(),
            max_pending_frames,
            max_age,
        }
    }

    pub fn push(
        &mut self,
        fragment: VideoFragment<'_>,
    ) -> Result<Option<VideoFrame>, VideoReassemblyError> {
        self.expire_stale();
        self.validate_fragment(&fragment)?;
        let frame_id = fragment.frame_id;
        if self.completed.contains(&frame_id) {
            return Ok(None);
        }

        if !self.frames.contains_key(&fragment.frame_id)
            && self.frames.len() >= self.max_pending_frames
        {
            self.drop_oldest();
        }

        let frame = self
            .frames
            .entry(fragment.frame_id)
            .or_insert_with(|| PendingVideoFrame::new(&fragment));

        if !frame.matches(&fragment) {
            self.frames.remove(&frame_id);
            return Err(VideoReassemblyError::FrameMetadataChanged);
        }

        frame.push(fragment);
        if !frame.is_complete() {
            return Ok(None);
        }

        let frame = self.frames.remove(&frame_id).unwrap().finish()?;
        self.completed.insert(frame_id);
        self.completed_order.push_back(frame_id);
        while self.completed_order.len() > 64 {
            if let Some(expired) = self.completed_order.pop_front() {
                self.completed.remove(&expired);
            }
        }
        Ok(Some(frame))
    }

    pub fn pending_frames(&self) -> usize {
        self.frames.len()
    }

    pub fn expire_stale(&mut self) -> usize {
        let before = self.frames.len();
        let max_age = self.max_age;
        self.frames
            .retain(|_, frame| frame.last_updated.elapsed() <= max_age);
        before - self.frames.len()
    }

    fn validate_fragment(&self, fragment: &VideoFragment<'_>) -> Result<(), VideoReassemblyError> {
        if fragment.total == 0
            || (!fragment.is_recovery && fragment.index >= fragment.total)
            || (fragment.is_recovery && fragment.index != fragment.total)
            || fragment.total as usize > MAX_VIDEO_FRAGMENTS
            || fragment.payload.is_empty()
            || fragment.payload.len() > MAX_VIDEO_FRAGMENT_PAYLOAD
            || (fragment.is_recovery && fragment.payload.len() <= VIDEO_RECOVERY_HEADER_LEN)
        {
            return Err(VideoReassemblyError::InvalidFragment);
        }
        Ok(())
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
    recovery: Option<Vec<u8>>,
    last_updated: Instant,
}

impl PendingVideoFrame {
    fn new(fragment: &VideoFragment<'_>) -> Self {
        Self {
            frame_id: fragment.frame_id,
            timestamp_us: fragment.timestamp_us,
            is_keyframe: fragment.is_keyframe,
            total: fragment.total,
            chunks: vec![None; fragment.total as usize],
            received: 0,
            encoded_size: 0,
            recovery: None,
            last_updated: Instant::now(),
        }
    }

    fn matches(&self, fragment: &VideoFragment<'_>) -> bool {
        self.timestamp_us == fragment.timestamp_us
            && self.is_keyframe == fragment.is_keyframe
            && self.total == fragment.total
    }

    fn push(&mut self, fragment: VideoFragment<'_>) {
        self.last_updated = Instant::now();
        if fragment.is_recovery {
            if self.recovery.is_none() {
                self.recovery = Some(fragment.payload.into_owned());
            }
            return;
        }
        let slot = &mut self.chunks[fragment.index as usize];
        if slot.is_none() {
            self.encoded_size += fragment.payload.len();
            *slot = Some(fragment.payload.into_owned());
            self.received += 1;
        }
    }

    fn is_complete(&self) -> bool {
        self.received == self.total as usize
            || (self.recovery.is_some() && self.received + 1 == self.total as usize)
    }

    fn finish(mut self) -> Result<VideoFrame, VideoReassemblyError> {
        if self.received + 1 == self.total as usize {
            self.recover_missing_fragment()?;
        }
        if self.encoded_size > MAX_VIDEO_FRAME_SIZE || self.received != self.total as usize {
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

    fn recover_missing_fragment(&mut self) -> Result<(), VideoReassemblyError> {
        let recovery = self
            .recovery
            .take()
            .ok_or(VideoReassemblyError::InvalidFragment)?;
        let declared_size = u32::from_be_bytes(
            recovery[..VIDEO_RECOVERY_HEADER_LEN]
                .try_into()
                .map_err(|_| VideoReassemblyError::InvalidFragment)?,
        ) as usize;
        let chunk_size = recovery.len() - VIDEO_RECOVERY_HEADER_LEN;
        let total = self.total as usize;
        if declared_size == 0
            || declared_size > MAX_VIDEO_FRAME_SIZE
            || chunk_size == 0
            || declared_size <= chunk_size.saturating_mul(total.saturating_sub(1))
            || declared_size > chunk_size.saturating_mul(total)
        {
            return Err(VideoReassemblyError::InvalidFragment);
        }
        let missing = self
            .chunks
            .iter()
            .position(Option::is_none)
            .ok_or(VideoReassemblyError::InvalidFragment)?;
        let missing_size = if missing + 1 == total {
            declared_size - chunk_size * (total - 1)
        } else {
            chunk_size
        };
        let mut recovered = recovery[VIDEO_RECOVERY_HEADER_LEN..].to_vec();
        for chunk in self.chunks.iter().flatten() {
            if chunk.len() > recovered.len() {
                return Err(VideoReassemblyError::InvalidFragment);
            }
            for (target, byte) in recovered.iter_mut().zip(chunk) {
                *target ^= *byte;
            }
        }
        recovered.truncate(missing_size);
        self.encoded_size = self.encoded_size.saturating_add(recovered.len());
        self.chunks[missing] = Some(recovered);
        self.received += 1;
        if self.encoded_size != declared_size {
            return Err(VideoReassemblyError::InvalidFragment);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{JUMBO_DATAGRAM_SIZE, STANDARD_VIDEO_FRAGMENT_PAYLOAD};

    #[test]
    fn fragments_and_reassembles_out_of_order() {
        let payload = vec![7; STANDARD_VIDEO_FRAGMENT_PAYLOAD * 2 + 17];
        let fragments = fragment_video_frame(42, 123_456, true, &payload).unwrap();
        assert_eq!(fragments.len(), 4);

        let mut reassembler = VideoReassembler::new(3, Duration::from_secs(1));
        assert_eq!(reassembler.push(fragments[2].clone()).unwrap(), None);
        assert_eq!(reassembler.push(fragments[0].clone()).unwrap(), None);
        let frame = reassembler.push(fragments[1].clone()).unwrap().unwrap();

        assert_eq!(frame.frame_id, 42);
        assert_eq!(frame.timestamp_us, 123_456);
        assert!(frame.is_keyframe);
        assert_eq!(frame.payload, payload);
    }

    #[test]
    fn duplicate_fragment_is_ignored() {
        let payload = vec![1; STANDARD_VIDEO_FRAGMENT_PAYLOAD + 1];
        let fragments = fragment_video_frame(8, 99, false, &payload).unwrap();
        let mut reassembler = VideoReassembler::new(2, Duration::from_secs(1));

        assert_eq!(reassembler.push(fragments[0].clone()).unwrap(), None);
        assert_eq!(reassembler.push(fragments[0].clone()).unwrap(), None);
        assert_eq!(
            reassembler
                .push(fragments[1].clone())
                .unwrap()
                .unwrap()
                .payload,
            payload
        );
    }

    #[test]
    fn recovery_fragment_repairs_one_lost_datagram() {
        let payload = (0..STANDARD_VIDEO_FRAGMENT_PAYLOAD * 3 + 17)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let fragments = fragment_video_frame(11, 500, false, &payload).unwrap();
        let delayed_fragment = fragments[1].clone();
        let mut reassembler = VideoReassembler::new(3, Duration::from_secs(1));
        let mut completed = None;
        for (index, fragment) in fragments.into_iter().enumerate() {
            if index != 1 {
                completed = reassembler.push(fragment).unwrap().or(completed);
            }
        }
        assert_eq!(completed.unwrap().payload, payload);
        assert_eq!(reassembler.push(delayed_fragment).unwrap(), None);
        assert_eq!(reassembler.pending_frames(), 0);
    }

    #[test]
    fn recovery_fragment_does_not_emit_with_two_losses() {
        let payload = vec![9; STANDARD_VIDEO_FRAGMENT_PAYLOAD * 3];
        let fragments = fragment_video_frame(12, 600, false, &payload).unwrap();
        let mut reassembler = VideoReassembler::new(3, Duration::from_secs(1));
        let mut completed = None;
        for (index, fragment) in fragments.into_iter().enumerate() {
            if index != 0 && index != 2 {
                completed = reassembler.push(fragment).unwrap().or(completed);
            }
        }
        assert!(completed.is_none());
        assert_eq!(reassembler.pending_frames(), 1);
    }

    #[test]
    fn pending_frames_are_bounded() {
        let payload = vec![0; STANDARD_VIDEO_FRAGMENT_PAYLOAD + 1];
        let first = fragment_video_frame(1, 0, false, &payload).unwrap();
        let second = fragment_video_frame(2, 1, false, &payload).unwrap();
        let mut reassembler = VideoReassembler::new(1, Duration::from_secs(1));

        reassembler.push(first[0].clone()).unwrap();
        reassembler.push(second[0].clone()).unwrap();
        assert_eq!(reassembler.pending_frames(), 1);
    }

    #[test]
    fn jumbo_mode_reduces_fragment_count_and_round_trips() {
        let payload = vec![3; 32_000];
        let standard = fragment_video_frame(7, 10, true, &payload).unwrap();
        let jumbo =
            fragment_video_frame_with_datagram_size(7, 10, true, &payload, JUMBO_DATAGRAM_SIZE)
                .unwrap();
        assert!(jumbo.len() < standard.len());

        let mut reassembler = VideoReassembler::new(2, Duration::from_secs(1));
        let mut completed = None;
        for fragment in jumbo {
            completed = reassembler.push(fragment).unwrap().or(completed);
        }
        assert_eq!(completed.unwrap().payload, payload);
    }

    #[test]
    fn rejects_unnegotiated_datagram_sizes() {
        assert_eq!(
            fragment_video_frame_with_datagram_size(1, 0, false, &[1], 1399),
            Err(VideoFragmentationError::InvalidDatagramSize)
        );
        assert_eq!(
            fragment_video_frame_with_datagram_size(1, 0, false, &[1], MAX_DATAGRAM_SIZE + 1,),
            Err(VideoFragmentationError::InvalidDatagramSize)
        );
    }
}
