//! Render-world half: partial `write_texture` into the GPU image that Bevy
//! created once for each surface. The image asset itself is never mutated.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
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
    let uploads = std::mem::take(&mut *channel.0.pending.lock().unwrap());
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
                channel.0.repaint.lock().unwrap().insert(upload.image);
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
            channel.0.repaint.lock().unwrap().insert(upload.image);
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
    staged.uploads = waiting;
    staged.written.retain(|id, _| images.get(*id).is_some());
}
