//! GPU backend, "standard" path: wgpu on Vulkan, same shader as the GLES spike.

use std::ptr::NonNull;

use common::wheel::{self, BOX, GAP, R_HUB, R_IN, R_OUT, SEGMENTS};
use common::{Ctx, Frame, Renderer};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use wayland_client::{Proxy, protocol::wl_surface::WlSurface};

const SHADER: &str = r#"
struct P {
    center: vec2<f32>,
    hover: u32,
    n: u32,
    geo: vec4<f32>, // r_out, r_in, r_hub, gap
    seg: vec4<f32>,
    hov: vec4<f32>,
    hub: vec4<f32>,
};
@group(0) @binding(0) var<uniform> u: P;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let p = pos.xy - u.center;
    let r = length(p);
    let ring = clamp(u.geo.x - r + 0.5, 0.0, 1.0) * clamp(r - u.geo.y + 0.5, 0.0, 1.0);
    var a = atan2(p.y, p.x);
    if (a < 0.0) { a += 6.28318530718; }
    let seg = 6.28318530718 / f32(u.n);
    let idx = u32(a / seg);
    let local = a - f32(idx) * seg;
    let gap = clamp(min(local, seg - local) * r - u.geo.w + 0.5, 0.0, 1.0);
    let c = select(u.seg, u.hov, idx == u.hover) * (ring * gap);
    let hub = clamp(u.geo.z - r + 0.5, 0.0, 1.0);
    return c + u.hub * hub;
}
"#;

struct Wgpu {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    module: wgpu::ShaderModule,
    layout: wgpu::PipelineLayout,
    bind_group: wgpu::BindGroup,
    uniforms: wgpu::Buffer,
    pipeline: Option<(wgpu::TextureFormat, wgpu::RenderPipeline)>,
    surface: Option<(wgpu::Surface<'static>, wgpu::SurfaceConfiguration)>,
    display: RawDisplayHandle,
}

fn params(center: [f32; 2], hover: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(80);
    for f in center {
        b.extend(f.to_le_bytes());
    }
    b.extend(hover.to_le_bytes());
    b.extend(SEGMENTS.to_le_bytes());
    for v in [
        [R_OUT, R_IN, R_HUB, GAP],
        wheel::SEG,
        wheel::HOVER,
        wheel::HUB,
    ] {
        for f in v {
            b.extend(f.to_le_bytes());
        }
    }
    b
}

impl Renderer for Wgpu {
    fn init(cx: &Ctx) -> Self {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            apply_limit_buckets: false,
        }))
        .expect("adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            required_limits: wgpu::Limits {
                max_texture_dimension_2d: 8192,
                ..wgpu::Limits::downlevel_defaults()
            },
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            ..Default::default()
        }))
        .expect("device");
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: None,
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 80,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniforms.as_entire_binding(),
            }],
        });
        let display = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            NonNull::new(cx.conn.backend().display_ptr() as *mut _).unwrap(),
        ));
        println!("M wgpu adapter={:?}", adapter.get_info().name);
        Wgpu {
            instance,
            adapter,
            device,
            queue,
            module,
            layout,
            bind_group,
            uniforms,
            pipeline: None,
            surface: None,
            display,
        }
    }

    fn attach(&mut self, _: &Ctx, surface: &WlSurface, width: u32, height: u32) {
        let window = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(surface.id().as_ptr() as *mut _).unwrap(),
        ));
        // SAFETY: display and wl_surface outlive the wgpu surface; detach()
        // drops it before the layer surface is destroyed.
        let s = unsafe {
            self.instance
                .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: Some(self.display),
                    raw_window_handle: window,
                })
        }
        .expect("surface");
        let caps = s.get_capabilities(&self.adapter);
        let mut cfg = s
            .get_default_config(&self.adapter, width, height)
            .expect("config");
        if caps.formats.contains(&wgpu::TextureFormat::Bgra8Unorm) {
            cfg.format = wgpu::TextureFormat::Bgra8Unorm;
        }
        cfg.alpha_mode = if caps
            .alpha_modes
            .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
        {
            wgpu::CompositeAlphaMode::PreMultiplied
        } else {
            caps.alpha_modes[0]
        };
        cfg.present_mode = [wgpu::PresentMode::Mailbox, wgpu::PresentMode::Immediate]
            .into_iter()
            .find(|m| caps.present_modes.contains(m))
            .unwrap_or(wgpu::PresentMode::Fifo);
        cfg.desired_maximum_frame_latency = 1;
        s.configure(&self.device, &cfg);

        if self.pipeline.as_ref().is_none_or(|(f, _)| *f != cfg.format) {
            println!(
                "M wgpu format={:?} alpha={:?} present={:?}",
                cfg.format, cfg.alpha_mode, cfg.present_mode
            );
            let p = self
                .device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: None,
                    layout: Some(&self.layout),
                    vertex: wgpu::VertexState {
                        module: &self.module,
                        entry_point: Some("vs"),
                        compilation_options: Default::default(),
                        buffers: &[],
                    },
                    primitive: Default::default(),
                    depth_stencil: None,
                    multisample: Default::default(),
                    fragment: Some(wgpu::FragmentState {
                        module: &self.module,
                        entry_point: Some("fs"),
                        compilation_options: Default::default(),
                        targets: &[Some(cfg.format.into())],
                    }),
                    multiview_mask: None,
                    cache: None,
                });
            self.pipeline = Some((cfg.format, p));
        }
        self.surface = Some((s, cfg));
    }

    fn draw(&mut self, _: &WlSurface, f: &Frame) {
        let (s, cfg) = self.surface.as_ref().unwrap();
        let mut acquired = s.get_current_texture();
        for _ in 0..3 {
            if !matches!(acquired, wgpu::CurrentSurfaceTexture::Outdated) {
                break;
            }
            println!("M wgpu outdated_reconfigure");
            s.configure(&self.device, cfg);
            acquired = s.get_current_texture();
        }
        let tex = match acquired {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            other => panic!("surface texture unavailable: {other:?}"),
        };
        let view = tex.texture.create_view(&Default::default());
        let (ox, oy) = wheel::origin(f.width, f.height);
        let c = BOX as f32 / 2.0;
        self.queue.write_buffer(
            &self.uniforms,
            0,
            &params([ox as f32 + c, oy as f32 + c], f.hover),
        );
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline.as_ref().unwrap().1);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_scissor_rect(ox, oy, BOX.min(f.width), BOX.min(f.height));
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([enc.finish()]);
        self.queue.present(tex);
    }

    fn detach(&mut self) {
        self.surface = None;
    }
}

fn main() {
    common::run::<Wgpu>("wgpu");
}
