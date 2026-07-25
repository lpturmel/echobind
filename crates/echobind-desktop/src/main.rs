mod app;
mod session;
mod video;
#[cfg(target_os = "macos")]
mod video_renderer_macos;

fn main() -> eframe::Result {
    tracing_subscriber::fmt::init();

    let wgpu_options =
        eframe::WgpuConfiguration::default().with_surface_config(eframe::SurfaceConfig {
            present_mode: eframe::wgpu::PresentMode::AutoNoVsync,
            desired_maximum_frame_latency: Some(1),
        });
    #[cfg(target_os = "windows")]
    let wgpu_options = {
        let mut wgpu_options = wgpu_options;
        // Keep the host process on Microsoft's graphics stack. The capture and
        // encoder already use D3D11/NVENC; allowing wgpu to prefer Vulkan adds
        // a second NVIDIA user-mode driver stack for a UI that needs no Vulkan
        // functionality and complicates device-removal diagnosis.
        if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut wgpu_options.wgpu_setup {
            setup.instance_descriptor.backends = eframe::wgpu::Backends::DX12;
        }
        wgpu_options
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 760.0])
            .with_min_inner_size([720.0, 520.0]),
        wgpu_options,
        ..Default::default()
    };

    eframe::run_native(
        "Echobind",
        options,
        Box::new(|creation_context| Ok(Box::new(app::EchobindApp::new(creation_context)))),
    )
}
