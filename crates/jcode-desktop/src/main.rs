use anyhow::{Context, Result};
use wgpu::{CompositeAlphaMode, PresentMode, SurfaceError, TextureUsages};
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, Event, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowBuilder};

const CLEAR_COLOR: wgpu::Color = wgpu::Color {
    r: 1.0,
    g: 1.0,
    b: 1.0,
    a: 1.0,
};

fn main() -> Result<()> {
    pollster::block_on(run())
}

async fn run() -> Result<()> {
    let event_loop = EventLoop::new().context("failed to create event loop")?;
    let window: &'static Window = Box::leak(Box::new(
        WindowBuilder::new()
            .with_title("Jcode Desktop")
            .with_fullscreen(Some(Fullscreen::Borderless(None)))
            .build(&event_loop)
            .context("failed to create desktop window")?,
    ));

    let mut canvas = Canvas::new(window).await?;

    event_loop.run(move |event, target| {
        target.set_control_flow(ControlFlow::Wait);

        match event {
            Event::WindowEvent { event, window_id } if window_id == window.id() => match event {
                WindowEvent::CloseRequested => target.exit(),
                WindowEvent::Resized(size) => {
                    canvas.resize(size);
                    window.request_redraw();
                }
                WindowEvent::ScaleFactorChanged { .. } => {
                    canvas.resize(window.inner_size());
                    window.request_redraw();
                }
                WindowEvent::KeyboardInput { event, .. }
                    if event.state == ElementState::Pressed
                        && matches!(event.logical_key, Key::Named(NamedKey::Escape)) =>
                {
                    target.exit();
                }
                WindowEvent::RedrawRequested => match canvas.render() {
                    Ok(()) => {}
                    Err(SurfaceError::Lost | SurfaceError::Outdated) => {
                        canvas.resize(window.inner_size());
                        window.request_redraw();
                    }
                    Err(SurfaceError::OutOfMemory) => target.exit(),
                    Err(SurfaceError::Timeout) => {
                        window.request_redraw();
                    }
                },
                _ => {}
            },
            Event::AboutToWait => {
                if canvas.needs_initial_frame {
                    canvas.needs_initial_frame = false;
                    window.request_redraw();
                }
            }
            _ => {}
        }
    })?;

    Ok(())
}

struct Canvas<'window> {
    surface: wgpu::Surface<'window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    size: PhysicalSize<u32>,
    needs_initial_frame: bool,
}

impl<'window> Canvas<'window> {
    async fn new(window: &'window Window) -> Result<Self> {
        let size = non_zero_size(window.inner_size());
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });
        let surface = instance
            .create_surface(window)
            .context("failed to create wgpu surface")?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .context("failed to find a compatible GPU adapter")?;
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("jcode-desktop-device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                },
                None,
            )
            .await
            .context("failed to create wgpu device")?;
        let capabilities = surface.get_capabilities(&adapter);
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(|format| format.is_srgb())
            .unwrap_or(capabilities.formats[0]);
        let present_mode = if capabilities.present_modes.contains(&PresentMode::Fifo) {
            PresentMode::Fifo
        } else {
            capabilities.present_modes[0]
        };
        let alpha_mode = if capabilities
            .alpha_modes
            .contains(&CompositeAlphaMode::Opaque)
        {
            CompositeAlphaMode::Opaque
        } else {
            capabilities.alpha_modes[0]
        };
        let config = wgpu::SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width,
            height: size.height,
            present_mode,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        Ok(Self {
            surface,
            device,
            queue,
            config,
            size,
            needs_initial_frame: true,
        })
    }

    fn resize(&mut self, size: PhysicalSize<u32>) {
        let size = non_zero_size(size);
        if self.size == size {
            return;
        }

        self.size = size;
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
    }

    fn render(&mut self) -> std::result::Result<(), SurfaceError> {
        let frame = self.surface.get_current_texture()?;
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("jcode-desktop-clear-white"),
            });

        {
            let _render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("jcode-desktop-white-canvas-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(CLEAR_COLOR),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
        }

        self.queue.submit(Some(encoder.finish()));
        frame.present();
        Ok(())
    }
}

fn non_zero_size(size: PhysicalSize<u32>) -> PhysicalSize<u32> {
    PhysicalSize::new(size.width.max(1), size.height.max(1))
}
