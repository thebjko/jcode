mod workspace;

use anyhow::{Context, Result};
use bytemuck::{Pod, Zeroable};
use std::collections::BTreeMap;
use wgpu::util::DeviceExt;
use wgpu::{CompositeAlphaMode, PresentMode, SurfaceError, TextureUsages};
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{ElementState, Event, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowBuilder};
use workspace::{InputMode, KeyInput, KeyOutcome, Workspace};

const DEFAULT_WINDOW_WIDTH: f64 = 1280.0;
const DEFAULT_WINDOW_HEIGHT: f64 = 800.0;
const OUTER_PADDING: f32 = 28.0;
const GAP: f32 = 16.0;
const STATUS_BAR_HEIGHT: f32 = 42.0;
const HEADER_HEIGHT: f32 = 28.0;
const FOCUSED_BORDER_WIDTH: f32 = 2.0;
const UNFOCUSED_BORDER_WIDTH: f32 = 1.0;

const CLEAR_COLOR: wgpu::Color = wgpu::Color {
    r: 0.955,
    g: 0.965,
    b: 0.985,
    a: 1.0,
};

const SURFACE_COLORS: [[f32; 4]; 8] = [
    [0.875, 0.925, 1.000, 1.0],
    [0.900, 0.965, 0.925, 1.0],
    [0.980, 0.925, 0.985, 1.0],
    [1.000, 0.950, 0.875, 1.0],
    [0.930, 0.930, 0.980, 1.0],
    [0.900, 0.965, 0.965, 1.0],
    [0.985, 0.930, 0.930, 1.0],
    [0.930, 0.970, 0.900, 1.0],
];

const BACKGROUND_TOP_LEFT: [f32; 4] = [0.933, 0.957, 1.000, 1.0];
const BACKGROUND_TOP_RIGHT: [f32; 4] = [0.969, 0.949, 1.000, 1.0];
const BACKGROUND_BOTTOM_RIGHT: [f32; 4] = [0.953, 0.980, 0.969, 1.0];
const BACKGROUND_BOTTOM_LEFT: [f32; 4] = [0.961, 0.973, 0.984, 1.0];
const FOCUS_RING_COLOR: [f32; 4] = [0.420, 0.447, 0.502, 1.0];
const UNFOCUSED_BORDER_COLOR: [f32; 4] = [0.730, 0.760, 0.815, 0.72];
const NAV_STATUS_COLOR: [f32; 4] = [0.184, 0.204, 0.251, 1.0];
const INSERT_STATUS_COLOR: [f32; 4] = [0.310, 0.435, 0.376, 1.0];

const SHADER: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(@location(0) position: vec2<f32>, @location(1) color: vec4<f32>) -> VertexOutput {
    var out: VertexOutput;
    out.position = vec4<f32>(position, 0.0, 1.0);
    out.color = color;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

fn main() -> Result<()> {
    pollster::block_on(run())
}

async fn run() -> Result<()> {
    let fullscreen = std::env::args().any(|arg| arg == "--fullscreen");
    let event_loop = EventLoop::new().context("failed to create event loop")?;
    let mut window_builder = WindowBuilder::new()
        .with_title("Jcode Desktop")
        .with_inner_size(LogicalSize::new(
            DEFAULT_WINDOW_WIDTH,
            DEFAULT_WINDOW_HEIGHT,
        ));

    if fullscreen {
        window_builder = window_builder.with_fullscreen(Some(Fullscreen::Borderless(None)));
    }

    let window: &'static Window = Box::leak(Box::new(
        window_builder
            .build(&event_loop)
            .context("failed to create desktop window")?,
    ));

    let mut workspace = Workspace::fake();
    window.set_title(&workspace.status_title());
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
                    if event.state == ElementState::Pressed =>
                {
                    match workspace.handle_key(to_key_input(&event.logical_key)) {
                        KeyOutcome::Exit => target.exit(),
                        KeyOutcome::Redraw => {
                            window.set_title(&workspace.status_title());
                            window.request_redraw();
                        }
                        KeyOutcome::None => {}
                    }
                }
                WindowEvent::RedrawRequested => match canvas.render(&workspace) {
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

fn to_key_input(key: &Key) -> KeyInput {
    match key {
        Key::Named(NamedKey::Escape) => KeyInput::Escape,
        Key::Named(NamedKey::Enter) => KeyInput::Enter,
        Key::Named(NamedKey::Backspace) => KeyInput::Backspace,
        Key::Character(text) => KeyInput::Character(text.to_string()),
        _ => KeyInput::Other,
    }
}

struct Canvas<'window> {
    surface: wgpu::Surface<'window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    render_pipeline: wgpu::RenderPipeline,
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

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("jcode-desktop-primitive-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("jcode-desktop-primitive-pipeline-layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });
        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("jcode-desktop-primitive-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[Vertex::layout()],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        });

        Ok(Self {
            surface,
            device,
            queue,
            config,
            render_pipeline,
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

    fn render(&mut self, workspace: &Workspace) -> std::result::Result<(), SurfaceError> {
        let frame = self.surface.get_current_texture()?;
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("jcode-desktop-render-workspace"),
            });
        let vertices = build_vertices(workspace, self.size);
        let vertex_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("jcode-desktop-workspace-vertices"),
                contents: bytemuck::cast_slice(&vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("jcode-desktop-workspace-pass"),
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
            render_pass.set_pipeline(&self.render_pipeline);
            render_pass.set_vertex_buffer(0, vertex_buffer.slice(..));
            render_pass.draw(0..vertices.len() as u32, 0..1);
        }

        self.queue.submit(Some(encoder.finish()));
        frame.present();
        Ok(())
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct Vertex {
    position: [f32; 2],
    color: [f32; 4],
}

impl Vertex {
    fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                wgpu::VertexAttribute {
                    offset: std::mem::size_of::<[f32; 2]>() as wgpu::BufferAddress,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x4,
                },
            ],
        }
    }
}

#[derive(Clone, Copy)]
struct Rect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

fn build_vertices(workspace: &Workspace, size: PhysicalSize<u32>) -> Vec<Vertex> {
    let width = size.width as f32;
    let height = size.height as f32;
    let mut vertices = Vec::new();

    push_gradient_rect(
        &mut vertices,
        Rect {
            x: 0.0,
            y: 0.0,
            width,
            height,
        },
        BACKGROUND_TOP_LEFT,
        BACKGROUND_BOTTOM_LEFT,
        BACKGROUND_BOTTOM_RIGHT,
        BACKGROUND_TOP_RIGHT,
        size,
    );

    let status_color = match workspace.mode {
        InputMode::Navigation => NAV_STATUS_COLOR,
        InputMode::Insert => INSERT_STATUS_COLOR,
    };
    push_rect(
        &mut vertices,
        Rect {
            x: 0.0,
            y: height - STATUS_BAR_HEIGHT,
            width,
            height: STATUS_BAR_HEIGHT,
        },
        status_color,
        size,
    );

    if workspace.zoomed {
        if let Some(surface) = workspace.focused_surface() {
            let rect = Rect {
                x: OUTER_PADDING,
                y: OUTER_PADDING,
                width: (width - OUTER_PADDING * 2.0).max(1.0),
                height: (height - STATUS_BAR_HEIGHT - OUTER_PADDING * 2.0).max(1.0),
            };
            push_surface(&mut vertices, rect, surface.color_index, true, size);
        }
        return vertices;
    }

    let lanes = index_by(workspace.surfaces.iter().map(|surface| surface.lane));
    let columns = index_by(workspace.surfaces.iter().map(|surface| surface.column));
    let lane_count = lanes.len().max(1) as f32;
    let column_count = columns.len().max(1) as f32;
    let workspace_height = (height - STATUS_BAR_HEIGHT - OUTER_PADDING * 2.0).max(1.0);
    let workspace_width = (width - OUTER_PADDING * 2.0).max(1.0);
    let cell_width = ((workspace_width - GAP * (column_count - 1.0)) / column_count).max(24.0);
    let cell_height = ((workspace_height - GAP * (lane_count - 1.0)) / lane_count).max(24.0);

    for surface in &workspace.surfaces {
        let column = columns.get(&surface.column).copied().unwrap_or_default() as f32;
        let lane = lanes.get(&surface.lane).copied().unwrap_or_default() as f32;
        let rect = Rect {
            x: OUTER_PADDING + column * (cell_width + GAP),
            y: OUTER_PADDING + lane * (cell_height + GAP),
            width: cell_width,
            height: cell_height,
        };
        push_surface(
            &mut vertices,
            rect,
            surface.color_index,
            workspace.is_focused(surface.id),
            size,
        );
    }

    vertices
}

fn index_by(values: impl Iterator<Item = i32>) -> BTreeMap<i32, usize> {
    let mut map = BTreeMap::new();
    for value in values {
        if !map.contains_key(&value) {
            let index = map.len();
            map.insert(value, index);
        }
    }
    map
}

fn push_surface(
    vertices: &mut Vec<Vertex>,
    rect: Rect,
    color_index: usize,
    focused: bool,
    size: PhysicalSize<u32>,
) {
    let fill = SURFACE_COLORS[color_index % SURFACE_COLORS.len()];
    let header = darken(fill, 0.86);
    let border = if focused {
        FOCUS_RING_COLOR
    } else {
        UNFOCUSED_BORDER_COLOR
    };

    push_rect(vertices, rect, border, size);
    let inset = if focused {
        FOCUSED_BORDER_WIDTH
    } else {
        UNFOCUSED_BORDER_WIDTH
    };
    let inner = inset_rect(rect, inset);
    push_rect(vertices, inner, fill, size);
    push_rect(
        vertices,
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: HEADER_HEIGHT.min(inner.height),
        },
        header,
        size,
    );
}

fn inset_rect(rect: Rect, amount: f32) -> Rect {
    Rect {
        x: rect.x + amount,
        y: rect.y + amount,
        width: (rect.width - amount * 2.0).max(1.0),
        height: (rect.height - amount * 2.0).max(1.0),
    }
}

fn darken(color: [f32; 4], factor: f32) -> [f32; 4] {
    [
        color[0] * factor,
        color[1] * factor,
        color[2] * factor,
        color[3],
    ]
}

fn push_rect(vertices: &mut Vec<Vertex>, rect: Rect, color: [f32; 4], size: PhysicalSize<u32>) {
    push_gradient_rect(vertices, rect, color, color, color, color, size);
}

fn push_gradient_rect(
    vertices: &mut Vec<Vertex>,
    rect: Rect,
    top_left_color: [f32; 4],
    bottom_left_color: [f32; 4],
    bottom_right_color: [f32; 4],
    top_right_color: [f32; 4],
    size: PhysicalSize<u32>,
) {
    let width = size.width.max(1) as f32;
    let height = size.height.max(1) as f32;
    let left = rect.x / width * 2.0 - 1.0;
    let right = (rect.x + rect.width) / width * 2.0 - 1.0;
    let top = 1.0 - rect.y / height * 2.0;
    let bottom = 1.0 - (rect.y + rect.height) / height * 2.0;

    vertices.extend_from_slice(&[
        Vertex {
            position: [left, top],
            color: top_left_color,
        },
        Vertex {
            position: [left, bottom],
            color: bottom_left_color,
        },
        Vertex {
            position: [right, bottom],
            color: bottom_right_color,
        },
        Vertex {
            position: [left, top],
            color: top_left_color,
        },
        Vertex {
            position: [right, bottom],
            color: bottom_right_color,
        },
        Vertex {
            position: [right, top],
            color: top_right_color,
        },
    ]);
}

fn non_zero_size(size: PhysicalSize<u32>) -> PhysicalSize<u32> {
    PhysicalSize::new(size.width.max(1), size.height.max(1))
}
