//! Render-world half: partial `write_texture` into the GPU image that Bevy
//! created once for each surface. The image asset itself is never mutated.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use bevy::render::render_asset::RenderAssets;
use bevy::render::render_resource::{
    Extent3d, Origin3d, TexelCopyBufferLayout, TexelCopyTextureInfo, TextureAspect, TextureId,
};
use bevy::render::renderer::RenderQueue;
use bevy::render::texture::GpuImage;
use bevy::render::{Extract, ExtractSchedule, Render, RenderApp, RenderSystems};

use crate::surface::Rect;
use crate::upload::UploadOp;

/// Frames an upload may wait for its texture before it is dropped and the
/// surface asked for a full repaint instead.
const MAX_WAIT_FRAMES: u32 = 120;

pub struct SurfaceUpload {
    pub image: AssetId<Image>,
    /// The texture this plan was made for.
    pub texture: UVec2,
    /// The part of it the surface shows.
    pub visible: UVec2,
    pub ops: Vec<UploadOp>,
}

/// Main world to render world hand-off, plus what the render world did.
#[derive(Default)]
pub struct Shared {
    pending: Mutex<Vec<SurfaceUpload>>,
    /// Images whose texture must be repainted in full (lost or replaced).
    repaint: Mutex<HashSet<AssetId<Image>>>,
    /// Uploads are staged for a texture that does not exist yet: the main
    /// world must keep updating until they land or are given up.
    pub waiting: AtomicBool,
    pub bytes_written: AtomicU64,
    pub rects_written: AtomicU64,
}

impl Shared {
    pub fn push(&self, upload: SurfaceUpload) {
        self.pending.lock().unwrap().push(upload);
    }
    pub fn take_repaints(&self) -> HashSet<AssetId<Image>> {
        std::mem::take(&mut *self.repaint.lock().unwrap())
    }
    pub(crate) fn request_repaint(&self, image: AssetId<Image>) {
        self.repaint.lock().unwrap().insert(image);
    }
}

#[derive(Resource, Clone, Default)]
pub struct GpuChannel(pub Arc<Shared>);

#[derive(Resource, Default)]
struct Staged {
    uploads: Vec<(SurfaceUpload, u32)>,
    /// The texture last written for each image: a different one means Bevy
    /// re-created it and the old contents are gone.
    written: HashMap<AssetId<Image>, TextureId>,
}

pub fn install(app: &mut App, channel: GpuChannel) {
    let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
        return;
    };
    render_app
        .insert_resource(channel)
        .init_resource::<Staged>()
        .add_systems(ExtractSchedule, extract)
        .add_systems(
            Render,
            write
                .in_set(RenderSystems::PrepareResources)
                .after(RenderSystems::PrepareAssets),
        );
}

fn extract(channel: Extract<Res<GpuChannel>>, mut staged: ResMut<Staged>) {
    stage(&channel.0, &mut staged);
}

fn stage(shared: &Shared, staged: &mut Staged) {
    let uploads = std::mem::take(&mut *shared.pending.lock().unwrap());
    staged.uploads.extend(uploads.into_iter().map(|u| (u, 0)));
}

fn write(
    mut staged: ResMut<Staged>,
    channel: Res<GpuChannel>,
    images: Res<RenderAssets<GpuImage>>,
    queue: Res<RenderQueue>,
) {
    let staged = &mut *staged;
    let mut waiting = Vec::new();
    for (upload, waited) in staged.uploads.drain(..) {
        let Some(gpu) = images.get(upload.image) else {
            if waited < MAX_WAIT_FRAMES {
                waiting.push((upload, waited + 1));
            } else {
                channel.0.request_repaint(upload.image);
            }
            continue;
        };
        let size = gpu.texture_descriptor.size;
        if UVec2::new(size.width, size.height) != upload.texture {
            // A stale plan for a texture that has since been replaced.
            continue;
        }
        let id = gpu.texture.id();
        let full = upload
            .ops
            .iter()
            .any(|op| op.rect == Rect::new(0, 0, upload.visible.x, upload.visible.y));
        if staged
            .written
            .insert(upload.image, id)
            .is_some_and(|old| old != id)
            && !full
        {
            channel.0.request_repaint(upload.image);
        }
        for op in &upload.ops {
            queue.write_texture(
                TexelCopyTextureInfo {
                    texture: &gpu.texture,
                    mip_level: 0,
                    origin: Origin3d {
                        x: op.rect.x,
                        y: op.rect.y,
                        z: 0,
                    },
                    aspect: TextureAspect::All,
                },
                &op.bytes,
                TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(op.rect.w * 4),
                    rows_per_image: None,
                },
                Extent3d {
                    width: op.rect.w,
                    height: op.rect.h,
                    depth_or_array_layers: 1,
                },
            );
            channel
                .0
                .bytes_written
                .fetch_add(op.bytes.len() as u64, Ordering::Relaxed);
            channel.0.rects_written.fetch_add(1, Ordering::Relaxed);
        }
    }
    channel
        .0
        .waiting
        .store(!waiting.is_empty(), Ordering::Relaxed);
    staged.uploads = waiting;
    staged.written.retain(|id, _| images.get(*id).is_some());
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::system::RunSystemOnce;
    use bevy::render::render_resource::{
        TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    };
    use bevy::render::renderer::WgpuWrapper;

    use crate::upload::extract as copy_rect;

    struct Gpu {
        world: World,
        device: wgpu::Device,
        shared: Arc<Shared>,
    }

    impl Gpu {
        fn new() -> Self {
            let (device, queue) = wgpu::Device::noop(&wgpu::DeviceDescriptor::default());
            let mut world = World::new();
            let shared = Arc::new(Shared::default());
            world.insert_resource(RenderQueue(Arc::new(WgpuWrapper::new(queue))));
            world.insert_resource(RenderAssets::<GpuImage>::default());
            world.insert_resource(GpuChannel(shared.clone()));
            world.init_resource::<Staged>();
            Self {
                world,
                device,
                shared,
            }
        }

        /// Stands in for Bevy's `prepare_assets`: a real (noop-backend)
        /// texture, validated by wgpu on every write.
        fn texture(&mut self, image: AssetId<Image>, size: UVec2) -> TextureId {
            let descriptor = TextureDescriptor {
                label: None,
                size: Extent3d {
                    width: size.x,
                    height: size.y,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format: TextureFormat::Rgba8UnormSrgb,
                usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
                view_formats: &[],
            };
            let texture = self.device.create_texture(&descriptor);
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let sampler = self
                .device
                .create_sampler(&wgpu::SamplerDescriptor::default());
            let texture: bevy::render::render_resource::Texture = texture.into();
            let id = texture.id();
            self.world.resource_mut::<RenderAssets<GpuImage>>().insert(
                image,
                GpuImage {
                    texture,
                    texture_view: view.into(),
                    sampler: sampler.into(),
                    texture_descriptor: descriptor,
                    texture_view_descriptor: None,
                    had_data: false,
                },
            );
            id
        }

        fn frame(&mut self) {
            let world = &mut self.world;
            world.resource_scope(|_, mut staged: Mut<Staged>| stage(&self.shared, &mut staged));
            world.run_system_once(write).unwrap();
        }

        fn written(&self) -> (u64, u64) {
            (
                self.shared.bytes_written.swap(0, Ordering::Relaxed),
                self.shared.rects_written.swap(0, Ordering::Relaxed),
            )
        }
    }

    fn image(n: u32) -> AssetId<Image> {
        let mut assets = Assets::<Image>::default();
        let mut id = None;
        for _ in 0..=n {
            id = Some(assets.add(Image::default()).id());
        }
        id.unwrap()
    }

    fn upload(
        image: AssetId<Image>,
        texture: UVec2,
        visible: UVec2,
        rects: &[Rect],
    ) -> SurfaceUpload {
        let buffer = vec![0x80u8; (texture.x * texture.y * 4) as usize];
        SurfaceUpload {
            image,
            texture,
            visible,
            ops: rects
                .iter()
                .map(|rect| copy_rect(&buffer, texture.x * 4, *rect))
                .collect(),
        }
    }

    #[test]
    fn damage_rects_are_written_into_the_bucketed_texture() {
        let mut gpu = Gpu::new();
        let a = image(0);
        let tex = UVec2::new(256, 128);
        gpu.texture(a, tex);
        let rects = [Rect::new(0, 0, 200, 100), Rect::new(250, 120, 6, 8)];
        gpu.shared
            .push(upload(a, tex, UVec2::new(200, 100), &rects));
        gpu.frame();
        assert_eq!(gpu.written(), (200 * 100 * 4 + 6 * 8 * 4, 2));
        assert!(!gpu.shared.waiting.load(Ordering::Relaxed));
        assert!(gpu.shared.take_repaints().is_empty());
        // Nothing staged: nothing written.
        gpu.frame();
        assert_eq!(gpu.written(), (0, 0));
    }

    #[test]
    fn uploads_wait_for_their_texture_then_give_up_with_a_repaint() {
        let mut gpu = Gpu::new();
        let (a, b) = (image(0), image(1));
        let tex = UVec2::new(128, 128);
        let full = [Rect::new(0, 0, 64, 64)];
        gpu.shared.push(upload(a, tex, UVec2::splat(64), &full));
        gpu.frame();
        assert_eq!(gpu.written(), (0, 0));
        assert!(
            gpu.shared.waiting.load(Ordering::Relaxed),
            "host must keep updating"
        );
        // The texture appears: the staged upload lands.
        gpu.texture(a, tex);
        gpu.frame();
        assert_eq!(gpu.written(), (64 * 64 * 4, 1));
        assert!(!gpu.shared.waiting.load(Ordering::Relaxed));

        // One that never gets a texture is dropped after the wait budget.
        gpu.shared.push(upload(b, tex, UVec2::splat(64), &full));
        for _ in 0..=MAX_WAIT_FRAMES {
            gpu.frame();
        }
        assert_eq!(gpu.written(), (0, 0));
        assert!(!gpu.shared.waiting.load(Ordering::Relaxed));
        assert_eq!(gpu.shared.take_repaints(), HashSet::from([b]));
    }

    #[test]
    fn stale_plans_are_dropped_and_replaced_textures_ask_for_a_repaint() {
        let mut gpu = Gpu::new();
        let a = image(0);
        let tex = UVec2::new(128, 128);
        let first = gpu.texture(a, tex);
        let partial = [Rect::new(4, 4, 8, 8)];
        gpu.shared.push(upload(a, tex, UVec2::splat(100), &partial));
        gpu.frame();
        assert_eq!(gpu.written(), (8 * 8 * 4, 1));

        // A plan for a different texture size is not written.
        gpu.shared
            .push(upload(a, UVec2::new(256, 128), UVec2::splat(100), &partial));
        gpu.frame();
        assert_eq!(gpu.written(), (0, 0));

        // Bevy re-created the texture: a partial write asks for a full repaint.
        let second = gpu.texture(a, tex);
        assert_ne!(first, second);
        gpu.shared.push(upload(a, tex, UVec2::splat(100), &partial));
        gpu.frame();
        assert_eq!(gpu.shared.take_repaints(), HashSet::from([a]));
        // A full repaint of the visible part does not.
        gpu.texture(a, tex);
        let full = [Rect::new(0, 0, 100, 100)];
        gpu.shared.push(upload(a, tex, UVec2::splat(100), &full));
        gpu.frame();
        assert!(gpu.shared.take_repaints().is_empty());
        assert_eq!(gpu.written(), (8 * 8 * 4 + 100 * 100 * 4, 2));
    }
}
