//! GPU renderer.
//!
//! Frame structure:
//!   1. (at most `bg_fps` times a second) render the animated background into
//!      a low-resolution float texture and blur a copy of it for frosted panels
//!   2. blit the cached background to the screen with vignette + dither
//!   3. per layer: instanced quads, then text
//!
//! Text is only re-prepared when the draw list changes, so frames where just
//! the background moves cost one blit and a replay.

use crate::scene::{Quad, Scene, LAYERS};
use crate::text::TextSystem;
use crate::theme::BgSpec;
use crate::util::Rgba;
use bytemuck::{Pod, Zeroable};
use glyphon::{Cache, ColorMode, Resolution, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport};
use std::hash::{Hash, Hasher};
use wgpu::util::DeviceExt;

pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

const BG_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Default, Debug, PartialEq)]
pub struct SceneRaw {
    /// intensity, time, scale, kind
    pub a: [f32; 4],
    pub k: [f32; 4],
    pub c: [[f32; 4]; 4],
    pub base: [f32; 4],
}

impl SceneRaw {
    pub fn new(spec: &BgSpec, base: Rgba, time: f32) -> SceneRaw {
        SceneRaw {
            a: [spec.intensity, time, spec.scale.max(0.05), spec.kind.id() as f32],
            k: spec.knobs,
            c: [spec.colors[0].0, spec.colors[1].0, spec.colors[2].0, spec.colors[3].0],
            base: base.0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Default)]
struct BgRaw {
    res: [f32; 4],
    sa: SceneRaw,
    sb: SceneRaw,
}

/// What the background should look like this frame.
#[derive(Clone, Copy, Default)]
pub struct BgFrame {
    pub a: SceneRaw,
    pub b: SceneRaw,
    /// 0 = only `a`, 1 = only `b`.
    pub fade: f32,
    /// Internal resolution as a fraction of the window.
    pub scale: f32,
    /// Re-render the background this frame.
    pub render: bool,
    pub vignette: f32,
    pub time: f32,
}

struct Target {
    view: wgpu::TextureView,
    w: u32,
    h: u32,
}

fn make_target(dev: &wgpu::Device, w: u32, h: u32, label: &str) -> Target {
    let tex = dev.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: w.max(1), height: h.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: BG_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    Target { view: tex.create_view(&wgpu::TextureViewDescriptor::default()), w: w.max(1), h: h.max(1) }
}

pub struct Renderer {
    format: wgpu::TextureFormat,
    size: (u32, u32),

    sampler: wgpu::Sampler,
    bg_pipeline: wgpu::RenderPipeline,
    bg_ubuf: wgpu::Buffer,
    bg_bind: wgpu::BindGroup,
    bg_scale: f32,
    bg_tex: Target,
    blur_a: Target,
    blur_b: Target,
    have_bg: bool,

    post_layout: wgpu::BindGroupLayout,
    blur_pipeline: wgpu::RenderPipeline,
    blit_pipeline: wgpu::RenderPipeline,
    post_ubufs: [wgpu::Buffer; 4],
    /// downsample, blur-h, blur-v, blit
    post_binds: [wgpu::BindGroup; 4],

    quad_layout: wgpu::BindGroupLayout,
    quad_pipeline: wgpu::RenderPipeline,
    quad_ubuf: wgpu::Buffer,
    quad_bind: wgpu::BindGroup,
    quad_buf: wgpu::Buffer,
    quad_cap: usize,
    quad_ranges: [(u32, u32); LAYERS],
    quad_hash: u64,

    atlas: TextAtlas,
    viewport: Viewport,
    text_layers: Vec<TextRenderer>,
    text_hash: u64,
}

fn fullscreen_pipeline(
    dev: &wgpu::Device,
    label: &str,
    layout: &wgpu::BindGroupLayout,
    module: &wgpu::ShaderModule,
    fs: &str,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let pl = dev.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(layout)],
        ..Default::default()
    });
    dev.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&pl),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(fs),
            targets: &[Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        cache: None,
        multiview_mask: None,
    })
}

fn uniform_entry(binding: u32, vis: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: vis,
        ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
        count: None,
    }
}

fn tex_entries(first: u32) -> [wgpu::BindGroupLayoutEntry; 2] {
    [
        wgpu::BindGroupLayoutEntry {
            binding: first,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                multisampled: false,
                view_dimension: wgpu::TextureViewDimension::D2,
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
            },
            count: None,
        },
        wgpu::BindGroupLayoutEntry {
            binding: first + 1,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        },
    ]
}

fn tex_bind(
    dev: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    ubuf: &wgpu::Buffer,
    view: &wgpu::TextureView,
    sampler: &wgpu::Sampler,
) -> wgpu::BindGroup {
    dev.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: ubuf.as_entire_binding() },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(view) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(sampler) },
        ],
    })
}

impl Renderer {
    pub fn new(gpu: &Gpu, format: wgpu::TextureFormat, w: u32, h: u32) -> Renderer {
        let dev = &gpu.device;
        let sampler = dev.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("linear clamp"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // background
        let bg_mod = dev.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("bg"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/bg.wgsl").into()),
        });
        let bg_layout = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bg"),
            entries: &[uniform_entry(0, wgpu::ShaderStages::FRAGMENT)],
        });
        let bg_ubuf = dev.create_buffer(&wgpu::BufferDescriptor {
            label: Some("bg uniforms"),
            size: std::mem::size_of::<BgRaw>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bg_bind = dev.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &bg_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: bg_ubuf.as_entire_binding() }],
        });
        let bg_pipeline = fullscreen_pipeline(dev, "bg", &bg_layout, &bg_mod, "fs", BG_FORMAT);

        // post
        let post_mod = dev.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("post"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/post.wgsl").into()),
        });
        let [t0, t1] = tex_entries(1);
        let post_layout = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("post"),
            entries: &[uniform_entry(0, wgpu::ShaderStages::FRAGMENT), t0, t1],
        });
        let blur_pipeline = fullscreen_pipeline(dev, "blur", &post_layout, &post_mod, "fs_blur", BG_FORMAT);
        let blit_pipeline = fullscreen_pipeline(dev, "blit", &post_layout, &post_mod, "fs_blit", format);
        let post_ubufs: [wgpu::Buffer; 4] = std::array::from_fn(|_| {
            dev.create_buffer(&wgpu::BufferDescriptor {
                label: Some("post uniforms"),
                size: 16,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        });

        // quads
        let quad_mod = dev.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("quad"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/quad.wgsl").into()),
        });
        let [q0, q1] = tex_entries(1);
        let quad_layout = dev.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("quad"),
            entries: &[uniform_entry(0, wgpu::ShaderStages::VERTEX_FRAGMENT), q0, q1],
        });
        let quad_pl = dev.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("quad"),
            bind_group_layouts: &[Some(&quad_layout)],
            ..Default::default()
        });
        let attrs = wgpu::vertex_attr_array![
            0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4,
            4 => Float32x4, 5 => Float32x4, 6 => Float32x4
        ];
        let quad_pipeline = dev.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("quad"),
            layout: Some(&quad_pl),
            vertex: wgpu::VertexState {
                module: &quad_mod,
                entry_point: Some("vs"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Quad>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &attrs,
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &quad_mod,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            cache: None,
            multiview_mask: None,
        });
        let quad_ubuf = dev.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quad globals"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let quad_cap = 1024;
        let quad_buf = dev.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quads"),
            size: (quad_cap * std::mem::size_of::<Quad>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // text
        let cache = Cache::new(dev);
        let viewport = Viewport::new(dev, &cache);
        let mut atlas = TextAtlas::with_color_mode(dev, &gpu.queue, &cache, format, ColorMode::Web);
        let text_layers =
            (0..LAYERS).map(|_| TextRenderer::new(&mut atlas, dev, wgpu::MultisampleState::default(), None)).collect();

        // placeholder targets; `resize` builds the real ones
        let bg_tex = make_target(dev, 1, 1, "bg");
        let blur_a = make_target(dev, 1, 1, "blur a");
        let blur_b = make_target(dev, 1, 1, "blur b");
        let post_binds = [
            tex_bind(dev, &post_layout, &post_ubufs[0], &bg_tex.view, &sampler),
            tex_bind(dev, &post_layout, &post_ubufs[1], &blur_a.view, &sampler),
            tex_bind(dev, &post_layout, &post_ubufs[2], &blur_b.view, &sampler),
            tex_bind(dev, &post_layout, &post_ubufs[3], &bg_tex.view, &sampler),
        ];
        let quad_bind = tex_bind(dev, &quad_layout, &quad_ubuf, &blur_a.view, &sampler);

        let mut r = Renderer {
            format,
            size: (0, 0),
            sampler,
            bg_pipeline,
            bg_ubuf,
            bg_bind,
            bg_scale: 0.5,
            bg_tex,
            blur_a,
            blur_b,
            have_bg: false,
            post_layout,
            blur_pipeline,
            blit_pipeline,
            post_ubufs,
            post_binds,
            quad_layout,
            quad_pipeline,
            quad_ubuf,
            quad_bind,
            quad_buf,
            quad_cap,
            quad_ranges: [(0, 0); LAYERS],
            quad_hash: 0,
            atlas,
            viewport,
            text_layers,
            text_hash: 0,
        };
        r.resize(gpu, w, h);
        r
    }

    pub fn format(&self) -> wgpu::TextureFormat {
        self.format
    }

    pub fn resize(&mut self, gpu: &Gpu, w: u32, h: u32) {
        let (w, h) = (w.max(1), h.max(1));
        if self.size == (w, h) {
            return;
        }
        self.size = (w, h);
        self.rebuild_targets(gpu);
        self.text_hash = 0;
    }

    fn rebuild_targets(&mut self, gpu: &Gpu) {
        let dev = &gpu.device;
        let (w, h) = self.size;
        let bw = ((w as f32 * self.bg_scale).ceil() as u32).max(1);
        let bh = ((h as f32 * self.bg_scale).ceil() as u32).max(1);
        self.bg_tex = make_target(dev, bw, bh, "bg");
        self.blur_a = make_target(dev, bw / 4, bh / 4, "blur a");
        self.blur_b = make_target(dev, bw / 4, bh / 4, "blur b");
        self.post_binds = [
            tex_bind(dev, &self.post_layout, &self.post_ubufs[0], &self.bg_tex.view, &self.sampler),
            tex_bind(dev, &self.post_layout, &self.post_ubufs[1], &self.blur_a.view, &self.sampler),
            tex_bind(dev, &self.post_layout, &self.post_ubufs[2], &self.blur_b.view, &self.sampler),
            tex_bind(dev, &self.post_layout, &self.post_ubufs[3], &self.bg_tex.view, &self.sampler),
        ];
        self.quad_bind = tex_bind(dev, &self.quad_layout, &self.quad_ubuf, &self.blur_a.view, &self.sampler);
        self.have_bg = false;
    }

    fn pass<'a>(enc: &'a mut wgpu::CommandEncoder, view: &'a wgpu::TextureView, label: &'a str) -> wgpu::RenderPass<'a> {
        enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some(label),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        })
    }

    /// Draw a frame into `target`. `scene_dirty` says whether the draw list
    /// changed since the previous call.
    pub fn render(
        &mut self,
        gpu: &Gpu,
        target: &wgpu::TextureView,
        scene: &Scene,
        scene_dirty: bool,
        bg: &BgFrame,
        text: &mut TextSystem,
    ) {
        let (w, h) = self.size;
        let q = &gpu.queue;
        let dev = &gpu.device;

        let scale = bg.scale.clamp(0.1, 1.0);
        if (scale - self.bg_scale).abs() > 1e-3 {
            self.bg_scale = scale;
            self.rebuild_targets(gpu);
        }
        let render_bg = bg.render || !self.have_bg;

        // ---- uniforms
        if render_bg {
            let raw = BgRaw { res: [self.bg_tex.w as f32, self.bg_tex.h as f32, bg.fade, 0.0], sa: bg.a, sb: bg.b };
            q.write_buffer(&self.bg_ubuf, 0, bytemuck::bytes_of(&raw));
            let spread = (self.bg_scale / 0.5).max(1.0);
            let (bw, bh) = (self.blur_a.w as f32, self.blur_a.h as f32);
            let down = [1.0 / self.bg_tex.w as f32, 1.0 / self.bg_tex.h as f32, 0.0, 0.0f32];
            let bh_ = [spread / bw, 0.0, 1.0, 0.0f32];
            let bv_ = [0.0, spread / bh, 1.0, 0.0f32];
            q.write_buffer(&self.post_ubufs[0], 0, bytemuck::bytes_of(&down));
            q.write_buffer(&self.post_ubufs[1], 0, bytemuck::bytes_of(&bh_));
            q.write_buffer(&self.post_ubufs[2], 0, bytemuck::bytes_of(&bv_));
        }
        q.write_buffer(&self.post_ubufs[3], 0, bytemuck::bytes_of(&[w as f32, h as f32, bg.time, bg.vignette]));

        // ---- quads
        if scene_dirty {
            let mut all: Vec<Quad> = Vec::with_capacity(scene.layers.iter().map(|l| l.quads.len()).sum());
            for (i, l) in scene.layers.iter().enumerate() {
                let start = all.len() as u32;
                all.extend_from_slice(&l.quads);
                self.quad_ranges[i] = (start, all.len() as u32);
            }
            let bytes: &[u8] = bytemuck::cast_slice(&all);
            let mut hsh = std::collections::hash_map::DefaultHasher::new();
            bytes.hash(&mut hsh);
            let hv = hsh.finish();
            if hv != self.quad_hash {
                self.quad_hash = hv;
                if all.len() > self.quad_cap {
                    self.quad_cap = all.len().next_power_of_two();
                    self.quad_buf = dev.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("quads"),
                        contents: bytemuck::cast_slice(&{
                            let mut v = all.clone();
                            v.resize(self.quad_cap, Quad::zeroed());
                            v
                        }),
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    });
                } else if !bytes.is_empty() {
                    q.write_buffer(&self.quad_buf, 0, bytes);
                }
            }
        }
        q.write_buffer(&self.quad_ubuf, 0, bytemuck::bytes_of(&[w as f32, h as f32, bg.time, 0.0]));

        // ---- text
        if scene_dirty {
            let mut hsh = std::collections::hash_map::DefaultHasher::new();
            for l in &scene.layers {
                l.texts.len().hash(&mut hsh);
                for t in &l.texts {
                    t.key.hash(&mut hsh);
                    (t.x.to_bits(), t.y.to_bits()).hash(&mut hsh);
                    (t.clip.x.to_bits(), t.clip.y.to_bits(), t.clip.w.to_bits(), t.clip.h.to_bits()).hash(&mut hsh);
                    t.color.to_u8().hash(&mut hsh);
                }
            }
            let hv = hsh.finish();
            if hv != self.text_hash {
                self.text_hash = hv;
                self.viewport.update(q, Resolution { width: w, height: h });
                self.atlas.trim();
                let (fonts, swash, shaped_cache) = text.parts();
                for (i, l) in scene.layers.iter().enumerate() {
                    let areas = l.texts.iter().filter_map(|t| {
                        let shaped = shaped_cache.get(&t.key)?;
                        let [r, g, b, a] = t.color.to_u8();
                        Some(TextArea {
                            buffer: &shaped.buffer,
                            left: t.x,
                            top: t.y,
                            scale: 1.0,
                            bounds: TextBounds {
                                left: t.clip.x.floor() as i32,
                                top: t.clip.y.floor() as i32,
                                right: t.clip.right().ceil() as i32,
                                bottom: t.clip.bottom().ceil() as i32,
                            },
                            default_color: glyphon::Color::rgba(r, g, b, a),
                            custom_glyphs: &[],
                        })
                    });
                    if let Err(e) =
                        self.text_layers[i].prepare(dev, q, fonts, &mut self.atlas, &self.viewport, areas, swash)
                    {
                        log::warn!("text prepare failed: {e:?}");
                    }
                }
            }
        }

        // ---- passes
        let mut enc = dev.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        if render_bg {
            {
                let mut p = Self::pass(&mut enc, &self.bg_tex.view, "bg");
                p.set_pipeline(&self.bg_pipeline);
                p.set_bind_group(0, &self.bg_bind, &[]);
                p.draw(0..3, 0..1);
            }
            // downsample then two gaussian iterations
            let steps: [(usize, &wgpu::TextureView); 5] = [
                (0, &self.blur_a.view),
                (1, &self.blur_b.view),
                (2, &self.blur_a.view),
                (1, &self.blur_b.view),
                (2, &self.blur_a.view),
            ];
            for (bind, dst) in steps {
                let mut p = Self::pass(&mut enc, dst, "blur");
                p.set_pipeline(&self.blur_pipeline);
                p.set_bind_group(0, &self.post_binds[bind], &[]);
                p.draw(0..3, 0..1);
            }
            self.have_bg = true;
        }
        {
            let mut p = Self::pass(&mut enc, target, "main");
            p.set_pipeline(&self.blit_pipeline);
            p.set_bind_group(0, &self.post_binds[3], &[]);
            p.draw(0..3, 0..1);
            for i in 0..LAYERS {
                let (a, b) = self.quad_ranges[i];
                if b > a {
                    p.set_pipeline(&self.quad_pipeline);
                    p.set_bind_group(0, &self.quad_bind, &[]);
                    p.set_vertex_buffer(0, self.quad_buf.slice(..));
                    p.draw(0..4, a..b);
                }
                if !scene.layers[i].texts.is_empty() {
                    if let Err(e) = self.text_layers[i].render(&self.atlas, &self.viewport, &mut p) {
                        log::warn!("text render failed: {e:?}");
                    }
                }
            }
        }
        q.submit(Some(enc.finish()));
    }
}

/// Render one frame off-screen and return RGBA pixels. Used by `--snapshot`.
pub fn render_to_pixels(
    gpu: &Gpu,
    renderer: &mut Renderer,
    w: u32,
    h: u32,
    scene: &Scene,
    bg: &BgFrame,
    text: &mut TextSystem,
) -> Vec<u8> {
    let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("snapshot"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: renderer.format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    renderer.resize(gpu, w, h);
    renderer.render(gpu, &view, scene, true, bg, text);

    let row = (w * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("snapshot readback"),
        size: (row * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture: &tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(h) },
        },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("gpu poll");
    let data = buf.slice(..).get_mapped_range().expect("map readback");
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let s = (y * row) as usize;
        out.extend_from_slice(&data[s..s + (w * 4) as usize]);
    }
    out
}

pub fn save_png(path: &std::path::Path, w: u32, h: u32, rgba: &[u8]) -> std::io::Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().map_err(std::io::Error::other)?;
    wr.write_image_data(rgba).map_err(std::io::Error::other)?;
    Ok(())
}

/// Create a device without a window (for snapshots).
pub fn headless_gpu() -> Result<Gpu, String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    }))
    .map_err(|e| format!("no GPU adapter: {e}"))?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
        .map_err(|e| format!("no GPU device: {e}"))?;
    Ok(Gpu { device, queue })
}
