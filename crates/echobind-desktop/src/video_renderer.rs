//! Draws decoded 4:2:0 frames by uploading their planes as they are and
//! converting to RGB in the fragment shader.
//!
//! Converting on the CPU and uploading RGBA cost a software viewer several
//! milliseconds per frame on top of decoding, and moved 4 bytes per pixel to
//! the GPU instead of 1.5.

use eframe::{
    egui,
    egui_wgpu::{self, CallbackResources, CallbackTrait},
    wgpu,
};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

const YUV_SHADER: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    var uvs = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 1.0),
        vec2<f32>(2.0, 1.0),
        vec2<f32>(0.0, -1.0),
    );
    var output: VertexOutput;
    output.position = vec4<f32>(positions[vertex_index], 0.0, 1.0);
    output.uv = uvs[vertex_index];
    return output;
}

@group(0) @binding(0) var y_plane: texture_2d<f32>;
@group(0) @binding(1) var chroma_a: texture_2d<f32>;
@group(0) @binding(2) var chroma_b: texture_2d<f32>;
@group(0) @binding(3) var video_sampler: sampler;

// NVENC and VideoToolbox both signal limited-range BT.709.
fn limited_bt709(y_sample: f32, chroma: vec2<f32>) -> vec4<f32> {
    let y = max(0.0, (y_sample - 16.0 / 255.0) * (255.0 / 219.0));
    let c = chroma - vec2<f32>(0.5);
    let rgb = vec3<f32>(
        y + 1.792741 * c.y,
        y - 0.213249 * c.x - 0.532909 * c.y,
        y + 2.112402 * c.x,
    );
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}

@fragment
fn fs_nv12(input: VertexOutput) -> @location(0) vec4<f32> {
    let y = textureSample(y_plane, video_sampler, input.uv).r;
    let chroma = textureSample(chroma_a, video_sampler, input.uv).rg;
    return limited_bt709(y, chroma);
}

@fragment
fn fs_i420(input: VertexOutput) -> @location(0) vec4<f32> {
    let y = textureSample(y_plane, video_sampler, input.uv).r;
    let u = textureSample(chroma_a, video_sampler, input.uv).r;
    let v = textureSample(chroma_b, video_sampler, input.uv).r;
    return limited_bt709(y, vec2<f32>(u, v));
}
"#;

/// One image plane, `stride` bytes per row.
#[derive(Clone, Debug)]
pub struct YuvPlane {
    pub data: Vec<u8>,
    pub stride: usize,
}

impl YuvPlane {
    /// Copies the first `rows` rows of a plane whose rows are `stride`
    /// bytes apart, keeping that stride so the copy is one `memcpy`.
    pub fn copy_rows(source: &[u8], stride: usize, row_bytes: usize, rows: usize) -> Self {
        let length = stride * rows.saturating_sub(1) + row_bytes;
        Self {
            data: source[..length.min(source.len())].to_vec(),
            stride,
        }
    }
}

/// A decoded 4:2:0 picture in one of the layouts decoders produce.
#[derive(Clone, Debug)]
pub enum YuvFrame {
    /// Full-size Y, then half-size interleaved UV (hardware decoders).
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    Nv12 { y: YuvPlane, uv: YuvPlane },
    /// Full-size Y, then half-size U and V (OpenH264).
    I420 {
        y: YuvPlane,
        u: YuvPlane,
        v: YuvPlane,
    },
}

impl YuvFrame {
    fn layout(&self) -> YuvLayout {
        match self {
            Self::Nv12 { .. } => YuvLayout::Nv12,
            Self::I420 { .. } => YuvLayout::I420,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum YuvLayout {
    Nv12,
    I420,
}

struct UploadedFrame {
    layout: YuvLayout,
    width: u32,
    height: u32,
    y: wgpu::Texture,
    chroma_a: wgpu::Texture,
    chroma_b: Option<wgpu::Texture>,
    bind_group: wgpu::BindGroup,
    published_at: Instant,
    presented: AtomicBool,
}

pub(super) struct YuvVideoRenderer {
    nv12_pipeline: wgpu::RenderPipeline,
    i420_pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    frame: Option<UploadedFrame>,
    presented_frames: Arc<AtomicU64>,
    present_latency_us: Arc<AtomicU64>,
}

impl YuvVideoRenderer {
    pub(super) fn new(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        presented_frames: Arc<AtomicU64>,
        present_latency_us: Arc<AtomicU64>,
    ) -> Self {
        let plane_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("echobind_yuv_bind_group_layout"),
            entries: &[
                plane_entry(0),
                plane_entry(1),
                plane_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("echobind_yuv_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("echobind_yuv_shader"),
            source: wgpu::ShaderSource::Wgsl(YUV_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("echobind_yuv_pipeline_layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = |entry_point, label| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                },
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(entry_point),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target_format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                }),
                multiview_mask: None,
                cache: None,
            })
        };

        Self {
            nv12_pipeline: pipeline("fs_nv12", "echobind_yuv_nv12_pipeline"),
            i420_pipeline: pipeline("fs_i420", "echobind_yuv_i420_pipeline"),
            bind_group_layout,
            sampler,
            frame: None,
            presented_frames,
            present_latency_us,
        }
    }

    pub(super) fn set_frame(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: usize,
        height: usize,
        yuv: &YuvFrame,
        published_at: Instant,
    ) -> Result<(), String> {
        let (width, height) = (
            u32::try_from(width).map_err(|_| "video frame is too wide")?,
            u32::try_from(height).map_err(|_| "video frame is too tall")?,
        );
        if width == 0 || height == 0 {
            return Err("video frame has no pixels".to_owned());
        }
        let layout = yuv.layout();
        let reusable = self.frame.as_ref().is_some_and(|frame| {
            frame.layout == layout && frame.width == width && frame.height == height
        });
        if !reusable {
            self.frame = Some(self.allocate(device, layout, width, height));
        }
        let frame = self.frame.as_mut().expect("video textures were allocated");
        let (chroma_width, chroma_height) = (width.div_ceil(2), height.div_ceil(2));
        match yuv {
            YuvFrame::Nv12 { y, uv } => {
                write_plane(queue, &frame.y, y, width, height, 1)?;
                write_plane(queue, &frame.chroma_a, uv, chroma_width, chroma_height, 2)?;
            }
            YuvFrame::I420 { y, u, v } => {
                let chroma_b = frame
                    .chroma_b
                    .as_ref()
                    .expect("I420 frames allocate a V plane");
                write_plane(queue, &frame.y, y, width, height, 1)?;
                write_plane(queue, &frame.chroma_a, u, chroma_width, chroma_height, 1)?;
                write_plane(queue, chroma_b, v, chroma_width, chroma_height, 1)?;
            }
        }
        frame.published_at = published_at;
        frame.presented.store(false, Ordering::Relaxed);
        Ok(())
    }

    pub(super) fn clear(&mut self) {
        self.frame = None;
    }

    fn allocate(
        &self,
        device: &wgpu::Device,
        layout: YuvLayout,
        width: u32,
        height: u32,
    ) -> UploadedFrame {
        let plane = |label, width, height, format| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
        };
        let (chroma_width, chroma_height) = (width.div_ceil(2), height.div_ceil(2));
        let y = plane(
            "echobind_yuv_y",
            width,
            height,
            wgpu::TextureFormat::R8Unorm,
        );
        let (chroma_a, chroma_b) = match layout {
            YuvLayout::Nv12 => (
                plane(
                    "echobind_yuv_uv",
                    chroma_width,
                    chroma_height,
                    wgpu::TextureFormat::Rg8Unorm,
                ),
                None,
            ),
            YuvLayout::I420 => (
                plane(
                    "echobind_yuv_u",
                    chroma_width,
                    chroma_height,
                    wgpu::TextureFormat::R8Unorm,
                ),
                Some(plane(
                    "echobind_yuv_v",
                    chroma_width,
                    chroma_height,
                    wgpu::TextureFormat::R8Unorm,
                )),
            ),
        };
        let y_view = y.create_view(&wgpu::TextureViewDescriptor::default());
        let a_view = chroma_a.create_view(&wgpu::TextureViewDescriptor::default());
        // NV12 has no third plane; its shader never samples binding 2.
        let b_view = chroma_b.as_ref().map_or_else(
            || a_view.clone(),
            |texture| texture.create_view(&wgpu::TextureViewDescriptor::default()),
        );
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("echobind_yuv_bind_group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&a_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&b_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        UploadedFrame {
            layout,
            width,
            height,
            y,
            chroma_a,
            chroma_b,
            bind_group,
            published_at: Instant::now(),
            presented: AtomicBool::new(true),
        }
    }

    fn draw(&self, render_pass: &mut wgpu::RenderPass<'_>) {
        let Some(frame) = &self.frame else {
            return;
        };
        render_pass.set_pipeline(match frame.layout {
            YuvLayout::Nv12 => &self.nv12_pipeline,
            YuvLayout::I420 => &self.i420_pipeline,
        });
        render_pass.set_bind_group(0, &frame.bind_group, &[]);
        render_pass.draw(0..3, 0..1);
        if !frame.presented.swap(true, Ordering::Relaxed) {
            self.presented_frames.fetch_add(1, Ordering::Relaxed);
            self.present_latency_us.fetch_add(
                frame
                    .published_at
                    .elapsed()
                    .as_micros()
                    .min(u128::from(u64::MAX)) as u64,
                Ordering::Relaxed,
            );
        }
    }
}

fn write_plane(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    plane: &YuvPlane,
    width: u32,
    height: u32,
    bytes_per_texel: u32,
) -> Result<(), String> {
    let row_bytes = width as usize * bytes_per_texel as usize;
    let required = plane.stride * (height as usize - 1) + row_bytes;
    if plane.stride < row_bytes || plane.data.len() < required {
        return Err(format!(
            "video plane holds {} bytes with stride {}, but {width}x{height} needs {required}",
            plane.data.len(),
            plane.stride
        ));
    }
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &plane.data[..required],
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(
                u32::try_from(plane.stride).map_err(|_| "video plane stride is too large")?,
            ),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    Ok(())
}

/// The largest viewport side wgpu accepts on any adapter.
const MAX_VIEWPORT_PX: f32 = 8192.0;

/// The viewport, in pixels, that stretches the frame across `video`.
///
/// egui clamps a callback's viewport to the screen, which would squash a
/// cropped picture. A viewport may reach past the screen, though, so this one
/// covers the whole picture and the paint's clip rect keeps only what shows.
/// None when it is too large for wgpu, which keeps egui's own.
pub(super) fn video_viewport(video: egui::Rect, pixels_per_point: f32) -> Option<[f32; 4]> {
    let size = video.size() * pixels_per_point;
    (size.x <= MAX_VIEWPORT_PX && size.y <= MAX_VIEWPORT_PX).then_some([
        video.min.x * pixels_per_point,
        video.min.y * pixels_per_point,
        size.x,
        size.y,
    ])
}

pub(super) fn set_video_viewport(
    render_pass: &mut wgpu::RenderPass<'_>,
    video: egui::Rect,
    pixels_per_point: f32,
) {
    if let Some([x, y, width, height]) = video_viewport(video, pixels_per_point) {
        render_pass.set_viewport(x, y, width, height, 0.0, 1.0);
    }
}

/// Paints the frame across `video`, which may reach past the paint's rect.
#[derive(Clone)]
struct YuvVideoPaintCallback {
    video: egui::Rect,
}

impl CallbackTrait for YuvVideoPaintCallback {
    fn paint(
        &self,
        info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &CallbackResources,
    ) {
        if let Some(renderer) = callback_resources.get::<YuvVideoRenderer>() {
            set_video_viewport(render_pass, self.video, info.pixels_per_point);
            renderer.draw(render_pass);
        }
    }
}

/// Paints the visible `rect` of a frame laid out across `video`. Paint it
/// with a painter clipped to `rect`.
pub(super) fn paint_callback(rect: egui::Rect, video: egui::Rect) -> egui::PaintCallback {
    egui_wgpu::Callback::new_paint_callback(rect, YuvVideoPaintCallback { video })
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIDTH: u32 = 8;
    const HEIGHT: u32 = 4;

    /// Keeps the GPU tests to one device at a time, which a software adapter
    /// such as WARP on a CI runner handles more reliably.
    static GPU: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn gpu() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .ok()?;
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()
    }

    /// Renders `frame` into an RGBA texture, across `video` when given, and
    /// reads back its pixels.
    fn render(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: &YuvFrame,
        video: Option<egui::Rect>,
    ) -> Vec<[u8; 4]> {
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let mut renderer = YuvVideoRenderer::new(
            device,
            format,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        );
        renderer
            .set_frame(
                device,
                queue,
                WIDTH as usize,
                HEIGHT as usize,
                frame,
                Instant::now(),
            )
            .unwrap();
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 256 * u64::from(HEIGHT),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(video) = video {
                set_video_viewport(&mut pass, video, 1.0);
            }
            renderer.draw(&mut pass);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(HEIGHT),
                },
            },
            wgpu::Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);
        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let bytes = readback.slice(..).get_mapped_range();
        (0..HEIGHT as usize)
            .flat_map(|row| {
                let bytes = &bytes;
                (0..WIDTH as usize).map(move |column| {
                    let offset = row * 256 + column * 4;
                    bytes[offset..offset + 4].try_into().unwrap()
                })
            })
            .collect()
    }

    /// A plane filled with `value`, padded to `stride` like decoder output.
    fn plane(value: u8, row_bytes: usize, stride: usize, rows: usize) -> YuvPlane {
        let mut data = vec![0xEE; stride * rows];
        for row in data.chunks_mut(stride) {
            row[..row_bytes].fill(value);
        }
        YuvPlane { data, stride }
    }

    fn assert_close(pixels: &[[u8; 4]], expected: [u8; 3]) {
        for pixel in pixels {
            for channel in 0..3 {
                assert!(
                    pixel[channel].abs_diff(expected[channel]) <= 2,
                    "got {pixel:?}, expected about {expected:?}"
                );
            }
        }
    }

    #[test]
    fn padded_i420_and_nv12_planes_render_the_same_colour() {
        let _gpu = GPU
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((device, queue)) = gpu() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let (w, h) = (WIDTH as usize, HEIGHT as usize);
        // Limited-range BT.709 Y=63, Cb=102, Cr=240 is pure red.
        let i420 = YuvFrame::I420 {
            y: plane(63, w, w + 24, h),
            u: plane(102, w / 2, w / 2 + 8, h / 2),
            v: plane(240, w / 2, w / 2 + 8, h / 2),
        };
        let mut uv = plane(0, w, w + 16, h / 2);
        for row in uv.data.chunks_mut(uv.stride) {
            for pair in row[..w].chunks_mut(2) {
                pair.copy_from_slice(&[102, 240]);
            }
        }
        let nv12 = YuvFrame::Nv12 {
            y: plane(63, w, w + 40, h),
            uv,
        };

        let i420_pixels = render(&device, &queue, &i420, None);
        let nv12_pixels = render(&device, &queue, &nv12, None);
        assert_close(&i420_pixels, [255, 0, 0]);
        assert_eq!(i420_pixels, nv12_pixels);
    }

    #[test]
    fn a_picture_wider_than_the_target_is_cropped_not_squashed() {
        let _gpu = GPU
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((device, queue)) = gpu() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let (w, h) = (WIDTH as usize, HEIGHT as usize);
        // Black on the left half, white on the right.
        let mut y = plane(16, w, w + 8, h);
        for row in y.data.chunks_mut(w + 8) {
            row[w / 2..w].fill(235);
        }
        let frame = YuvFrame::I420 {
            y,
            u: plane(128, w / 2, w / 2 + 8, h / 2),
            v: plane(128, w / 2, w / 2 + 8, h / 2),
        };
        let columns = |pixels: &[[u8; 4]], range: std::ops::Range<usize>| -> Vec<[u8; 4]> {
            pixels
                .chunks(w)
                .flat_map(|row| row[range.clone()].to_vec())
                .collect()
        };

        let whole = render(&device, &queue, &frame, None);
        assert_close(&columns(&whole, 0..w / 2), [0, 0, 0]);
        // Twice the target's width, panned to its right half.
        let video = egui::Rect::from_min_size(
            egui::pos2(-(WIDTH as f32), 0.0),
            egui::vec2(2.0 * WIDTH as f32, HEIGHT as f32),
        );
        let cropped = render(&device, &queue, &frame, Some(video));
        // Column 0 blends with the black texel across the crop's edge.
        assert_close(&columns(&cropped, 1..w), [255, 255, 255]);
    }

    #[test]
    fn viewports_too_large_for_wgpu_are_left_to_egui() {
        let video = egui::Rect::from_min_size(egui::pos2(-640.0, 0.0), egui::vec2(2580.0, 1080.0));
        assert_eq!(
            video_viewport(video, 2.0),
            Some([-1280.0, 0.0, 5160.0, 2160.0])
        );
        assert_eq!(video_viewport(video, 4.0), None);
    }

    #[test]
    fn short_planes_are_rejected_instead_of_read_past_their_end() {
        let _gpu = GPU
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((device, queue)) = gpu() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut renderer = YuvVideoRenderer::new(
            &device,
            wgpu::TextureFormat::Rgba8Unorm,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        );
        let frame = YuvFrame::Nv12 {
            y: YuvPlane {
                data: vec![16; 8 * 3],
                stride: 8,
            },
            uv: YuvPlane {
                data: vec![128; 8 * 2],
                stride: 8,
            },
        };
        let error = renderer
            .set_frame(&device, &queue, 8, 4, &frame, Instant::now())
            .unwrap_err();
        assert!(error.contains("needs"), "{error}");
    }
}
