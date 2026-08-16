//! One renderer.
//!
//! Everything that composites a frame — stacking tracks, scaling and positioning pictures,
//! colour adjustment, opacity, fades, dissolves and titles — happens here, on the GPU, once.
//! The preview draws the result into the window; the export reads the same result back frame by
//! frame and hands raw pixels to ffmpeg, which is left with demux, decode and encode.
//!
//! The two paths differ only in where the pictures come from ([`FrameSource`]) and what happens
//! to the finished target. The compositing itself is a single code path, which is the entire
//! point: an effect written here appears in both, and cannot drift between them.
//!
//! Working targets are `Rgba16Float`. Source pictures are uploaded as `Rgba8Unorm` — deliberately
//! *not* sRGB — because the colour maths Kite already shipped is defined on gamma-encoded 0..1
//! values, and phase A is about having one renderer, not about changing what a grade looks like.
//! The conversion to display gamma happens once, in the blit that hands the frame to the window.

use crate::decode::DecodedFrame;
use crate::project::{
    Clip, ClipId, ClipSource, ColorAdjust, MediaId, TextAlign, TextProps, Timeline, TrackKind,
};
use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// The plan: what a frame is made of, independent of any graphics API.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum LayerSource {
    /// A picture from a media item. `clip` is carried so an export can keep one decoder per clip
    /// even when two clips read the same file at different points.
    Media { clip: ClipId, media: MediaId, src_frame: i64 },
    Solid([u8; 4]),
    Text(TextProps),
}

#[derive(Clone, Debug)]
pub struct Layer {
    pub source: LayerSource,
    pub alpha: f32,
    pub scale: f32,
    pub pos_x: f32,
    pub pos_y: f32,
    pub color: ColorAdjust,
}

/// One frame, described bottom layer first.
#[derive(Clone, Debug)]
pub struct FramePlan {
    pub width: u32,
    pub height: u32,
    pub layers: Vec<Layer>,
}

fn layer_of(c: &Clip, f: i64, alpha: f32) -> Layer {
    Layer {
        source: match &c.source {
            ClipSource::Media(m) => LayerSource::Media {
                clip: c.id,
                media: *m,
                src_frame: c.source_frame(f).max(0),
            },
            ClipSource::Color(v) => LayerSource::Solid(*v),
            ClipSource::Text(t) => LayerSource::Text(t.clone()),
        },
        alpha,
        scale: c.scale,
        pos_x: c.pos_x,
        pos_y: c.pos_y,
        color: c.color,
    }
}

/// Works out what timeline frame `frame` is made of.
///
/// Video tracks are stored top-first and composite bottom-up, so they are walked in reverse. A
/// clip with a dissolve into it keeps the previous clip running underneath, using material past
/// its out point, and fades up over it — which is why a dissolve does not shorten the sequence.
pub fn plan_frame(tl: &Timeline, frame: i64, width: u32, height: u32) -> FramePlan {
    let mut layers = Vec::new();
    let video: Vec<_> = tl
        .tracks
        .iter()
        .filter(|t| t.kind == TrackKind::Video && !t.hidden)
        .collect();
    for track in video.iter().rev() {
        let Some(c) = track.clip_at(frame) else { continue };
        let mut alpha = c.alpha_at(frame);
        if c.transition_in > 0 && frame < c.start + c.transition_in {
            let t = ((frame - c.start) as f32 + 1.0) / c.transition_in as f32;
            if let Some(prev) = track.prev_clip(c.id) {
                layers.push(layer_of(prev, frame, prev.alpha_at(prev.end() - 1)));
            }
            alpha *= t.clamp(0.0, 1.0);
        }
        layers.push(layer_of(c, frame, alpha));
    }
    FramePlan { width, height, layers }
}

/// Where the renderer gets pictures from. The preview answers out of the proxy cache; the export
/// answers from full-resolution decoders running over the original files.
pub trait FrameSource {
    fn frame(&mut self, clip: ClipId, media: MediaId, src_frame: i64) -> Option<Arc<DecodedFrame>>;
}

/// A source that has nothing in it, for planning-only checks.
pub struct NoFrames;
impl FrameSource for NoFrames {
    fn frame(&mut self, _: ClipId, _: MediaId, _: i64) -> Option<Arc<DecodedFrame>> {
        None
    }
}

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Gpu {
    pub device: Arc<wgpu::Device>,
    pub queue: Arc<wgpu::Queue>,
    pub adapter: String,
}

impl Gpu {
    /// A device with no window attached, which is what the export renders on.
    pub fn headless() -> Result<Self> {
        let backends = wgpu::Instance::enabled_backend_features();
        if backends.is_empty() {
            return Err(anyhow!(
                "this build has no GPU backend compiled in; check the wgpu features in Cargo.toml"
            ));
        }
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor { backends, ..Default::default() });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .map_err(|e| anyhow!("no usable graphics adapter for rendering: {e}"))?;
        let name = {
            let i = adapter.get_info();
            format!("{} ({:?})", i.name, i.backend)
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("kite-render"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..Default::default()
        }))
        .context("creating a graphics device for rendering")?;
        Ok(Self { device: Arc::new(device), queue: Arc::new(queue), adapter: name })
    }
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

const TARGET_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
const DISPLAY_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const SOURCE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// Uniform blocks are addressed with a dynamic offset, which must be aligned.
const UNIFORM_STRIDE: u64 = 256;
/// How many uploaded pictures to keep. A crossfade with a picture-in-picture over it touches
/// three at once, so this is generous rather than tight.
const TEX_BUDGET: usize = 48;

struct Cached {
    view: wgpu::TextureView,
    bind: wgpu::BindGroup,
    used: u64,
}

struct Target {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

pub struct Renderer {
    gpu: Gpu,
    composite: wgpu::RenderPipeline,
    to_display: wgpu::RenderPipeline,
    mip: wgpu::RenderPipeline,
    draw_layout: wgpu::BindGroupLayout,
    tex_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    uniform_slots: u64,
    draw_bind: wgpu::BindGroup,
    textures: HashMap<u64, Cached>,
    tick: u64,
    target: Option<Target>,
    display: Option<Target>,
    font: Option<ab_glyph::FontVec>,
    /// Rasterised titles, keyed on the text and the resolution it was drawn for.
    text_cache: HashMap<u64, Option<RasterText>>,
}

#[derive(Clone)]
struct RasterText {
    rgba: Vec<u8>,
    w: u32,
    h: u32,
    x: f32,
    y: f32,
}

impl Renderer {
    pub fn new(gpu: Gpu) -> Result<Self> {
        let device = gpu.device.clone();
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("kite-composite"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let draw_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("kite-draw"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: std::num::NonZeroU64::new(48),
                },
                count: None,
            }],
        });
        let tex_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("kite-texture"),
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
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("kite-pipeline"),
            bind_group_layouts: &[&draw_layout, &tex_layout],
            push_constant_ranges: &[],
        });

        let make = |name: &str, entry: &str, format: wgpu::TextureFormat, blend: bool| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(name),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some(entry),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        // The fragment shader writes premultiplied alpha, so "over" is a plain
                        // add against one-minus-source.
                        blend: blend.then_some(wgpu::BlendState {
                            color: wgpu::BlendComponent {
                                src_factor: wgpu::BlendFactor::One,
                                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                                operation: wgpu::BlendOperation::Add,
                            },
                            alpha: wgpu::BlendComponent {
                                src_factor: wgpu::BlendFactor::One,
                                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                                operation: wgpu::BlendOperation::Add,
                            },
                        }),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            })
        };

        let composite = make("kite-composite", "fs_composite", TARGET_FORMAT, true);
        let to_display = make("kite-display", "fs_display", DISPLAY_FORMAT, false);
        let mip = make("kite-mip", "fs_copy", SOURCE_FORMAT, false);

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("kite-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let uniform_slots = 64;
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("kite-draws"),
            size: uniform_slots * UNIFORM_STRIDE,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let draw_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("kite-draws"),
            layout: &draw_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &uniforms,
                    offset: 0,
                    size: std::num::NonZeroU64::new(48),
                }),
            }],
        });

        let font = embedded_font();

        let mut me = Self {
            gpu,
            composite,
            to_display,
            mip,
            draw_layout,
            tex_layout,
            sampler,
            uniforms,
            uniform_slots,
            draw_bind,
            textures: HashMap::new(),
            tick: 0,
            target: None,
            display: None,
            font,
            text_cache: HashMap::new(),
        };
        // A one-pixel white texture stands in for solid colour cards, so every layer goes
        // through exactly the same shader.
        me.upload(WHITE_KEY, 1, 1, &[255, 255, 255, 255], false);
        Ok(me)
    }

    pub fn adapter(&self) -> &str {
        &self.gpu.adapter
    }

    /// Composites one frame.
    pub fn render(&mut self, plan: &FramePlan, source: &mut dyn FrameSource) -> Result<()> {
        self.tick += 1;
        let (w, h) = (plan.width.max(1), plan.height.max(1));
        self.ensure_target(w, h);

        // Resolve every layer into a texture key, a destination rectangle and a uniform block
        // before touching the command encoder, because uploads may reallocate the texture map.
        let mut draws: Vec<(u64, [f32; 12])> = Vec::new();
        for l in &plan.layers {
            let alpha = l.alpha.clamp(0.0, 1.0);
            if alpha <= 0.0 {
                continue;
            }
            let (key, rect, tint) = match &l.source {
                LayerSource::Media { clip, media, src_frame } => {
                    let Some(img) = source.frame(*clip, *media, *src_frame) else { continue };
                    if img.width == 0 || img.height == 0 {
                        continue;
                    }
                    let key = media_key(*media, *src_frame, img.width, img.height);
                    if !self.textures.contains_key(&key) {
                        self.upload(key, img.width, img.height, &img.rgba, true);
                    }
                    let aspect = img.width as f32 / img.height.max(1) as f32;
                    (key, fit_rect(w, h, aspect, l.scale, l.pos_x, l.pos_y), [1.0, 1.0, 1.0, 1.0])
                }
                LayerSource::Solid(c) => (
                    WHITE_KEY,
                    [0.0, 0.0, w as f32, h as f32],
                    [
                        c[0] as f32 / 255.0,
                        c[1] as f32 / 255.0,
                        c[2] as f32 / 255.0,
                        c[3] as f32 / 255.0,
                    ],
                ),
                LayerSource::Text(t) => {
                    let Some(r) = self.raster_text(t, w, h) else { continue };
                    let key = text_key(t, w, h);
                    if !self.textures.contains_key(&key) {
                        let (rw, rh, px) = (r.w, r.h, r.rgba.clone());
                        self.upload(key, rw, rh, &px, false);
                    }
                    (key, [r.x, r.y, r.x + r.w as f32, r.y + r.h as f32], [1.0, 1.0, 1.0, 1.0])
                }
            };
            if let Some(c) = self.textures.get_mut(&key) {
                c.used = self.tick;
            }
            let ndc = [
                rect[0] / w as f32 * 2.0 - 1.0,
                1.0 - rect[1] / h as f32 * 2.0,
                rect[2] / w as f32 * 2.0 - 1.0,
                1.0 - rect[3] / h as f32 * 2.0,
            ];
            draws.push((
                key,
                [
                    ndc[0],
                    ndc[1],
                    ndc[2],
                    ndc[3],
                    tint[0],
                    tint[1],
                    tint[2],
                    tint[3],
                    l.color.brightness,
                    l.color.contrast,
                    l.color.saturation,
                    alpha,
                ],
            ));
        }

        self.ensure_uniform_capacity(draws.len() as u64);
        for (i, (_, u)) in draws.iter().enumerate() {
            let mut bytes = [0u8; 48];
            for (j, v) in u.iter().enumerate() {
                bytes[j * 4..j * 4 + 4].copy_from_slice(&v.to_ne_bytes());
            }
            self.gpu.queue.write_buffer(&self.uniforms, i as u64 * UNIFORM_STRIDE, &bytes);
        }

        let target = self.target.as_ref().expect("target was just ensured");
        let mut enc = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("kite-frame") });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("kite-composite"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Both old paths started from opaque black.
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.composite);
            for (i, (key, _)) in draws.iter().enumerate() {
                let Some(c) = self.textures.get(key) else { continue };
                pass.set_bind_group(0, &self.draw_bind, &[(i as u64 * UNIFORM_STRIDE) as u32]);
                pass.set_bind_group(1, &c.bind, &[]);
                pass.draw(0..6, 0..1);
            }
        }
        self.gpu.queue.submit([enc.finish()]);
        self.evict();
        Ok(())
    }

    /// Converts the fp16 target into an sRGB texture the window can sample, and returns its view.
    ///
    /// The renderer works in the gamma-encoded space Kite's colour controls are defined on, so
    /// the values are turned back into linear light here — the sRGB target then re-encodes them,
    /// and what reaches the screen is exactly the number the shader computed.
    pub fn to_display_texture(&mut self) -> Option<&wgpu::TextureView> {
        let (w, h) = {
            let t = self.target.as_ref()?;
            (t.width, t.height)
        };
        let stale = self.display.as_ref().is_none_or(|d| d.width != w || d.height != h);
        if stale {
            self.display = Some(self.make_target(w, h, DISPLAY_FORMAT, false));
        }
        let src = self.target.as_ref()?;
        let bind = self.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("kite-display-src"),
            layout: &self.tex_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&src.view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        let full = full_screen_uniform();
        self.gpu.queue.write_buffer(&self.uniforms, 0, &full);

        let dst = self.display.as_ref()?;
        let mut enc = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("kite-display") });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("kite-display"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &dst.view,
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
            });
            pass.set_pipeline(&self.to_display);
            pass.set_bind_group(0, &self.draw_bind, &[0u32]);
            pass.set_bind_group(1, &bind, &[]);
            pass.draw(0..6, 0..1);
        }
        self.gpu.queue.submit([enc.finish()]);
        self.display.as_ref().map(|d| &d.view)
    }

    /// Pulls the composited frame back as tightly packed RGBA8, which is what the encoder eats.
    pub fn read_rgba(&self) -> Result<Vec<u8>> {
        let t = self.target.as_ref().context("nothing has been rendered yet")?;
        let (w, h) = (t.width, t.height);
        let unpadded = w as u64 * 8; // Rgba16Float
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as u64;
        let padded = unpadded.div_ceil(align) * align;
        let buffer = self.gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("kite-readback"),
            size: padded * h as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("kite-readback") });
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &t.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded as u32),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.gpu.queue.submit([enc.finish()]);

        let (tx, rx) = std::sync::mpsc::channel();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.gpu
            .device
            .poll(wgpu::PollType::Wait { submission_index: None, timeout: None })
            .map_err(|e| anyhow!("waiting for the GPU: {e:?}"))?;
        rx.recv()
            .map_err(|_| anyhow!("the readback never completed"))?
            .map_err(|e| anyhow!("mapping the readback buffer: {e:?}"))?;

        let mut out = vec![0u8; (w * h * 4) as usize];
        {
            let view = buffer.slice(..).get_mapped_range();
            for y in 0..h as usize {
                let row = &view[y * padded as usize..y * padded as usize + unpadded as usize];
                let dst = &mut out[y * w as usize * 4..(y + 1) * w as usize * 4];
                for x in 0..w as usize {
                    for c in 0..4 {
                        let bits = u16::from_ne_bytes([row[x * 8 + c * 2], row[x * 8 + c * 2 + 1]]);
                        dst[x * 4 + c] = (half_to_f32(bits).clamp(0.0, 1.0) * 255.0).round() as u8;
                    }
                }
            }
        }
        buffer.unmap();
        Ok(out)
    }

    // -- plumbing ----------------------------------------------------------

    fn make_target(&self, w: u32, h: u32, format: wgpu::TextureFormat, readable: bool) -> Target {
        let mut usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
        if readable {
            usage |= wgpu::TextureUsages::COPY_SRC;
        }
        let texture = self.gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("kite-target"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        Target { texture, view, width: w, height: h }
    }

    fn ensure_target(&mut self, w: u32, h: u32) {
        if self.target.as_ref().is_none_or(|t| t.width != w || t.height != h) {
            self.target = Some(self.make_target(w, h, TARGET_FORMAT, true));
        }
    }

    fn ensure_uniform_capacity(&mut self, n: u64) {
        if n <= self.uniform_slots {
            return;
        }
        self.uniform_slots = n.next_power_of_two().max(64);
        self.uniforms = self.gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("kite-draws"),
            size: self.uniform_slots * UNIFORM_STRIDE,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.draw_bind = self.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("kite-draws"),
            layout: &self.draw_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &self.uniforms,
                    offset: 0,
                    size: std::num::NonZeroU64::new(48),
                }),
            }],
        });
    }

    /// Uploads a picture and, when it may end up minified, builds its mip chain.
    ///
    /// Without mips a picture-in-picture at a third of frame size samples one texel in nine and
    /// crawls with aliasing — visibly worse than the scaler the filtergraph used to run. The
    /// hardware picks the level from the quad's derivatives, so this is the whole fix.
    fn upload(&mut self, key: u64, w: u32, h: u32, rgba: &[u8], mips: bool) {
        let levels = if mips { (32 - w.max(h).leading_zeros()).max(1) } else { 1 };
        let texture = self.gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("kite-source"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: SOURCE_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        self.gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );

        if levels > 1 {
            let full = full_screen_uniform();
            self.gpu.queue.write_buffer(&self.uniforms, 0, &full);
            let mut enc = self.gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("kite-mips"),
            });
            for level in 1..levels {
                let src = texture.create_view(&wgpu::TextureViewDescriptor {
                    base_mip_level: level - 1,
                    mip_level_count: Some(1),
                    ..Default::default()
                });
                let dst = texture.create_view(&wgpu::TextureViewDescriptor {
                    base_mip_level: level,
                    mip_level_count: Some(1),
                    ..Default::default()
                });
                let bind = self.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("kite-mip-src"),
                    layout: &self.tex_layout,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&src) },
                        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                    ],
                });
                let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("kite-mip"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &dst,
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
                });
                pass.set_pipeline(&self.mip);
                pass.set_bind_group(0, &self.draw_bind, &[0u32]);
                pass.set_bind_group(1, &bind, &[]);
                pass.draw(0..6, 0..1);
            }
            self.gpu.queue.submit([enc.finish()]);
        }

        let view = texture.create_view(&Default::default());
        let bind = self.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("kite-source"),
            layout: &self.tex_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        self.textures.insert(key, Cached { view, bind, used: self.tick });
    }

    fn evict(&mut self) {
        if self.textures.len() <= TEX_BUDGET {
            return;
        }
        let mut ages: Vec<(u64, u64)> = self
            .textures
            .iter()
            .filter(|(k, _)| **k != WHITE_KEY)
            .map(|(k, v)| (v.used, *k))
            .collect();
        ages.sort_unstable();
        for (_, k) in ages.iter().take(self.textures.len() - TEX_BUDGET * 3 / 4) {
            self.textures.remove(k);
        }
    }

    fn raster_text(&mut self, t: &TextProps, w: u32, h: u32) -> Option<RasterText> {
        let key = text_key(t, w, h);
        if let Some(v) = self.text_cache.get(&key) {
            return v.clone();
        }
        let out = self.font.as_ref().and_then(|f| rasterise(f, t, w, h));
        if self.text_cache.len() > 32 {
            self.text_cache.clear();
        }
        self.text_cache.insert(key, out.clone());
        out
    }
}

/// Where a picture lands: fit to the frame preserving its own aspect, then scaled and offset.
/// The offsets are fractions of the *frame*, so a layout survives a change of resolution.
fn fit_rect(w: u32, h: u32, src_aspect: f32, scale: f32, px: f32, py: f32) -> [f32; 4] {
    let (fw, fh) = (w as f32, h as f32);
    let mut dw = fw;
    let mut dh = dw / src_aspect.max(0.0001);
    if dh > fh {
        dh = fh;
        dw = dh * src_aspect;
    }
    dw *= scale;
    dh *= scale;
    let cx = fw / 2.0 + px * fw;
    let cy = fh / 2.0 + py * fh;
    [cx - dw / 2.0, cy - dh / 2.0, cx + dw / 2.0, cy + dh / 2.0]
}

const WHITE_KEY: u64 = 0;

fn media_key(media: MediaId, frame: i64, w: u32, h: u32) -> u64 {
    let mut s = 0xcbf29ce484222325u64;
    for v in [media, frame as u64, w as u64, h as u64, 1] {
        s = (s ^ v).wrapping_mul(0x100000001b3);
    }
    s | 1
}

fn text_key(t: &TextProps, w: u32, h: u32) -> u64 {
    let mut s = 0xcbf29ce484222325u64;
    let mut mix = |v: u64| s = (s ^ v).wrapping_mul(0x100000001b3);
    for b in t.text.as_bytes() {
        mix(*b as u64);
    }
    mix(t.size.to_bits() as u64);
    mix(t.x.to_bits() as u64);
    mix(t.y.to_bits() as u64);
    mix(u32::from_ne_bytes(t.color) as u64);
    mix(t.align as u64);
    mix(t.bold as u64);
    mix(t.shadow as u64);
    mix(t.box_bg as u64);
    mix(w as u64);
    mix(h as u64);
    mix(2);
    s | 1
}

fn full_screen_uniform() -> [u8; 48] {
    let mut bytes = [0u8; 48];
    for (j, v) in [-1.0f32, 1.0, 1.0, -1.0, 1.0, 1.0, 1.0, 1.0, 0.0, 1.0, 1.0, 1.0]
        .iter()
        .enumerate()
    {
        bytes[j * 4..j * 4 + 4].copy_from_slice(&v.to_ne_bytes());
    }
    bytes
}

fn half_to_f32(bits: u16) -> f32 {
    let s = ((bits >> 15) & 1) as u32;
    let e = ((bits >> 10) & 0x1f) as u32;
    let m = (bits & 0x3ff) as u32;
    let out = if e == 0 {
        if m == 0 {
            s << 31
        } else {
            let mut exp = 127 - 15 + 1;
            let mut man = m;
            while man & 0x400 == 0 {
                man <<= 1;
                exp -= 1;
            }
            (s << 31) | ((exp as u32) << 23) | ((man & 0x3ff) << 13)
        }
    } else if e == 31 {
        (s << 31) | (0xff << 23) | (m << 13)
    } else {
        (s << 31) | ((e + 112) << 23) | (m << 13)
    };
    f32::from_bits(out)
}

// ---------------------------------------------------------------------------
// Titles
// ---------------------------------------------------------------------------

/// Titles use the font egui already embeds, so the preview, the export and every platform draw
/// the same glyphs without depending on anything being installed. The old export reached for a
/// system TrueType file and silently produced a substituted font when it could not open one.
fn embedded_font() -> Option<ab_glyph::FontVec> {
    let defs = egui::FontDefinitions::default();
    let data = defs.font_data.get("Ubuntu-Light")?;
    ab_glyph::FontVec::try_from_vec(data.font.to_vec()).ok()
}

struct Canvas {
    w: usize,
    h: usize,
    /// Premultiplied RGBA, so "over" is a straight lerp and layering is order-independent noise-free.
    px: Vec<f32>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        Self { w, h, px: vec![0.0; w * h * 4] }
    }
    fn over(&mut self, x: i64, y: i64, rgb: [f32; 3], a: f32) {
        if a <= 0.0 || x < 0 || y < 0 || x >= self.w as i64 || y >= self.h as i64 {
            return;
        }
        let a = a.min(1.0);
        let i = (y as usize * self.w + x as usize) * 4;
        for c in 0..3 {
            self.px[i + c] = rgb[c] * a + self.px[i + c] * (1.0 - a);
        }
        self.px[i + 3] = a + self.px[i + 3] * (1.0 - a);
    }
    fn to_rgba8(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.w * self.h * 4];
        for i in 0..self.w * self.h {
            let a = self.px[i * 4 + 3];
            for c in 0..3 {
                let v = if a > 0.0001 { self.px[i * 4 + c] / a } else { 0.0 };
                out[i * 4 + c] = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
            out[i * 4 + 3] = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        out
    }
}

fn rasterise(font: &ab_glyph::FontVec, t: &TextProps, w: u32, h: u32) -> Option<RasterText> {
    use ab_glyph::{Font, ScaleFont};
    let size = (t.size * h as f32).max(6.0);
    let scaled = font.as_scaled(ab_glyph::PxScale::from(size));
    let line_h = scaled.height() + scaled.line_gap();

    // Lay each line out once so both the box and the glyphs know where they are.
    let lines: Vec<&str> = t.text.split('\n').collect();
    let mut laid: Vec<(f32, Vec<(ab_glyph::GlyphId, f32)>)> = Vec::new();
    for line in &lines {
        let mut x = 0.0f32;
        let mut glyphs = Vec::new();
        let mut prev: Option<ab_glyph::GlyphId> = None;
        for ch in line.chars() {
            let id = scaled.glyph_id(ch);
            if let Some(p) = prev {
                x += scaled.kern(p, id);
            }
            glyphs.push((id, x));
            x += scaled.h_advance(id);
            prev = Some(id);
        }
        laid.push((x, glyphs));
    }
    let block_w = laid.iter().fold(0.0f32, |a, (w, _)| a.max(*w));
    let block_h = line_h * lines.len() as f32;
    if block_w <= 0.0 {
        return None;
    }

    // The anchor matches what the preview always did: horizontal by alignment, vertical centred.
    let anchor_x = t.x * w as f32;
    let anchor_y = t.y * h as f32;
    let left = match t.align {
        TextAlign::Left => anchor_x,
        TextAlign::Center => anchor_x - block_w / 2.0,
        TextAlign::Right => anchor_x - block_w,
    };
    let top = anchor_y - block_h / 2.0;

    let shadow_off = if t.shadow { (size * 0.035).max(1.0) } else { 0.0 };
    let bold_off = if t.bold { (size * 0.02).max(0.6) } else { 0.0 };
    let box_pad = if t.box_bg { size * 0.12 } else { 0.0 };
    let pad = (size * 0.35 + shadow_off + box_pad).ceil();

    let cw = (block_w + pad * 2.0).ceil() as usize;
    let ch = (block_h + pad * 2.0).ceil() as usize;
    if cw == 0 || ch == 0 || cw > 16384 || ch > 16384 {
        return None;
    }
    let mut canvas = Canvas::new(cw, ch);
    let ox = pad;
    let oy = pad;

    if t.box_bg {
        let x0 = (ox - box_pad).floor().max(0.0) as usize;
        let y0 = (oy - box_pad).floor().max(0.0) as usize;
        let x1 = ((ox + block_w + box_pad).ceil() as usize).min(cw);
        let y1 = ((oy + block_h + box_pad).ceil() as usize).min(ch);
        for y in y0..y1 {
            for x in x0..x1 {
                canvas.over(x as i64, y as i64, [0.0, 0.0, 0.0], 0.5);
            }
        }
    }

    let colour = [
        t.color[0] as f32 / 255.0,
        t.color[1] as f32 / 255.0,
        t.color[2] as f32 / 255.0,
    ];
    let text_alpha = t.color[3] as f32 / 255.0;

    // Shadow first, then the text over it. Emboldening is a few sub-pixel restrikes, which is
    // enough to read as bold without shipping a second font face.
    let mut strikes: Vec<(f32, f32, [f32; 3], f32)> = Vec::new();
    if t.shadow {
        strikes.push((shadow_off, shadow_off, [0.0, 0.0, 0.0], 0.6 * text_alpha));
    }
    strikes.push((0.0, 0.0, colour, text_alpha));
    if bold_off > 0.0 {
        strikes.push((bold_off, 0.0, colour, text_alpha));
        strikes.push((0.0, bold_off, colour, text_alpha));
        strikes.push((bold_off, bold_off, colour, text_alpha));
    }

    for (dx, dy, rgb, alpha) in strikes {
        for (li, (line_w, glyphs)) in laid.iter().enumerate() {
            let line_shift = match t.align {
                TextAlign::Left => 0.0,
                TextAlign::Center => (block_w - line_w) / 2.0,
                TextAlign::Right => block_w - line_w,
            };
            let baseline = oy + line_h * li as f32 + scaled.ascent();
            for (id, gx) in glyphs {
                let g = id.with_scale_and_position(
                    ab_glyph::PxScale::from(size),
                    ab_glyph::point(ox + line_shift + gx + dx, baseline + dy),
                );
                if let Some(outline) = font.outline_glyph(g) {
                    let bounds = outline.px_bounds();
                    outline.draw(|x, y, c| {
                        canvas.over(
                            bounds.min.x as i64 + x as i64,
                            bounds.min.y as i64 + y as i64,
                            rgb,
                            c * alpha,
                        );
                    });
                }
            }
        }
    }

    Some(RasterText {
        rgba: canvas.to_rgba8(),
        w: cw as u32,
        h: ch as u32,
        x: left - ox,
        y: top - oy,
    })
}

// ---------------------------------------------------------------------------

const SHADER: &str = r#"
struct Draw {
    rect: vec4<f32>,
    tint: vec4<f32>,
    adj:  vec4<f32>,
};

@group(0) @binding(0) var<uniform> d: Draw;
@group(1) @binding(0) var src: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    var us = array<f32, 6>(0.0, 1.0, 0.0, 0.0, 1.0, 1.0);
    var vs_ = array<f32, 6>(0.0, 0.0, 1.0, 1.0, 0.0, 1.0);
    let u = us[i];
    let v = vs_[i];
    var o: VsOut;
    o.pos = vec4<f32>(mix(d.rect.x, d.rect.z, u), mix(d.rect.y, d.rect.w, v), 0.0, 1.0);
    o.uv = vec2<f32>(u, v);
    return o;
}

// Contrast and brightness act on luma, saturation on the distance from it. This is the
// decomposition ffmpeg's `eq` filter uses, and the one Kite's colour controls were built against.
@fragment
fn fs_composite(in: VsOut) -> @location(0) vec4<f32> {
    let c = textureSample(src, samp, in.uv) * d.tint;
    let y = 0.299 * c.r + 0.587 * c.g + 0.114 * c.b;
    let y2 = clamp(d.adj.y * (y - 0.5) + 0.5 + d.adj.x, 0.0, 1.0);
    let rgb = clamp(vec3<f32>(y2) + (c.rgb - vec3<f32>(y)) * d.adj.z, vec3<f32>(0.0), vec3<f32>(1.0));
    let a = c.a * d.adj.w;
    return vec4<f32>(rgb * a, a);
}

@fragment
fn fs_copy(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(src, samp, in.uv);
}

fn to_linear(v: f32) -> f32 {
    if (v <= 0.04045) {
        return v / 12.92;
    }
    return pow((v + 0.055) / 1.055, 2.4);
}

// The target is sRGB, so it re-encodes whatever is written here. Undoing the encode first means
// the value that reaches the screen is the one the composite shader computed.
@fragment
fn fs_display(in: VsOut) -> @location(0) vec4<f32> {
    let c = textureSample(src, samp, in.uv);
    return vec4<f32>(to_linear(c.r), to_linear(c.g), to_linear(c.b), 1.0);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{Project, TrackKind};

    #[test]
    fn a_dissolve_plans_both_clips() {
        let mut p = Project::default();
        let a = p.new_clip(ClipSource::Color([255, 0, 0, 255]), 0, 30, 0);
        let mut b = p.new_clip(ClipSource::Color([0, 255, 0, 255]), 30, 30, 0);
        b.transition_in = 10;
        let tid = p.tracks().iter().find(|t| t.kind == TrackKind::Video).unwrap().id;
        p.track_mut(tid).unwrap().clips.push(a);
        p.track_mut(tid).unwrap().clips.push(b);
        p.normalize();

        let outside = plan_frame(p.tl(), 5, 320, 180);
        assert_eq!(outside.layers.len(), 1, "before the dissolve only one clip is live");

        let inside = plan_frame(p.tl(), 34, 320, 180);
        assert_eq!(inside.layers.len(), 2, "inside the dissolve both clips are live");
        assert!(
            inside.layers[1].alpha > 0.0 && inside.layers[1].alpha < 1.0,
            "the incoming clip should be part way up, got {}",
            inside.layers[1].alpha
        );
    }

    #[test]
    fn a_scaled_offset_layer_lands_where_it_should() {
        // A 16:9 source at 0.5 scale, pushed a quarter frame right, in a 16:9 frame.
        let r = fit_rect(1920, 1080, 16.0 / 9.0, 0.5, 0.25, 0.0);
        assert!((r[2] - r[0] - 960.0).abs() < 0.01, "half width, got {}", r[2] - r[0]);
        assert!((r[3] - r[1] - 540.0).abs() < 0.01, "half height");
        let cx = (r[0] + r[2]) / 2.0;
        assert!((cx - (960.0 + 480.0)).abs() < 0.01, "centre should shift by a quarter frame");
    }

    #[test]
    fn half_floats_decode() {
        for v in [0.0f32, 0.5, 1.0, 0.25, 0.75] {
            // Encode by hand the way the GPU would, then check the decode round-trips.
            let bits = f32_to_half(v);
            assert!((half_to_f32(bits) - v).abs() < 1e-3, "{v} did not round trip");
        }
    }

    fn f32_to_half(v: f32) -> u16 {
        let b = v.to_bits();
        let s = ((b >> 16) & 0x8000) as u16;
        let e = ((b >> 23) & 0xff) as i32 - 127 + 15;
        let m = (b & 0x7fffff) >> 13;
        if e <= 0 {
            return s;
        }
        s | ((e as u16) << 10) | m as u16
    }
}
