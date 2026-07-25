use apple_cf::{
    cf::CFType,
    cv::{CVMetalTextureCache, CVPixelBuffer},
    raw,
};
use eframe::{
    egui,
    egui_wgpu::{self, CallbackResources, CallbackTrait},
    wgpu,
};
use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_metal::{MTLTexture, MTLTextureType};
use std::{
    ffi::c_void,
    ptr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

const NV12_SHADER: &str = r#"
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
@group(0) @binding(1) var uv_plane: texture_2d<f32>;
@group(0) @binding(2) var video_sampler: sampler;

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    // NVENC H.264 and VideoToolbox use limited-range BT.709 for this path.
    let y_sample = textureSample(y_plane, video_sampler, input.uv).r;
    let chroma = textureSample(uv_plane, video_sampler, input.uv).rg - vec2<f32>(0.5);
    let y = max(0.0, (y_sample - 16.0 / 255.0) * (255.0 / 219.0));
    let rgb = vec3<f32>(
        y + 1.792741 * chroma.y,
        y - 0.213249 * chroma.x - 0.532909 * chroma.y,
        y + 2.112402 * chroma.x,
    );
    return vec4<f32>(rgb, 1.0);
}
"#;

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVMetalTextureCacheCreate(
        allocator: raw::CFAllocatorRef,
        cache_attributes: raw::CFDictionaryRef,
        metal_device: *mut c_void,
        texture_attributes: raw::CFDictionaryRef,
        cache_out: *mut raw::CVMetalTextureCacheRef,
    ) -> i32;
}

struct SendMetalTextureCache(CVMetalTextureCache);

// CoreVideo cache retain/release and texture creation are thread-safe.
unsafe impl Send for SendMetalTextureCache {}
unsafe impl Sync for SendMetalTextureCache {}

struct SendCvMetalTexture(CFType);

// CVMetalTexture is an immutable Core Foundation wrapper around id<MTLTexture>.
unsafe impl Send for SendCvMetalTexture {}
unsafe impl Sync for SendCvMetalTexture {}

struct ImportedNv12Frame {
    _pixel_buffer: CVPixelBuffer,
    _cv_y: SendCvMetalTexture,
    _cv_uv: SendCvMetalTexture,
    _y_texture: wgpu::Texture,
    _uv_texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    published_at: Instant,
    presented: AtomicBool,
}

pub(super) struct MacVideoRenderer {
    cache: SendMetalTextureCache,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    frame: Option<ImportedNv12Frame>,
    presented_frames: Arc<AtomicU64>,
    present_latency_us: Arc<AtomicU64>,
}

unsafe impl Send for MacVideoRenderer {}
unsafe impl Sync for MacVideoRenderer {}

impl MacVideoRenderer {
    pub(super) fn new(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        presented_frames: Arc<AtomicU64>,
        present_latency_us: Arc<AtomicU64>,
    ) -> Result<Self, String> {
        let metal_device = unsafe {
            device
                .as_hal::<wgpu::hal::metal::Api>()
                .ok_or_else(|| "wgpu is not using its Metal backend".to_owned())?
        };
        let mut cache = ptr::null_mut();
        let status = unsafe {
            CVMetalTextureCacheCreate(
                raw::kCFAllocatorDefault,
                ptr::null(),
                Retained::as_ptr(metal_device.raw_device())
                    .cast_mut()
                    .cast(),
                ptr::null(),
                &mut cache,
            )
        };
        if status != 0 {
            return Err(format!(
                "Unable to create the Metal video texture cache (status {status})"
            ));
        }
        let cache = CVMetalTextureCache::from_raw(cache.cast())
            .ok_or_else(|| "CoreVideo returned no Metal texture cache".to_owned())?;

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("echobind_nv12_bind_group_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("echobind_nv12_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("echobind_nv12_shader"),
            source: wgpu::ShaderSource::Wgsl(NV12_SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("echobind_nv12_pipeline_layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("echobind_nv12_pipeline"),
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
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            multiview_mask: None,
            cache: None,
        });

        Ok(Self {
            cache: SendMetalTextureCache(cache),
            pipeline,
            bind_group_layout,
            sampler,
            frame: None,
            presented_frames,
            present_latency_us,
        })
    }

    pub(super) fn set_frame(
        &mut self,
        device: &wgpu::Device,
        pixel_buffer: CVPixelBuffer,
        published_at: Instant,
    ) -> Result<(), String> {
        if pixel_buffer.plane_count() != 2 {
            return Err(format!(
                "NV12 pixel buffer has {} planes instead of 2",
                pixel_buffer.plane_count()
            ));
        }
        let y_width = pixel_buffer.width_of_plane(0);
        let y_height = pixel_buffer.height_of_plane(0);
        let uv_width = pixel_buffer.width_of_plane(1);
        let uv_height = pixel_buffer.height_of_plane(1);
        let cv_y = create_cv_metal_texture(
            &self.cache.0,
            &pixel_buffer,
            objc2_metal::MTLPixelFormat::R8Unorm.0,
            y_width,
            y_height,
            0,
        )?;
        let cv_uv = create_cv_metal_texture(
            &self.cache.0,
            &pixel_buffer,
            objc2_metal::MTLPixelFormat::RG8Unorm.0,
            uv_width,
            uv_height,
            1,
        )?;
        let y_texture = import_metal_texture(
            device,
            &cv_y.0,
            wgpu::TextureFormat::R8Unorm,
            y_width,
            y_height,
            "echobind_nv12_y",
        )?;
        let uv_texture = import_metal_texture(
            device,
            &cv_uv.0,
            wgpu::TextureFormat::Rg8Unorm,
            uv_width,
            uv_height,
            "echobind_nv12_uv",
        )?;
        let y_view = y_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let uv_view = uv_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("echobind_nv12_bind_group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&uv_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.frame = Some(ImportedNv12Frame {
            _pixel_buffer: pixel_buffer,
            _cv_y: cv_y,
            _cv_uv: cv_uv,
            _y_texture: y_texture,
            _uv_texture: uv_texture,
            bind_group,
            published_at,
            presented: AtomicBool::new(false),
        });
        Ok(())
    }

    pub(super) fn clear(&mut self) {
        self.frame = None;
        self.cache.0.flush();
    }
}

fn create_cv_metal_texture(
    cache: &CVMetalTextureCache,
    pixel_buffer: &CVPixelBuffer,
    pixel_format: usize,
    width: usize,
    height: usize,
    plane: usize,
) -> Result<SendCvMetalTexture, String> {
    let mut texture = ptr::null_mut();
    let status = unsafe {
        raw::CVMetalTextureCacheCreateTextureFromImage(
            raw::kCFAllocatorDefault,
            cache.as_ptr().cast(),
            pixel_buffer.as_ptr().cast(),
            ptr::null(),
            pixel_format,
            width,
            height,
            plane,
            &mut texture,
        )
    };
    if status != 0 || texture.is_null() {
        return Err(format!(
            "Unable to map NV12 plane {plane} to Metal (status {status})"
        ));
    }
    let owned = CFType::from_raw(texture.cast())
        .ok_or_else(|| format!("CoreVideo returned no texture for NV12 plane {plane}"))?;
    Ok(SendCvMetalTexture(owned))
}

fn import_metal_texture(
    device: &wgpu::Device,
    cv_texture: &CFType,
    format: wgpu::TextureFormat,
    width: usize,
    height: usize,
    label: &'static str,
) -> Result<wgpu::Texture, String> {
    let raw_texture = unsafe { raw::CVMetalTextureGetTexture(cv_texture.as_ptr().cast()) };
    if raw_texture.is_null() {
        return Err(format!("{label} has no underlying Metal texture"));
    }
    let retained = unsafe {
        Retained::<ProtocolObject<dyn MTLTexture>>::retain(
            raw_texture.cast::<ProtocolObject<dyn MTLTexture>>(),
        )
        .ok_or_else(|| format!("Unable to retain {label}"))?
    };
    let hal_texture = unsafe {
        wgpu::hal::metal::Device::texture_from_raw(
            retained,
            format,
            MTLTextureType::Type2D,
            1,
            1,
            wgpu::hal::CopyExtent {
                width: width as u32,
                height: height as u32,
                depth: 1,
            },
        )
    };
    let descriptor = wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: width as u32,
            height: height as u32,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    };
    Ok(
        unsafe {
            device.create_texture_from_hal::<wgpu::hal::metal::Api>(hal_texture, &descriptor)
        },
    )
}

#[derive(Clone)]
pub(super) struct MacVideoPaintCallback;

impl CallbackTrait for MacVideoPaintCallback {
    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        callback_resources: &CallbackResources,
    ) {
        let Some(renderer) = callback_resources.get::<MacVideoRenderer>() else {
            return;
        };
        let Some(frame) = &renderer.frame else {
            return;
        };
        render_pass.set_pipeline(&renderer.pipeline);
        render_pass.set_bind_group(0, &frame.bind_group, &[]);
        render_pass.draw(0..3, 0..1);
        if !frame.presented.swap(true, Ordering::Relaxed) {
            renderer.presented_frames.fetch_add(1, Ordering::Relaxed);
            renderer.present_latency_us.fetch_add(
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

pub(super) fn paint_callback(rect: egui::Rect) -> egui::PaintCallback {
    egui_wgpu::Callback::new_paint_callback(rect, MacVideoPaintCallback)
}
