use std::collections::VecDeque;
use std::io::Read;
use std::time::SystemTime;

use bevy::asset::RenderAssetUsages;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::window::PrimaryWindow;
use resvg::{tiny_skia, usvg};

const MAX_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ENTRIES: usize = 512;

#[derive(PartialEq)]
struct Key {
    path: String,
    width: u32,
    height: u32,
    scale: u32,
    stamp: Option<(SystemTime, u64)>,
}

enum Cached {
    Image(Handle<Image>),
    Persistent,
    Missing,
    // Retain only the diagnostic marker, never suppress an I/O retry.
    Transient,
}

#[derive(Resource, Default)]
pub(super) struct IconCache {
    entries: VecDeque<(Key, Cached)>,
}

pub(super) fn begin_revision(world: &mut World) {
    world.init_resource::<IconCache>();
    world
        .resource_mut::<IconCache>()
        .entries
        .retain(|(_, entry)| !matches!(entry, Cached::Missing));
}

pub(super) fn effective_scale(world: &mut World) -> f32 {
    let ui = world.get_resource::<UiScale>().map_or(1.0, |scale| scale.0);
    ui * world
        .query_filtered::<&Window, With<PrimaryWindow>>()
        .iter(world)
        .next()
        .map_or(1.0, Window::scale_factor)
}

pub(super) fn load(world: &mut World, path: &str, w: f32, h: f32) -> Option<Handle<Image>> {
    let factor = effective_scale(world);
    let w = (w * factor).ceil();
    let h = (h * factor).ceil();
    world.init_resource::<IconCache>();
    world.resource_scope(|world, mut cache: Mut<IconCache>| {
        let mut key = Key {
            path: path.into(),
            width: w.to_bits(),
            height: h.to_bits(),
            scale: factor.to_bits(),
            stamp: None,
        };
        if let Some(index) = cache
            .entries
            .iter()
            .position(|(k, v)| k == &key && matches!(v, Cached::Missing))
        {
            let entry = cache.entries.remove(index).unwrap();
            cache.entries.push_back(entry);
            return None;
        }
        // Follow symlinks, but inspect the target before opening (FIFOs/devices
        // must never be opened by the renderer).
        let metadata = std::fs::metadata(path);
        if let Ok(meta) = &metadata {
            key.stamp = meta.modified().ok().map(|mtime| (mtime, meta.len()));
        }
        let mut logged = false;
        if let Some(index) = cache.entries.iter().position(|(k, _)| k == &key) {
            let entry = cache.entries.remove(index).unwrap();
            if !matches!(entry.1, Cached::Transient) {
                let result = match &entry.1 {
                    Cached::Image(handle) => Some(handle.clone()),
                    _ => None,
                };
                cache.entries.push_back(entry);
                return result;
            }
            logged = true;
        }
        let result = (|| {
            if !w.is_finite() || !h.is_finite() || w <= 0.0 || h <= 0.0 || w > 1024.0 || h > 1024.0
            {
                return Err(Failure::Persistent(
                    "invalid icon target (must be 1..=1024 pixels per side)".into(),
                ));
            }
            let metadata = metadata.map_err(Failure::Io)?;
            let image = decode(path, w as u32, h as u32, &metadata)?;
            world.init_resource::<Assets<Image>>();
            Ok(world.resource_mut::<Assets<Image>>().add(image))
        })();
        let (entry, handle) = match result {
            Ok(handle) => (Cached::Image(handle.clone()), Some(handle)),
            Err(error) => {
                let (entry, message) = match error {
                    Failure::Persistent(message) => (Cached::Persistent, message),
                    Failure::Io(error) => (
                        if error.kind() == std::io::ErrorKind::NotFound {
                            Cached::Missing
                        } else {
                            Cached::Transient
                        },
                        error.to_string(),
                    ),
                };
                if !logged {
                    warn!("scene icon {path}: {message}");
                }
                (entry, None)
            }
        };
        cache.entries.push_back((key, entry));
        while cache.entries.len() > MAX_ENTRIES {
            cache.entries.pop_front();
        }
        handle
    })
}

#[derive(Debug)]
enum Failure {
    Persistent(String),
    Io(std::io::Error),
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Persistent(message)
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Self::Persistent(message.into())
    }
}

fn decode(path: &str, w: u32, h: u32, metadata: &std::fs::Metadata) -> Result<Image, Failure> {
    if !metadata.is_file() {
        return Err("icon is not a regular file".into());
    }
    if metadata.len() > MAX_BYTES {
        return Err("icon file exceeds 4 MiB".into());
    }
    let file = std::fs::File::open(path).map_err(Failure::Io)?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(Failure::Io)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("icon file exceeds 4 MiB".into());
    }
    let rgba = if std::path::Path::new(path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"))
    {
        let options = usvg::Options {
            image_href_resolver: usvg::ImageHrefResolver {
                resolve_data: Box::new(|_, _, _| None),
                resolve_string: Box::new(|_, _| None),
            },
            ..Default::default()
        };
        let tree = usvg::Tree::from_data(&bytes, &options).map_err(|e| e.to_string())?;
        let mut pixmap = tiny_skia::Pixmap::new(w, h).ok_or("invalid icon size")?;
        let transform = svg_transform(tree.size().width(), tree.size().height(), w, h)?;
        // resvg 0.47 render returns (), not a fallible result. Validate its
        // size/transform inputs above; transparent SVGs are valid images.
        resvg::render(&tree, transform, &mut pixmap.as_mut());
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
        let mut reader =
            image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Png);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(4096);
        limits.max_image_height = Some(4096);
        limits.max_alloc = Some(64 * 1024 * 1024);
        reader.limits(limits);
        reader
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

fn svg_transform(width: f32, height: f32, w: u32, h: u32) -> Result<tiny_skia::Transform, Failure> {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return Err("invalid SVG dimensions".into());
    }
    let scale = (w as f32 / width).min(h as f32 / height);
    if !scale.is_finite() || scale <= 0.0 {
        return Err("invalid SVG scale".into());
    }
    Ok(tiny_skia::Transform::from_row(
        scale,
        0.0,
        0.0,
        scale,
        (w as f32 - width * scale) / 2.0,
        (h as f32 - height * scale) / 2.0,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10"><path fill="#f00" d="M0 0H20V10H0Z"/></svg>"##;

    #[test]
    fn non_regular_icons_are_rejected_before_open() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("device.png");
        std::os::unix::fs::symlink("/dev/zero", &link).unwrap();
        for path in [dir.path(), link.as_path()] {
            assert!(
                matches!(decode(path.to_str().unwrap(), 24, 24, &std::fs::metadata(path).unwrap()),
                Err(Failure::Persistent(message)) if message == "icon is not a regular file")
            );
        }
    }

    #[test]
    fn svg_external_image_dev_zero_is_never_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("external.svg");
        let source = SVG.replace("</svg>", r#"<image xmlns:xlink="http://www.w3.org/1999/xlink" xlink:href="/dev/zero" width="20" height="10"/></svg>"#);
        std::fs::write(&path, source).unwrap();
        let start = std::time::Instant::now();
        let image = decode(
            path.to_str().unwrap(),
            20,
            20,
            &std::fs::metadata(&path).unwrap(),
        )
        .unwrap();
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        assert!(
            image
                .data
                .unwrap()
                .chunks_exact(4)
                .any(|pixel| pixel == [255, 0, 0, 255])
        );
    }

    #[test]
    fn svg_preserves_aspect_ratio_and_centres() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wide.svg");
        std::fs::write(&path, SVG).unwrap();
        let image = decode(
            path.to_str().unwrap(),
            20,
            20,
            &std::fs::metadata(&path).unwrap(),
        )
        .unwrap();
        for (index, pixel) in image.data.unwrap().chunks_exact(4).enumerate() {
            assert_eq!(
                pixel[3],
                if (5..15).contains(&(index / 20)) {
                    255
                } else {
                    0
                }
            );
        }
        for size in [0.0, -1.0, f32::INFINITY, f32::NAN] {
            assert!(svg_transform(size, 10.0, 24, 24).is_err());
            assert!(svg_transform(10.0, size, 24, 24).is_err());
        }
    }

    #[test]
    fn png_header_declaring_20000_square_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.png");
        image::RgbaImage::new(1, 1).save(&path).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[16..20].copy_from_slice(&20000u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&20000u32.to_be_bytes());
        // Recompute the IHDR CRC so this proves the dimension limit rather
        // than failing on an invalid checksum.
        let mut crc = u32::MAX;
        for byte in &bytes[12..29] {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ if crc & 1 == 1 { 0xedb88320 } else { 0 };
            }
        }
        bytes[29..33].copy_from_slice(&(!crc).to_be_bytes());
        std::fs::write(&path, bytes).unwrap();
        let result = decode(
            path.to_str().unwrap(),
            24,
            24,
            &std::fs::metadata(&path).unwrap(),
        );
        assert!(
            matches!(result, Err(Failure::Persistent(message)) if message.contains("limit")),
            "PNG must fail on decoder limits"
        );
    }

    #[test]
    fn icon_cache_is_bounded_and_lru() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("icon.svg");
        std::fs::write(&path, SVG).unwrap();
        let path = path.to_str().unwrap();
        let mut world = World::new();
        let first = load(&mut world, path, 1.0, 1.0).unwrap();
        let second = load(&mut world, path, 1.0, 2.0).unwrap();
        for height in 3..=MAX_ENTRIES {
            load(&mut world, path, 1.0, height as f32).unwrap();
        }
        assert_eq!(load(&mut world, path, 1.0, 1.0), Some(first.clone()));
        load(&mut world, path, 1.0, 513.0).unwrap();
        assert_eq!(world.resource::<IconCache>().entries.len(), MAX_ENTRIES);
        assert_eq!(load(&mut world, path, 1.0, 1.0), Some(first));
        assert_ne!(load(&mut world, path, 1.0, 2.0), Some(second));
        for height in 0..600 {
            assert!(load(&mut world, path, 0.0, height as f32).is_none());
        }
        assert_eq!(world.resource::<IconCache>().entries.len(), MAX_ENTRIES);
    }

    #[test]
    fn transient_failure_marker_does_not_suppress_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("icon.svg");
        std::fs::write(&path, SVG).unwrap();
        let mut world = World::new();
        load(&mut world, path.to_str().unwrap(), 24.0, 24.0).unwrap();
        // Exercise the same marker retained after an open/read I/O failure.
        world.resource_mut::<IconCache>().entries[0].1 = Cached::Transient;
        assert!(load(&mut world, path.to_str().unwrap(), 24.0, 24.0).is_some());
        assert!(matches!(
            world.resource::<IconCache>().entries[0].1,
            Cached::Image(_)
        ));
    }

    #[test]
    fn icon_mtime_and_effective_scale_invalidate_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("icon.svg");
        std::fs::write(&path, SVG).unwrap();
        let mut world = World::new();
        world.insert_resource(UiScale(2.0));
        let first = load(&mut world, path.to_str().unwrap(), 10.0, 10.0).unwrap();
        assert_eq!(
            world
                .resource::<Assets<Image>>()
                .get(&first)
                .unwrap()
                .width(),
            20
        );
        let stamp = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(stamp + std::time::Duration::from_secs(2))
            .unwrap();
        let updated = load(&mut world, path.to_str().unwrap(), 10.0, 10.0).unwrap();
        assert_ne!(first, updated);
        let mut window = Window::default();
        window.resolution.set_scale_factor_override(Some(1.5));
        world.spawn((window, PrimaryWindow));
        assert_eq!(effective_scale(&mut world), 3.0);
        let scaled = load(&mut world, path.to_str().unwrap(), 10.0, 10.0).unwrap();
        assert_ne!(scaled, updated);
        assert_eq!(
            world
                .resource::<Assets<Image>>()
                .get(&scaled)
                .unwrap()
                .width(),
            30
        );
    }
}
