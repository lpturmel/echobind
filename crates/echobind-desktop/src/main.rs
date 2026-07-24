mod app;
mod session;
mod video;

fn main() -> eframe::Result {
    tracing_subscriber::fmt::init();

    let wgpu_options =
        eframe::WgpuConfiguration::default().with_surface_config(eframe::SurfaceConfig {
            present_mode: eframe::wgpu::PresentMode::AutoNoVsync,
            desired_maximum_frame_latency: Some(1),
        });
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
