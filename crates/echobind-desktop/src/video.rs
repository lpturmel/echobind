#![cfg_attr(target_os = "windows", allow(dead_code))]

use openh264::formats::YUVSource;
use rayon::prelude::*;
use scap::frame::{BGRAFrame, YUVFrame};

#[derive(Default)]
pub struct I420Frame {
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    source_columns: Vec<usize>,
    source_rows: Vec<usize>,
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

    pub fn update_from_bgra_scaled(
        &mut self,
        frame: &BGRAFrame,
        max_width: u32,
        max_height: u32,
    ) -> Result<(), String> {
        let source_width = usize::try_from(frame.width).map_err(|_| "negative capture width")?;
        let source_height = usize::try_from(frame.height).map_err(|_| "negative capture height")?;

        if source_width == 0 || source_height == 0 {
            return Err(format!(
                "capture dimensions must be positive, got {source_width}x{source_height}"
            ));
        }
        let required_bytes = source_width
            .checked_mul(source_height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| "capture dimensions overflow the address space".to_owned())?;
        if frame.data.len() < required_bytes {
            return Err("BGRA capture buffer is smaller than its declared dimensions".to_owned());
        }

        let (width, height) = fit_dimensions(source_width, source_height, max_width, max_height)?;
        self.width = width;
        self.height = height;
        self.y.resize(width * height, 0);
        self.u.resize(width * height / 4, 0);
        self.v.resize(width * height / 4, 0);
        self.source_columns.resize(width, 0);
        self.source_rows.resize(height, 0);
        for (column, source_column) in self.source_columns.iter_mut().enumerate() {
            *source_column = (column * source_width / width).min(source_width - 1);
        }
        for (row, source_row) in self.source_rows.iter_mut().enumerate() {
            *source_row = (row * source_height / height).min(source_height - 1);
        }

        let chroma_width = width / 2;
        self.y
            .par_chunks_mut(width * 2)
            .zip(self.u.par_chunks_mut(chroma_width))
            .zip(self.v.par_chunks_mut(chroma_width))
            .enumerate()
            .for_each(|(chroma_row, ((y_rows, u_row), v_row))| {
                let row = chroma_row * 2;
                let source_top_row = self.source_rows[row];
                let source_bottom_row = self.source_rows[row + 1];
                let (top_y, bottom_y) = y_rows.split_at_mut(width);

                for column in (0..width).step_by(2) {
                    let source_left = self.source_columns[column];
                    let source_right = self.source_columns[column + 1];
                    let top_left =
                        bgra_components(&frame.data, source_top_row * source_width + source_left);
                    let top_right =
                        bgra_components(&frame.data, source_top_row * source_width + source_right);
                    let bottom_left = bgra_components(
                        &frame.data,
                        source_bottom_row * source_width + source_left,
                    );
                    let bottom_right = bgra_components(
                        &frame.data,
                        source_bottom_row * source_width + source_right,
                    );

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
            });

        Ok(())
    }
}

#[inline]
fn bgra_components(source: &[u8], pixel: usize) -> (i32, i32, i32) {
    let offset = pixel * 4;
    (
        i32::from(source[offset + 2]),
        i32::from(source[offset + 1]),
        i32::from(source[offset]),
    )
}

fn fit_dimensions(
    width: usize,
    height: usize,
    max_width: u32,
    max_height: u32,
) -> Result<(usize, usize), String> {
    let max_width = usize::try_from(max_width).map_err(|_| "maximum width is too large")?;
    let max_height = usize::try_from(max_height).map_err(|_| "maximum height is too large")?;
    if max_width < 2 || max_height < 2 {
        return Err("maximum video dimensions must be at least 2x2".to_owned());
    }

    let scale = (max_width as f64 / width as f64)
        .min(max_height as f64 / height as f64)
        .min(1.0);
    let fitted_width = ((width as f64 * scale).floor() as usize).max(2) & !1;
    let fitted_height = ((height as f64 * scale).floor() as usize).max(2) & !1;
    Ok((fitted_width, fitted_height))
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
        converted.update_from_bgra_scaled(&frame, 2, 2).unwrap();
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

        let error = I420Frame::default()
            .update_from_bgra_scaled(&frame, 2, 2)
            .unwrap_err();
        assert!(error.contains("smaller"));
    }

    #[test]
    fn scales_bgra_while_converting() {
        let frame = BGRAFrame {
            display_time: 0,
            width: 4,
            height: 4,
            data: vec![
                0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, //
                0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, //
                0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, //
                0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255,
            ],
        };

        let mut converted = I420Frame::default();
        converted.update_from_bgra_scaled(&frame, 2, 2).unwrap();
        assert_eq!(converted.dimensions(), (2, 2));
        assert_eq!(converted.y(), &[82, 82, 82, 82]);
        assert_eq!(converted.u(), &[90]);
        assert_eq!(converted.v(), &[240]);
    }

    #[test]
    fn fits_dimensions_without_upscaling() {
        assert_eq!(fit_dimensions(2560, 1440, 1280, 720).unwrap(), (1280, 720));
        assert_eq!(fit_dimensions(1920, 1200, 1280, 720).unwrap(), (1152, 720));
        assert_eq!(fit_dimensions(800, 600, 1920, 1080).unwrap(), (800, 600));
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
