use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

use bevy::asset::RenderAssetUsages;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::window::PrimaryWindow;
use resvg::{tiny_skia, usvg};

const MAX_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Resource, Default)]
pub(super) struct IconCache {
    images: BTreeMap<(String, u32, u32), Handle<Image>>,
    failed: BTreeSet<String>,
}

pub(super) fn load(world: &mut World, path: &str, w: f32, h: f32) -> Option<Handle<Image>> {
    let factor = world
        .query_filtered::<&Window, With<PrimaryWindow>>()
        .iter(world)
        .next()
        .map_or(3.0, Window::scale_factor);
    let w = (w * factor).ceil();
    let h = (h * factor).ceil();
    world.init_resource::<IconCache>();
    world.resource_scope(|world, mut cache: Mut<IconCache>| {
        if cache.failed.contains(path) {
            return None;
        }
        let result: Result<Handle<Image>, String> = (|| {
            if !w.is_finite() || !h.is_finite() || w <= 0.0 || h <= 0.0 || w > 1024.0 || h > 1024.0
            {
                return Err("invalid icon target (must be 1..=1024 pixels per side)".into());
            }
            let key = (path.to_owned(), w as u32, h as u32);
            if let Some(handle) = cache.images.get(&key) {
                return Ok(handle.clone());
            }
            let image = decode(path, key.1, key.2)?;
            world.init_resource::<Assets<Image>>();
            let handle = world.resource_mut::<Assets<Image>>().add(image);
            cache.images.insert(key, handle.clone());
            Ok(handle)
        })();
        match result {
            Ok(handle) => Some(handle),
            Err(error) => {
                warn!("scene icon {path}: {error}");
                cache.failed.insert(path.to_owned());
                None
            }
        }
    })
}

fn decode(path: &str, w: u32, h: u32) -> Result<Image, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    if file.metadata().map_err(|e| e.to_string())?.len() > MAX_BYTES {
        return Err("icon file exceeds 4 MiB".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("icon file exceeds 4 MiB".into());
    }
    let rgba = if std::path::Path::new(path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"))
    {
        let tree =
            usvg::Tree::from_data(&bytes, &usvg::Options::default()).map_err(|e| e.to_string())?;
        let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or("invalid icon size")?;
        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(
                w as f32 / tree.size().width(),
                h as f32 / tree.size().height(),
            ),
            &mut pixmap.as_mut(),
        );
        // tiny-skia stores premultiplied alpha; Bevy UI expects straight RGBA.
        pixmap
            .pixels()
            .iter()
            .flat_map(|pixel| {
                let color = pixel.demultiply();
                [color.red(), color.green(), color.blue(), color.alpha()]
            })
            .collect()
    } else {
        image::ImageReader::new(std::io::Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| e.to_string())?
            .decode()
            .map_err(|e| e.to_string())?
            .resize_exact(w, h, image::imageops::FilterType::Lanczos3)
            .to_rgba8()
            .into_raw()
    };
    Ok(Image::new(
        Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        rgba,
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
    ))
}
