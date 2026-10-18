//! GPU backend, lean path: EGL + GLES 3.0, the wheel is one fragment shader.

use std::ffi::c_void;

use common::wheel::{self, BOX, GAP, R_HUB, R_IN, R_OUT, SEGMENTS};
use common::{Ctx, Frame, Renderer};
use glow::HasContext;
use khronos_egl as egl;
use wayland_client::{Proxy, protocol::wl_surface::WlSurface};
use wayland_egl::WlEglSurface;

const PLATFORM_WAYLAND_KHR: egl::Enum = 0x31D8;

const VS: &str = r#"#version 300 es
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}"#;

const FS: &str = r#"#version 300 es
precision highp float;
uniform vec2 u_center;
uniform int u_hover;
uniform vec4 u_seg, u_hov, u_hub;
uniform vec4 u_geo; // r_out, r_in, r_hub, gap
uniform int u_n;
out vec4 o;
void main() {
    vec2 p = gl_FragCoord.xy - u_center;
    float r = length(p);
    float ring = clamp(u_geo.x - r + 0.5, 0.0, 1.0) * clamp(r - u_geo.y + 0.5, 0.0, 1.0);
    float a = atan(p.y, p.x);
    if (a < 0.0) a += 6.28318530718;
    float seg = 6.28318530718 / float(u_n);
    int idx = int(a / seg);
    float local = a - float(idx) * seg;
    float gap = clamp(min(local, seg - local) * r - u_geo.w + 0.5, 0.0, 1.0);
    vec4 c = (idx == u_hover ? u_hov : u_seg) * (ring * gap);
    float hub = clamp(u_geo.z - r + 0.5, 0.0, 1.0);
    o = c + u_hub * hub;
}"#;

struct Gles {
    egl: egl::Instance<egl::Static>,
    display: egl::Display,
    config: egl::Config,
    context: egl::Context,
    gl: glow::Context,
    program: glow::Program,
    u_hover: glow::UniformLocation,
    u_center: glow::UniformLocation,
    surface: Option<(egl::Surface, WlEglSurface)>,
    size: (u32, u32),
}

impl Renderer for Gles {
    fn init(cx: &Ctx) -> Self {
        let egl = egl::Instance::new(egl::Static);
        // SAFETY: display_ptr is the live wl_display of `cx.conn`, which
        // outlives the renderer.
        let display = unsafe {
            egl.get_platform_display(
                PLATFORM_WAYLAND_KHR,
                cx.conn.backend().display_ptr() as *mut c_void,
                &[egl::ATTRIB_NONE],
            )
        }
        .expect("egl display");
        egl.initialize(display).expect("egl init");
        egl.bind_api(egl::OPENGL_ES_API).unwrap();
        let config = egl
            .choose_first_config(
                display,
                &[
                    egl::RED_SIZE,
                    8,
                    egl::GREEN_SIZE,
                    8,
                    egl::BLUE_SIZE,
                    8,
                    egl::ALPHA_SIZE,
                    8,
                    egl::SURFACE_TYPE,
                    egl::WINDOW_BIT,
                    egl::RENDERABLE_TYPE,
                    egl::OPENGL_ES3_BIT,
                    egl::NONE,
                ],
            )
            .unwrap()
            .expect("egl config");
        let context = egl
            .create_context(
                display,
                config,
                None,
                &[egl::CONTEXT_MAJOR_VERSION, 3, egl::NONE],
            )
            .expect("egl context");
        // Surfaceless current context, so the pipeline is ready before any surface.
        egl.make_current(display, None, None, Some(context))
            .expect("surfaceless");

        // SAFETY: the context is current on this thread.
        let gl = unsafe {
            glow::Context::from_loader_function(|s| {
                egl.get_proc_address(s)
                    .map_or(std::ptr::null(), |p| p as *const c_void)
            })
        };
        // SAFETY: plain GL calls on the current context.
        let (program, u_hover, u_center) = unsafe {
            let program = gl.create_program().unwrap();
            for (ty, src) in [(glow::VERTEX_SHADER, VS), (glow::FRAGMENT_SHADER, FS)] {
                let s = gl.create_shader(ty).unwrap();
                gl.shader_source(s, src);
                gl.compile_shader(s);
                assert!(
                    gl.get_shader_compile_status(s),
                    "{}",
                    gl.get_shader_info_log(s)
                );
                gl.attach_shader(program, s);
            }
            gl.link_program(program);
            assert!(
                gl.get_program_link_status(program),
                "{}",
                gl.get_program_info_log(program)
            );
            gl.use_program(Some(program));
            let u = |n: &str| gl.get_uniform_location(program, n).unwrap();
            gl.uniform_4_f32_slice(Some(&u("u_seg")), &wheel::SEG);
            gl.uniform_4_f32_slice(Some(&u("u_hov")), &wheel::HOVER);
            gl.uniform_4_f32_slice(Some(&u("u_hub")), &wheel::HUB);
            gl.uniform_4_f32(Some(&u("u_geo")), R_OUT, R_IN, R_HUB, GAP);
            gl.uniform_1_i32(Some(&u("u_n")), SEGMENTS as i32);
            let vao = gl.create_vertex_array().unwrap();
            gl.bind_vertex_array(Some(vao));
            gl.enable(glow::SCISSOR_TEST);
            (program, u("u_hover"), u("u_center"))
        };

        Gles {
            egl,
            display,
            config,
            context,
            gl,
            program,
            u_hover,
            u_center,
            surface: None,
            size: (0, 0),
        }
    }

    fn attach(&mut self, _: &Ctx, surface: &WlSurface, width: u32, height: u32) {
        self.size = (width, height);
        let win = WlEglSurface::new(surface.id(), width as i32, height as i32).unwrap();
        // SAFETY: `win` is kept alive alongside the EGL surface and dropped after it.
        let s = unsafe {
            self.egl.create_window_surface(
                self.display,
                self.config,
                win.ptr() as *mut c_void,
                None,
            )
        }
        .expect("egl surface");
        self.egl
            .make_current(self.display, Some(s), Some(s), Some(self.context))
            .unwrap();
        // We pace with frame callbacks ourselves; don't block in swap.
        self.egl.swap_interval(self.display, 0).unwrap();
        // SAFETY: context current; first frame must clear the whole surface.
        unsafe {
            self.gl.viewport(0, 0, width as i32, height as i32);
            self.gl.scissor(0, 0, width as i32, height as i32);
            self.gl.clear_color(0.0, 0.0, 0.0, 0.0);
        }
        self.surface = Some((s, win));
    }

    fn draw(&mut self, _: &WlSurface, f: &Frame) {
        let (s, _) = self.surface.as_ref().unwrap();
        let (ox, oy) = wheel::origin(f.width, f.height);
        let c = BOX as f32 / 2.0;
        // SAFETY: context current with the surface bound in attach().
        unsafe {
            let gl = &self.gl;
            gl.scissor(0, 0, f.width as i32, f.height as i32);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.scissor(ox as i32, oy as i32, BOX as i32, BOX as i32);
            gl.uniform_2_f32(Some(&self.u_center), ox as f32 + c, oy as f32 + c);
            gl.uniform_1_i32(Some(&self.u_hover), f.hover as i32);
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
        }
        self.egl.swap_buffers(self.display, *s).unwrap();
    }

    fn detach(&mut self) {
        if let Some((s, win)) = self.surface.take() {
            self.egl
                .make_current(self.display, None, None, Some(self.context))
                .unwrap();
            self.egl.destroy_surface(self.display, s).unwrap();
            drop(win);
        }
    }
}

impl Drop for Gles {
    fn drop(&mut self) {
        self.detach();
        // SAFETY: context current; program no longer used.
        unsafe { self.gl.delete_program(self.program) };
        self.egl.make_current(self.display, None, None, None).ok();
        self.egl.destroy_context(self.display, self.context).ok();
        self.egl.terminate(self.display).ok();
    }
}

fn main() {
    common::run::<Gles>("gles");
}
