use super::*;

/// Four terminal rows bound retained-buffer copies on echo (damage is per cell),
/// without creating a widget for every physical scanline.
const ROWS_PER_BAND: usize = 4;

#[derive(Default)]
pub struct Surface {
    pub(super) tiles: Vec<PixelBand>,
    bands: Vec<DamageBand>,
    width: u32,
    height: u32,
    cell: (u32, u32),
}

impl Surface {
    #[cfg(test)]
    pub fn width(&self) -> u32 {
        self.width
    }
    #[cfg(test)]
    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub fn invalidate(&mut self) {
        for tile in &mut self.tiles {
            tile.invalidate();
        }
    }

    #[cfg(test)]
    // Preserve the shared Frame tests' RGBA oracle; runtime has no readback.
    pub fn rgba(&self) -> Vec<u8> {
        self.tiles
            .iter()
            .flat_map(|tile| {
                tile.native
                    .chunks_exact(4)
                    .flat_map(|p| [p[2], p[1], p[0], p[3]])
            })
            .collect()
    }

    #[cfg(test)]
    pub fn allocation(&self) -> *const u8 {
        self.tiles
            .first()
            .map_or(std::ptr::null(), |tile| tile.native.as_ptr())
    }

    pub fn paint(&mut self, raster: &mut Raster, screen: &Screen, dirty: &[bool]) -> &[DamageBand] {
        self.bands.clear();
        let (width, height) = raster.target_size(screen);
        let rows = height as usize / raster.height as usize;
        if (self.width, self.height, self.cell) != (width, height, (raster.width, raster.height)) {
            self.invalidate();
        }
        self.width = width;
        self.height = height;
        self.cell = (raster.width, raster.height);
        self.tiles
            .resize_with(rows.div_ceil(ROWS_PER_BAND), PixelBand::default);
        let valid_damage = rows == screen.rows && dirty.len() == rows;
        for (index, tile) in self.tiles.iter_mut().enumerate() {
            let first = index * ROWS_PER_BAND;
            let end = (first + ROWS_PER_BAND).min(rows);
            // Keep the original cell columns and translate only the row and
            // cursor origin. Core PaintState then erases the old cursor even
            // when it moves to another band or disappears on a resize.
            let part = Screen {
                clusters: screen.clusters.clone(),
                cols: screen.cols,
                rows: end - first,
                cursor: (screen.cursor.0, screen.cursor.1.saturating_sub(first)),
                cursor_visible: screen.cursor_visible && (first..end).contains(&screen.cursor.1),
                display_offset: screen.display_offset,
                cells: screen.cells[first * screen.cols..end * screen.cols].to_vec(),
                updated: screen.updated,
            };
            let dirty = if valid_damage {
                &dirty[first..end]
            } else {
                &[]
            };
            for damage in tile.paint_inner(raster, &part, dirty) {
                self.bands.push(DamageBand {
                    x: damage.x,
                    width: damage.width,
                    y: first as u32 * raster.height + damage.y,
                    height: damage.height,
                });
            }
        }
        &self.bands
    }

    pub fn cache_handle(&mut self, generation: u64) {
        for tile in &mut self.tiles {
            // paint releases the handle only for a potentially changed band.
            // Unchanged bands MUST keep their id across pane generations.
            if tile.cached.is_none() {
                tile.cache_handle(generation);
            }
        }
    }

    pub(super) fn images(&self, scale: f32) -> Vec<(Handle, iced::Rectangle)> {
        let mut y = 0;
        self.tiles
            .iter()
            .filter_map(|tile| {
                let top = y as f32 / scale;
                y += tile.height;
                let bottom = y as f32 / scale;
                let bounds = iced::Rectangle {
                    x: 0.0,
                    y: top,
                    width: tile.width as f32 / scale,
                    height: bottom - top,
                };
                tile.cached
                    .as_ref()
                    .map(|(_, handle)| (handle.clone(), bounds))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_term_core::{config::Cursor, terminal::Cell};
    use std::time::Instant;

    #[test]
    fn unicode_band_snapshots_retain_text_and_expand_spacer_damage() {
        let mut raster = Raster::new(1.25, 13.0, Cursor::Block).unwrap();
        let mut surface = Surface::default();
        let mut screen = cosmix_term_core::terminal::Terminal::from_test_vt(
            8,
            8,
            "\x1b[4;2H👍🏽👍🏻\x1b[5;2H👩‍💻".as_bytes(),
        )
        .screen(false);
        screen.cursor_visible = false;
        surface.paint(&mut raster, &screen, &[]);
        surface.cache_handle(1);
        let retained = surface.images(1.25);
        let old = surface.rgba();
        let old_handles: Vec<_> = retained.iter().map(|(h, _)| h.pixels().to_vec()).collect();
        screen.cells[3 * 8 + 1].extra = screen.cells[3 * 8 + 3].extra;
        screen.cells[3 * 8 + 2].bg = [1, 51, 91];
        let mut dirty = [false; 8];
        dirty[3] = true;
        assert_eq!(
            surface.paint(&mut raster, &screen, &dirty),
            &[DamageBand {
                x: raster.width,
                y: 3 * raster.height,
                width: 2 * raster.width,
                height: raster.height,
            }]
        );
        surface.cache_handle(2);
        assert_ne!(surface.rgba(), old);
        assert_eq!(surface.rgba(), raster.render(&screen));
        for ((handle, _), bytes) in retained.iter().zip(old_handles) {
            assert_eq!(&handle.pixels()[..], bytes.as_slice());
        }
        screen.cursor = (2, 3);
        screen.cursor_visible = true;
        surface.paint(&mut raster, &screen, &[false; 8]);
        screen.cursor = (2, 4);
        surface.paint(&mut raster, &screen, &[false; 8]);
        assert_eq!(surface.rgba(), raster.render(&screen));
    }

    #[test]
    fn bands_preserve_pixels_ids_and_cursor_damage_across_boundaries() {
        let mut raster = Raster::new(2.5, 13.0, Cursor::Block).unwrap();
        let mut surface = Surface::default();
        let mut screen = Screen {
            clusters: Default::default(),
            cols: 13,
            rows: 11,
            cursor: (2, 3),
            cursor_visible: true,
            display_offset: 0,
            cells: vec![
                Cell {
                    extra: 0,
                    width: Default::default(),
                    c: 'M',
                    fg: [200, 210, 220],
                    bg: [10, 20, 30],
                    bold: false
                };
                143
            ],
            updated: Instant::now(),
        };
        surface.paint(&mut raster, &screen, &[]);
        surface.cache_handle(1);
        let retained = surface.images(2.5);
        let before = surface.rgba();
        screen.cursor.1 = 4;
        surface.paint(&mut raster, &screen, &[false; 11]);
        surface.cache_handle(2);
        let next = surface.images(2.5);
        assert_ne!(next[0].0.generation(), retained[0].0.generation());
        assert_ne!(next[1].0.generation(), retained[1].0.generation());
        assert_eq!(next[2].0.generation(), retained[2].0.generation());
        let old: Vec<u8> = retained
            .iter()
            .flat_map(|(handle, _)| {
                handle
                    .pixels()
                    .chunks_exact(4)
                    .flat_map(|p| [p[2], p[1], p[0], p[3]])
            })
            .collect();
        assert_eq!(before, old, "retained image pixels were mutated");
        assert_eq!(surface.rgba(), raster.render(&screen));

        // Several paints before handle refresh must not lose earlier changes.
        screen.cursor_visible = false;
        for row in [0, 7, 10] {
            screen.cells[row * 13].bg = [80, 90, 100];
            let mut dirty = [false; 11];
            dirty[row] = true;
            surface.paint(&mut raster, &screen, &dirty);
        }
        surface.cache_handle(3);
        assert_eq!(surface.rgba(), raster.render(&screen));
        let stable = surface.images(2.5);
        assert!(surface.paint(&mut raster, &screen, &[false; 11]).is_empty());
        surface.cache_handle(4);
        assert_eq!(
            stable.iter().map(|p| p.0.generation()).collect::<Vec<_>>(),
            surface
                .images(2.5)
                .iter()
                .map(|p| p.0.generation())
                .collect::<Vec<_>>()
        );

        // Malformed snapshots, shrink, column changes and invalidation all
        // owe complete valid pixels, including the final partial band.
        screen.cells.truncate(13 * 6);
        surface.paint(&mut raster, &screen, &[false; 11]);
        assert_eq!(surface.rgba(), raster.render(&screen));
        screen.cols = 6;
        surface.paint(&mut raster, &screen, &[false; 11]);
        assert_eq!(surface.rgba(), raster.render(&screen));
        surface.invalidate();
        surface.paint(&mut raster, &screen, &[false; 11]);
        assert_eq!(surface.rgba(), raster.render(&screen));
        screen.cols = 0;
        surface.paint(&mut raster, &screen, &[]);
        surface.cache_handle(5);
        assert!(surface.is_empty());
        assert!(surface.images(2.5).is_empty());
        screen.rows = 0;
        surface.paint(&mut raster, &screen, &[]);
        surface.cache_handle(5);
        assert!(surface.images(2.5).is_empty());
    }
}
