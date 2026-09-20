//! The grid on the GPU: one persistent texture, damage-band uploads.
//!
//! This is the whole reason the frontend is on `iced::widget::shader` rather
//! than `iced::widget::image`. An iced image handle is immutable and cached by
//! id, so putting a terminal grid through one means a new handle — and a new
//! texture — per damaged frame, which is precisely the Bevy terminal's
//! `Image::new`-per-frame cost that the 2026-09-20 memory anatomy found as
//! 320 MB in three GEM objects.
//!
//! Here the texture is created once per grid geometry and lives until the
//! geometry changes. A damaged frame costs `queue.write_texture` over the rows
//! that actually changed; an idle frame costs a three-vertex draw and no
//! transfer at all.

use crate::frame::Frame;
use iced::wgpu;
use iced::widget::shader;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// The `Shader` program: it owns nothing but the handle to the shared frame.
#[derive(Clone)]
pub struct GridProgram {
    frame: Arc<Mutex<Frame>>,
}

/// Which grid a primitive is for.
///
/// iced keeps **one** `Pipeline` per `Primitive` TYPE, in a `Storage` keyed on
/// that type's id — so every `GridPrimitive` in the tree shares one
/// `GridPipeline`, and a single `texture` field on it would mean pane B's
/// `prepare` overwriting pane A's texture and both `draw`s binding B's
/// (cold-review finding, 2026-09-21). T3 puts two of these on screen, so the
/// pipeline holds a texture per grid and each primitive addresses its own by
/// the identity of the `Frame` it renders.
type GridId = usize;

fn grid_id(frame: &Arc<Mutex<Frame>>) -> GridId {
    Arc::as_ptr(frame) as GridId
}

impl GridProgram {
    pub fn new(frame: Arc<Mutex<Frame>>) -> Self {
        Self { frame }
    }
}

impl<Message> shader::Program<Message> for GridProgram {
    type State = ();
    type Primitive = GridPrimitive;

    fn draw(
        &self,
        _state: &Self::State,
        _cursor: iced::mouse::Cursor,
        _bounds: iced::Rectangle,
    ) -> Self::Primitive {
        GridPrimitive {
            frame: self.frame.clone(),
        }
    }
}

pub struct GridPrimitive {
    frame: Arc<Mutex<Frame>>,
}

// `Primitive: Debug`, and a Mutex<Frame> has nothing legible to print — the
// derive would only add a `Frame: Debug` bound for a line nobody reads.
impl std::fmt::Debug for GridPrimitive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GridPrimitive")
    }
}

impl shader::Primitive for GridPrimitive {
    type Pipeline = GridPipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _bounds: &iced::Rectangle,
        _viewport: &shader::Viewport,
    ) {
        let id = grid_id(&self.frame);
        pipeline.live.insert(id);
        let mut frame = self.frame.lock().expect("frame lock");
        let (width, height) = (frame.surface().width(), frame.surface().height());
        if width == 0 || height == 0 {
            // The grid has no paintable rows. Drop the texture rather than
            // keep presenting pixels whose source is gone.
            pipeline.textures.remove(&id);
            frame.clear_damage();
            return;
        }
        let stride = frame.surface().stride();
        // A texture that was just created holds nothing, so pending damage is
        // not merely stale, it is wrong: upload the lot and drop it.
        if pipeline.ensure_texture(device, id, width, height) {
            pipeline.upload(queue, id, frame.surface().rgba(), stride, 0, height);
            frame.clear_damage();
            return;
        }
        for band in frame.take_damage() {
            // Clamp rather than trust: the surface is re-measured above, and a
            // band recorded against a larger one would be a GPU-side panic.
            let y = band.y.min(height);
            let rows = band.height.min(height - y);
            if rows > 0 {
                pipeline.upload(queue, id, frame.surface().rgba(), stride, y, rows);
            }
        }
    }

    fn draw(&self, pipeline: &Self::Pipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        let Some(texture) = pipeline.textures.get(&grid_id(&self.frame)) else {
            // True regardless: the fallback `render` path would begin a whole
            // extra render pass to draw the nothing we have.
            return true;
        };
        render_pass.set_pipeline(&pipeline.pipeline);
        render_pass.set_bind_group(0, &texture.bind_group, &[]);
        render_pass.draw(0..3, 0..1);
        true
    }
}

struct GridTexture {
    bind_group: wgpu::BindGroup,
    texture: wgpu::Texture,
    width: u32,
    height: u32,
}

pub struct GridPipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// The texture's own format, chosen to make the sample -> write path an
    /// identity transform against whatever surface format iced gave us.
    format: wgpu::TextureFormat,
    /// One entry per live grid; see [`GridId`].
    textures: HashMap<GridId, GridTexture>,
    /// Grids that prepared this frame. `trim` drops everything else, so a
    /// closed pane's texture is freed on the next frame rather than living
    /// until the process exits — and an `Arc` address reused by a new pane
    /// cannot inherit the old pane's pixels.
    live: HashSet<GridId>,
}

impl GridPipeline {
    /// Creates or resizes a grid's texture. Returns true when the caller now
    /// owes a full upload.
    fn ensure_texture(
        &mut self,
        device: &wgpu::Device,
        id: GridId,
        width: u32,
        height: u32,
    ) -> bool {
        if self
            .textures
            .get(&id)
            .is_some_and(|texture| texture.width == width && texture.height == height)
        {
            return false;
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("term grid"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("term grid bind group"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.textures.insert(
            id,
            GridTexture {
                bind_group,
                texture,
                width,
                height,
            },
        );
        true
    }

    /// Writes `rows` rows starting at `y` from the CPU surface. `rgba` is the
    /// whole surface; the row offset rides in the copy layout, so no slice and
    /// no temporary buffer is made for a band.
    fn upload(
        &self,
        queue: &wgpu::Queue,
        id: GridId,
        rgba: &[u8],
        stride: usize,
        y: u32,
        rows: u32,
    ) {
        let Some(target) = self.textures.get(&id) else {
            return;
        };
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &target.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: y as u64 * stride as u64,
                bytes_per_row: Some(stride as u32),
                rows_per_image: Some(rows),
            },
            wgpu::Extent3d {
                width: target.width,
                height: rows,
                depth_or_array_layers: 1,
            },
        );
    }
}

impl shader::Pipeline for GridPipeline {
    fn new(
        device: &wgpu::Device,
        _queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
    ) -> Self {
        // The VT hands us sRGB-encoded bytes. Matching the target's encoding
        // makes sample-then-write an identity: against an sRGB target the
        // sampler linearises and the blend-free write re-encodes; against a
        // plain Unorm target neither happens. Getting this backwards is the
        // classic washed-out-terminal bug, and it is invisible in a unit test.
        let texture_format = if format.is_srgb() {
            wgpu::TextureFormat::Rgba8UnormSrgb
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("term grid shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("grid.wgsl").into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("term grid bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("term grid pipeline layout"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("term grid pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    // The grid is opaque by construction (the raster writes
                    // alpha 255 into every cell), so there is nothing to blend
                    // and REPLACE saves the read.
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("term grid sampler"),
            // Nearest, and the quad is laid out at exactly the texture's
            // physical size: a terminal that resamples its own glyphs is a
            // blurry terminal, which is the HiDPI bug the raster's physical
            // -pixel cells exist to avoid.
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        Self {
            pipeline,
            layout,
            sampler,
            format: texture_format,
            textures: HashMap::new(),
            live: HashSet::new(),
        }
    }

    /// Called by iced at the end of each frame.
    fn trim(&mut self) {
        self.textures.retain(|id, _| self.live.contains(id));
        self.live.clear();
    }
}
