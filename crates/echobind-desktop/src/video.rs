use openh264::formats::YUVSource;
use scap::frame::{BGRAFrame, YUVFrame};

#[derive(Default)]
pub struct I420Frame {
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl I420Frame {
    pub fn update_from_nv12(&mut self, frame: &YUVFrame) -> Result<(), String> {
        let width = usize::try_from(frame.width).map_err(|_| "negative capture width")?;
        let height = usize::try_from(frame.height).map_err(|_| "negative capture height")?;
        let y_stride =
            usize::try_from(frame.luminance_stride).map_err(|_| "negative luminance stride")?;
        let uv_stride =
            usize::try_from(frame.chrominance_stride).map_err(|_| "negative chrominance stride")?;

        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(format!(
                "capture dimensions must be positive and even, got {width}x{height}"
            ));
        }
        if y_stride < width || uv_stride < width {
            return Err("capture stride is smaller than its visible width".to_owned());
        }
        if frame.luminance_bytes.len() < y_stride * height
            || frame.chrominance_bytes.len() < uv_stride * (height / 2)
        {
            return Err("capture planes are smaller than their declared dimensions".to_owned());
        }

        self.width = width;
        self.height = height;
        self.y.resize(width * height, 0);
        self.u.resize(width * height / 4, 0);
        self.v.resize(width * height / 4, 0);

        for row in 0..height {
            let source = &frame.luminance_bytes[row * y_stride..row * y_stride + width];
            let destination = &mut self.y[row * width..(row + 1) * width];
            destination.copy_from_slice(source);
        }

        let chroma_width = width / 2;
        for row in 0..height / 2 {
            let source = &frame.chrominance_bytes[row * uv_stride..row * uv_stride + width];
            let u_row = &mut self.u[row * chroma_width..(row + 1) * chroma_width];
            let v_row = &mut self.v[row * chroma_width..(row + 1) * chroma_width];
            for (column, pair) in source.chunks_exact(2).enumerate() {
                u_row[column] = pair[0];
                v_row[column] = pair[1];
            }
        }

        Ok(())
    }

    pub fn update_from_bgra(&mut self, frame: &BGRAFrame) -> Result<(), String> {
        let width = usize::try_from(frame.width).map_err(|_| "negative capture width")?;
        let height = usize::try_from(frame.height).map_err(|_| "negative capture height")?;

        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(format!(
                "capture dimensions must be positive and even, got {width}x{height}"
            ));
        }
        let required_bytes = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| "capture dimensions overflow the address space".to_owned())?;
        if frame.data.len() < required_bytes {
            return Err("BGRA capture buffer is smaller than its declared dimensions".to_owned());
        }

        self.width = width;
        self.height = height;
        self.y.resize(width * height, 0);
        self.u.resize(width * height / 4, 0);
        self.v.resize(width * height / 4, 0);

        let chroma_width = width / 2;
        for row in (0..height).step_by(2) {
            let top_source = &frame.data[row * width * 4..(row + 1) * width * 4];
            let bottom_source = &frame.data[(row + 1) * width * 4..(row + 2) * width * 4];
            let (top_y, remaining_y) = self.y.split_at_mut((row + 1) * width);
            let top_y = &mut top_y[row * width..];
            let bottom_y = &mut remaining_y[..width];
            let chroma_row = row / 2;
            let u_row = &mut self.u[chroma_row * chroma_width..(chroma_row + 1) * chroma_width];
            let v_row = &mut self.v[chroma_row * chroma_width..(chroma_row + 1) * chroma_width];

            for column in (0..width).step_by(2) {
                let top_left = bgra_components(top_source, column);
                let top_right = bgra_components(top_source, column + 1);
                let bottom_left = bgra_components(bottom_source, column);
                let bottom_right = bgra_components(bottom_source, column + 1);

                top_y[column] = rgb_to_y(top_left);
                top_y[column + 1] = rgb_to_y(top_right);
                bottom_y[column] = rgb_to_y(bottom_left);
                bottom_y[column + 1] = rgb_to_y(bottom_right);

                let red = top_left.0 + top_right.0 + bottom_left.0 + bottom_right.0;
                let green = top_left.1 + top_right.1 + bottom_left.1 + bottom_right.1;
                let blue = top_left.2 + top_right.2 + bottom_left.2 + bottom_right.2;
                u_row[column / 2] =
                    clamp_u8(((-38 * red - 74 * green + 112 * blue + 512) >> 10) + 128);
                v_row[column / 2] =
                    clamp_u8(((112 * red - 94 * green - 18 * blue + 512) >> 10) + 128);
            }
        }

        Ok(())
    }
}

#[inline]
fn bgra_components(source: &[u8], column: usize) -> (i32, i32, i32) {
    let offset = column * 4;
    (
        i32::from(source[offset + 2]),
        i32::from(source[offset + 1]),
        i32::from(source[offset]),
    )
}

#[inline]
fn rgb_to_y((red, green, blue): (i32, i32, i32)) -> u8 {
    clamp_u8(((66 * red + 129 * green + 25 * blue + 128) >> 8) + 16)
}

#[inline]
fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

impl YUVSource for I420Frame {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.y
    }

    fn u(&self) -> &[u8] {
        &self.u
    }

    fn v(&self) -> &[u8] {
        &self.v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echobind_core::{
        protocol::{Packet, MAX_DATAGRAM_SIZE},
        video::{fragment_video_frame, VideoReassembler},
    };
    use openh264::{decoder::Decoder, encoder::Encoder};
    use std::time::Duration;
    #[test]
    fn converts_padded_nv12_to_i420() {
        let frame = YUVFrame {
            display_time: 0,
            width: 4,
            height: 2,
            luminance_bytes: vec![1, 2, 3, 4, 99, 99, 5, 6, 7, 8, 99, 99],
            luminance_stride: 6,
            chrominance_bytes: vec![10, 20, 11, 21, 99, 99],
            chrominance_stride: 6,
        };

        let mut converted = I420Frame::default();
        converted.update_from_nv12(&frame).unwrap();
        assert_eq!(converted.y(), &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(converted.u(), &[10, 11]);
        assert_eq!(converted.v(), &[20, 21]);
    }

    #[test]
    fn converts_bgra_to_i420() {
        let frame = BGRAFrame {
            display_time: 0,
            width: 2,
            height: 2,
            data: vec![
                0, 0, 255, 255, 0, 0, 255, 255, //
                0, 0, 255, 255, 0, 0, 255, 255,
            ],
        };

        let mut converted = I420Frame::default();
        converted.update_from_bgra(&frame).unwrap();
        assert_eq!(converted.y(), &[82, 82, 82, 82]);
        assert_eq!(converted.u(), &[90]);
        assert_eq!(converted.v(), &[240]);
    }

    #[test]
    fn rejects_truncated_bgra_frames() {
        let frame = BGRAFrame {
            display_time: 0,
            width: 2,
            height: 2,
            data: vec![0; 15],
        };

        let error = I420Frame::default().update_from_bgra(&frame).unwrap_err();
        assert!(error.contains("smaller"));
    }

    #[test]
    fn video_survives_encode_packetize_reassemble_decode() {
        let width = 128;
        let height = 72;
        let frame = YUVFrame {
            display_time: 0,
            width,
            height,
            luminance_bytes: vec![96; (width * height) as usize],
            luminance_stride: width,
            chrominance_bytes: vec![128; (width * height / 2) as usize],
            chrominance_stride: width,
        };
        let mut source = I420Frame::default();
        source.update_from_nv12(&frame).unwrap();
        let mut encoder = Encoder::new().unwrap();
        let encoded = encoder.encode(&source).unwrap().to_vec();
        assert!(!encoded.is_empty());

        let fragments = fragment_video_frame(7, 12_345, true, &encoded).unwrap();
        let mut reassembler = VideoReassembler::new(3, Duration::from_secs(1));
        let mut packet = Vec::with_capacity(MAX_DATAGRAM_SIZE);
        let mut reassembled = None;

        for fragment in fragments.into_iter().rev() {
            Packet::Video(fragment).encode(&mut packet);
            let parsed = Packet::try_from(packet.as_slice()).unwrap();
            let Packet::Video(parsed) = parsed else {
                panic!("encoded packet changed type");
            };
            if let Some(frame) = reassembler.push(parsed).unwrap() {
                reassembled = Some(frame);
            }
        }

        let mut decoder = Decoder::new().unwrap();
        let decoded = decoder
            .decode(&reassembled.unwrap().payload)
            .unwrap()
            .expect("decoder should return the intra frame");
        assert_eq!(decoded.dimensions(), (width as usize, height as usize));
    }
}
