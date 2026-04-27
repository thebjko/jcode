mod workspace;

use anyhow::{Context, Result};
use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;
use wgpu::{CompositeAlphaMode, PresentMode, SurfaceError, TextureUsages};
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{ElementState, Event, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Fullscreen, Window, WindowBuilder};
use workspace::{InputMode, KeyInput, KeyOutcome, PanelSizePreset, Workspace};

const DEFAULT_WINDOW_WIDTH: f64 = 1280.0;
const DEFAULT_WINDOW_HEIGHT: f64 = 800.0;
const OUTER_PADDING: f32 = 8.0;
const GAP: f32 = 6.0;
const STATUS_BAR_HEIGHT: f32 = 30.0;
const FOCUSED_BORDER_WIDTH: f32 = 2.0;
const UNFOCUSED_BORDER_WIDTH: f32 = 1.0;
const PANEL_RADIUS: f32 = 8.0;
const STATUS_RADIUS: f32 = 7.0;
const ROUNDED_CORNER_SEGMENTS: usize = 6;
const PANEL_FIT_TOLERANCE: f32 = 0.15;
const MINIMAP_WIDTH: f32 = 170.0;
const MINIMAP_HEIGHT: f32 = 96.0;
const MINIMAP_RADIUS: f32 = 7.0;
const MINIMAP_INSET: f32 = 9.0;
const MINIMAP_LANE_RADIUS: i32 = 2;
const MINIMAP_ROW_GAP: f32 = 4.0;
const MINIMAP_COLUMN_GAP: f32 = 2.0;

const CLEAR_COLOR: wgpu::Color = wgpu::Color {
    r: 0.955,
    g: 0.965,
    b: 0.985,
    a: 1.0,
};

const BACKGROUND_TOP_LEFT: [f32; 4] = [0.902, 0.937, 1.000, 1.0];
const BACKGROUND_TOP_RIGHT: [f32; 4] = [0.957, 0.925, 1.000, 1.0];
const BACKGROUND_BOTTOM_RIGHT: [f32; 4] = [0.918, 0.984, 0.953, 1.0];
const BACKGROUND_BOTTOM_LEFT: [f32; 4] = [0.953, 0.965, 0.984, 1.0];
const GLASS_PANEL_FILL: [f32; 4] = [0.110, 0.125, 0.165, 0.10];
const FOCUS_RING_COLOR: [f32; 4] = [0.255, 0.275, 0.315, 0.75];
const UNFOCUSED_BORDER_COLOR: [f32; 4] = [0.235, 0.260, 0.305, 0.40];
const NAV_STATUS_COLOR: [f32; 4] = [0.184, 0.204, 0.251, 1.0];
const INSERT_STATUS_COLOR: [f32; 4] = [0.310, 0.435, 0.376, 1.0];
const MINIMAP_BORDER_COLOR: [f32; 4] = [0.235, 0.260, 0.305, 0.45];
const MINIMAP_FILL_COLOR: [f32; 4] = [0.110, 0.125, 0.165, 0.16];
const MINIMAP_ACTIVE_ROW_COLOR: [f32; 4] = [0.184, 0.204, 0.251, 0.30];
const MINIMAP_INACTIVE_ROW_COLOR: [f32; 4] = [0.235, 0.260, 0.305, 0.12];
const MINIMAP_SURFACE_COLOR: [f32; 4] = [0.110, 0.125, 0.165, 0.38];
const MINIMAP_ACTIVE_SURFACE_COLOR: [f32; 4] = [0.110, 0.125, 0.165, 0.58];
const MINIMAP_FOCUSED_SURFACE_COLOR: [f32; 4] = [0.255, 0.275, 0.315, 0.86];
const MINIMAP_VIEWPORT_COLOR: [f32; 4] = [0.255, 0.275, 0.315, 0.72];

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
    let mut modifiers = ModifiersState::empty();

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
                WindowEvent::ModifiersChanged(new_modifiers) => {
                    modifiers = new_modifiers.state();
                }
                WindowEvent::KeyboardInput { event, .. }
                    if event.state == ElementState::Pressed =>
                {
                    match workspace.handle_key(to_key_input(&event.logical_key, modifiers)) {
                        KeyOutcome::Exit => target.exit(),
                        KeyOutcome::Redraw => {
                            window.set_title(&workspace.status_title());
                            window.request_redraw();
                        }
                        KeyOutcome::None => {}
                    }
                }
                WindowEvent::RedrawRequested => match canvas.render(
                    &workspace,
                    window.current_monitor().map(|monitor| monitor.size()),
                ) {
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

fn to_key_input(key: &Key, modifiers: ModifiersState) -> KeyInput {
    match key {
        Key::Named(NamedKey::Escape) => KeyInput::Escape,
        Key::Named(NamedKey::Enter) => KeyInput::Enter,
        Key::Named(NamedKey::Backspace) => KeyInput::Backspace,
        Key::Character(text) if modifiers.control_key() && text == ";" => KeyInput::SpawnPanel,
        Key::Character(text) if modifiers.control_key() && (text == "?" || text == "/") => {
            KeyInput::HotkeyHelp
        }
        Key::Character(text) if modifiers.control_key() && text == "1" => {
            KeyInput::SetPanelSize(PanelSizePreset::Quarter)
        }
        Key::Character(text) if modifiers.control_key() && text == "2" => {
            KeyInput::SetPanelSize(PanelSizePreset::Half)
        }
        Key::Character(text) if modifiers.control_key() && text == "3" => {
            KeyInput::SetPanelSize(PanelSizePreset::ThreeQuarter)
        }
        Key::Character(text) if modifiers.control_key() && text == "4" => {
            KeyInput::SetPanelSize(PanelSizePreset::Full)
        }
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

    fn render(
        &mut self,
        workspace: &Workspace,
        monitor_size: Option<PhysicalSize<u32>>,
    ) -> std::result::Result<(), SurfaceError> {
        let frame = self.surface.get_current_texture()?;
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("jcode-desktop-render-workspace"),
            });
        let vertices = build_vertices(workspace, self.size, monitor_size);
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

#[derive(Clone, Copy)]
struct VisibleColumnLayout {
    visible_columns: u32,
    first_visible_column: i32,
}

fn build_vertices(
    workspace: &Workspace,
    size: PhysicalSize<u32>,
    monitor_size: Option<PhysicalSize<u32>>,
) -> Vec<Vertex> {
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
    push_rounded_rect(
        &mut vertices,
        Rect {
            x: OUTER_PADDING,
            y: OUTER_PADDING,
            width: (width - OUTER_PADDING * 2.0).max(1.0),
            height: STATUS_BAR_HEIGHT,
        },
        STATUS_RADIUS,
        status_color,
        size,
    );

    let active_workspace = workspace.current_workspace();
    let visible_layout = visible_column_layout(
        workspace,
        size.width,
        monitor_size.map(|size| size.width),
        active_workspace,
    );

    if workspace.zoomed {
        if let Some(surface) = workspace.focused_surface() {
            let rect = Rect {
                x: OUTER_PADDING,
                y: STATUS_BAR_HEIGHT + OUTER_PADDING * 2.0,
                width: (width - OUTER_PADDING * 2.0).max(1.0),
                height: (height - STATUS_BAR_HEIGHT - OUTER_PADDING * 3.0).max(1.0),
            };
            push_surface(&mut vertices, rect, surface.color_index, true, size);
        }
        push_minimap(
            &mut vertices,
            workspace,
            active_workspace,
            visible_layout,
            size,
        );
        return vertices;
    }

    let workspace_height = (height - STATUS_BAR_HEIGHT - OUTER_PADDING * 3.0).max(1.0);
    let workspace_width = (width - OUTER_PADDING * 2.0).max(1.0);
    let visible_columns_f = visible_layout.visible_columns as f32;
    let total_gap_width = GAP * (visible_columns_f - 1.0).max(0.0);
    let column_width = ((workspace_width - total_gap_width) / visible_columns_f).max(1.0);
    let scroll_offset = visible_layout.first_visible_column as f32 * (column_width + GAP);

    for surface in workspace
        .surfaces
        .iter()
        .filter(|surface| surface.lane == active_workspace)
    {
        let column = surface.column as f32;
        let rect = Rect {
            x: OUTER_PADDING + column * (column_width + GAP) - scroll_offset,
            y: STATUS_BAR_HEIGHT + OUTER_PADDING * 2.0,
            width: column_width,
            height: workspace_height,
        };
        push_surface(
            &mut vertices,
            rect,
            surface.color_index,
            workspace.is_focused(surface.id),
            size,
        );
    }

    push_minimap(
        &mut vertices,
        workspace,
        active_workspace,
        visible_layout,
        size,
    );

    vertices
}

fn visible_column_layout(
    workspace: &Workspace,
    window_width: u32,
    monitor_width: Option<u32>,
    active_workspace: i32,
) -> VisibleColumnLayout {
    let visible_columns = inferred_visible_column_count(
        window_width,
        monitor_width,
        workspace.preferred_panel_screen_fraction(),
    );
    let focused_column = workspace
        .focused_surface()
        .map(|surface| surface.column)
        .unwrap_or_default();
    let (min_column, max_column) = workspace
        .surfaces
        .iter()
        .filter(|surface| surface.lane == active_workspace)
        .map(|surface| surface.column)
        .fold((focused_column, focused_column), |(min, max), column| {
            (min.min(column), max.max(column))
        });
    let visible_columns_i = visible_columns as i32;
    let max_first_column = (max_column - visible_columns_i + 1).max(min_column);
    let preferred_first_column = focused_column - visible_columns_i / 2;
    let first_visible_column = preferred_first_column.clamp(min_column, max_first_column);

    VisibleColumnLayout {
        visible_columns,
        first_visible_column,
    }
}

fn inferred_visible_column_count(
    window_width: u32,
    monitor_width: Option<u32>,
    preferred_panel_screen_fraction: f32,
) -> u32 {
    let Some(monitor_width) = monitor_width.filter(|width| *width > 0) else {
        return 1;
    };

    let preferred_panel_screen_fraction = preferred_panel_screen_fraction.clamp(0.25, 1.0);
    let target_panel_width = monitor_width as f32 * preferred_panel_screen_fraction;
    ((window_width as f32 / target_panel_width + PANEL_FIT_TOLERANCE).floor() as u32).clamp(1, 4)
}

fn push_minimap(
    vertices: &mut Vec<Vertex>,
    workspace: &Workspace,
    active_workspace: i32,
    visible_layout: VisibleColumnLayout,
    size: PhysicalSize<u32>,
) {
    let screen_width = size.width as f32;
    let screen_height = size.height as f32;
    let width = MINIMAP_WIDTH.min((screen_width - OUTER_PADDING * 2.0).max(1.0));
    let height = MINIMAP_HEIGHT.min(
        (screen_height - STATUS_BAR_HEIGHT - OUTER_PADDING * 3.0)
            .max(1.0)
            .min(MINIMAP_HEIGHT),
    );

    if width < 80.0 || height < 48.0 {
        return;
    }

    let rect = Rect {
        x: (screen_width - OUTER_PADDING - width).max(OUTER_PADDING),
        y: STATUS_BAR_HEIGHT + OUTER_PADDING * 2.0,
        width,
        height,
    };
    push_rounded_rect(vertices, rect, MINIMAP_RADIUS, MINIMAP_BORDER_COLOR, size);
    let content_shell = inset_rect(rect, 1.0);
    push_rounded_rect(
        vertices,
        content_shell,
        (MINIMAP_RADIUS - 1.0).max(1.0),
        MINIMAP_FILL_COLOR,
        size,
    );

    let content = inset_rect(rect, MINIMAP_INSET);
    let lane_count = (MINIMAP_LANE_RADIUS * 2 + 1) as usize;
    let row_height = ((content.height - MINIMAP_ROW_GAP * (lane_count as f32 - 1.0))
        / lane_count as f32)
        .max(2.0);
    let first_lane = active_workspace - MINIMAP_LANE_RADIUS;
    let last_lane = active_workspace + MINIMAP_LANE_RADIUS;
    let viewport_first_column = visible_layout.first_visible_column;
    let viewport_last_column =
        viewport_first_column + visible_layout.visible_columns.saturating_sub(1) as i32;
    let (min_column, max_column) = workspace
        .surfaces
        .iter()
        .filter(|surface| surface.lane >= first_lane && surface.lane <= last_lane)
        .map(|surface| surface.column)
        .fold(
            (viewport_first_column, viewport_last_column),
            |(min, max), column| (min.min(column), max.max(column)),
        );
    let column_count = (max_column - min_column + 1).max(1) as f32;
    let column_width =
        ((content.width - MINIMAP_COLUMN_GAP * (column_count - 1.0)) / column_count).max(1.0);
    let column_pitch = column_width + MINIMAP_COLUMN_GAP;

    for lane in first_lane..=last_lane {
        let row_index = (lane - first_lane) as f32;
        let row = Rect {
            x: content.x,
            y: content.y + row_index * (row_height + MINIMAP_ROW_GAP),
            width: content.width,
            height: row_height,
        };
        let active_row = lane == active_workspace;
        let row_color = if active_row {
            MINIMAP_ACTIVE_ROW_COLOR
        } else {
            MINIMAP_INACTIVE_ROW_COLOR
        };
        push_rounded_rect(vertices, row, 2.5, row_color, size);

        for surface in workspace
            .surfaces
            .iter()
            .filter(|surface| surface.lane == lane)
        {
            let x = content.x + (surface.column - min_column) as f32 * column_pitch;
            let surface_rect = Rect {
                x,
                y: row.y + 2.0,
                width: column_width.min(content.x + content.width - x).max(1.0),
                height: (row.height - 4.0).max(1.0),
            };
            let color = if workspace.is_focused(surface.id) {
                MINIMAP_FOCUSED_SURFACE_COLOR
            } else if active_row {
                MINIMAP_ACTIVE_SURFACE_COLOR
            } else {
                MINIMAP_SURFACE_COLOR
            };
            push_rounded_rect(vertices, surface_rect, 2.0, color, size);
        }

        if active_row {
            let viewport_x = content.x + (viewport_first_column - min_column) as f32 * column_pitch;
            let viewport_width = (visible_layout.visible_columns as f32 * column_width
                + visible_layout.visible_columns.saturating_sub(1) as f32 * MINIMAP_COLUMN_GAP)
                .min(content.x + content.width - viewport_x)
                .max(1.0);
            push_stroked_rect(
                vertices,
                Rect {
                    x: viewport_x,
                    y: row.y,
                    width: viewport_width,
                    height: row.height,
                },
                1.0,
                MINIMAP_VIEWPORT_COLOR,
                size,
            );
        }
    }
}

fn push_surface(
    vertices: &mut Vec<Vertex>,
    rect: Rect,
    _color_index: usize,
    focused: bool,
    size: PhysicalSize<u32>,
) {
    let border = if focused {
        FOCUS_RING_COLOR
    } else {
        UNFOCUSED_BORDER_COLOR
    };

    push_rounded_rect(vertices, rect, PANEL_RADIUS, border, size);
    let inset = if focused {
        FOCUSED_BORDER_WIDTH
    } else {
        UNFOCUSED_BORDER_WIDTH
    };
    let inner = inset_rect(rect, inset);
    push_rounded_rect(
        vertices,
        inner,
        (PANEL_RADIUS - inset).max(1.0),
        GLASS_PANEL_FILL,
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

fn push_rect(vertices: &mut Vec<Vertex>, rect: Rect, color: [f32; 4], size: PhysicalSize<u32>) {
    push_gradient_rect(vertices, rect, color, color, color, color, size);
}

fn push_stroked_rect(
    vertices: &mut Vec<Vertex>,
    rect: Rect,
    stroke_width: f32,
    color: [f32; 4],
    size: PhysicalSize<u32>,
) {
    let stroke_width = stroke_width.max(1.0).min(rect.width).min(rect.height);
    push_rect(
        vertices,
        Rect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: stroke_width,
        },
        color,
        size,
    );
    push_rect(
        vertices,
        Rect {
            x: rect.x,
            y: rect.y + rect.height - stroke_width,
            width: rect.width,
            height: stroke_width,
        },
        color,
        size,
    );
    push_rect(
        vertices,
        Rect {
            x: rect.x,
            y: rect.y,
            width: stroke_width,
            height: rect.height,
        },
        color,
        size,
    );
    push_rect(
        vertices,
        Rect {
            x: rect.x + rect.width - stroke_width,
            y: rect.y,
            width: stroke_width,
            height: rect.height,
        },
        color,
        size,
    );
}

fn push_rounded_rect(
    vertices: &mut Vec<Vertex>,
    rect: Rect,
    radius: f32,
    color: [f32; 4],
    size: PhysicalSize<u32>,
) {
    let radius = radius.max(0.0).min(rect.width / 2.0).min(rect.height / 2.0);
    if radius <= 0.5 {
        push_rect(vertices, rect, color, size);
        return;
    }

    let center = [rect.x + rect.width / 2.0, rect.y + rect.height / 2.0];
    let mut points = Vec::with_capacity((ROUNDED_CORNER_SEGMENTS + 1) * 4);
    append_arc_points(
        &mut points,
        rect.x + rect.width - radius,
        rect.y + radius,
        radius,
        -std::f32::consts::FRAC_PI_2,
        0.0,
    );
    append_arc_points(
        &mut points,
        rect.x + rect.width - radius,
        rect.y + rect.height - radius,
        radius,
        0.0,
        std::f32::consts::FRAC_PI_2,
    );
    append_arc_points(
        &mut points,
        rect.x + radius,
        rect.y + rect.height - radius,
        radius,
        std::f32::consts::FRAC_PI_2,
        std::f32::consts::PI,
    );
    append_arc_points(
        &mut points,
        rect.x + radius,
        rect.y + radius,
        radius,
        std::f32::consts::PI,
        std::f32::consts::PI * 1.5,
    );

    for index in 0..points.len() {
        let next_index = (index + 1) % points.len();
        push_pixel_triangle(
            vertices,
            center,
            points[index],
            points[next_index],
            color,
            size,
        );
    }
}

fn append_arc_points(
    points: &mut Vec<[f32; 2]>,
    center_x: f32,
    center_y: f32,
    radius: f32,
    start_angle: f32,
    end_angle: f32,
) {
    for step in 0..=ROUNDED_CORNER_SEGMENTS {
        let t = step as f32 / ROUNDED_CORNER_SEGMENTS as f32;
        let angle = start_angle + (end_angle - start_angle) * t;
        points.push([
            center_x + radius * angle.cos(),
            center_y + radius * angle.sin(),
        ]);
    }
}

fn push_pixel_triangle(
    vertices: &mut Vec<Vertex>,
    a: [f32; 2],
    b: [f32; 2],
    c: [f32; 2],
    color: [f32; 4],
    size: PhysicalSize<u32>,
) {
    vertices.extend_from_slice(&[
        Vertex {
            position: pixel_to_ndc(a, size),
            color,
        },
        Vertex {
            position: pixel_to_ndc(b, size),
            color,
        },
        Vertex {
            position: pixel_to_ndc(c, size),
            color,
        },
    ]);
}

fn pixel_to_ndc(point: [f32; 2], size: PhysicalSize<u32>) -> [f32; 2] {
    let width = size.width.max(1) as f32;
    let height = size.height.max(1) as f32;
    [point[0] / width * 2.0 - 1.0, 1.0 - point[1] / height * 2.0]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarter_size_preset_follows_quarter_screen_width_steps() {
        let monitor_width = Some(2000);

        assert_eq!(inferred_visible_column_count(500, monitor_width, 0.25), 1);
        assert_eq!(inferred_visible_column_count(1000, monitor_width, 0.25), 2);
        assert_eq!(inferred_visible_column_count(1500, monitor_width, 0.25), 3);
        assert_eq!(inferred_visible_column_count(2000, monitor_width, 0.25), 4);
    }

    #[test]
    fn preferred_panel_size_limits_visible_column_count() {
        let monitor_width = Some(2000);

        assert_eq!(inferred_visible_column_count(2000, monitor_width, 0.25), 4);
        assert_eq!(inferred_visible_column_count(2000, monitor_width, 0.50), 2);
        assert_eq!(inferred_visible_column_count(2000, monitor_width, 0.75), 1);
        assert_eq!(inferred_visible_column_count(2000, monitor_width, 1.00), 1);

        assert_eq!(inferred_visible_column_count(500, monitor_width, 0.25), 1);
        assert_eq!(inferred_visible_column_count(500, monitor_width, 1.00), 1);
    }

    #[test]
    fn visible_column_count_tolerates_window_manager_gaps() {
        let monitor_width = Some(2000);

        assert_eq!(inferred_visible_column_count(1940, monitor_width, 0.25), 4);
        assert_eq!(inferred_visible_column_count(970, monitor_width, 0.25), 2);
        assert_eq!(inferred_visible_column_count(1940, monitor_width, 0.50), 2);
    }

    #[test]
    fn visible_column_count_is_clamped_and_safe_without_monitor() {
        assert_eq!(inferred_visible_column_count(1, Some(2000), 0.25), 1);
        assert_eq!(inferred_visible_column_count(3000, Some(2000), 0.25), 4);
        assert_eq!(inferred_visible_column_count(1000, Some(0), 0.25), 1);
        assert_eq!(inferred_visible_column_count(1000, None, 0.25), 1);
    }
}
