//! Test-only pre-rank-3 transport, retained as the pixel/performance oracle.
//! Keep allocation/reclaim and partial painting equivalent to the old PixelBand.
use super::*;
use iced::widget::image::Handle;

#[derive(Default)]
pub(super) struct RgbaBand {
    state: PaintState,
    pub rgba: Bytes,
    pub width: u32,
    pub height: u32,
    cell: (u32, u32),
    cursor: bool,
    bands: Vec<DamageBand>,
    pub cached: Option<(u64, Handle)>,
}

impl RgbaBand {
    pub fn paint(&mut self, raster: &mut Raster, screen: &Screen, dirty: &[bool]) {
        self.bands.clear();
        let (width, height) = raster.target_size(screen);
        if width == 0 || height == 0 {
            *self = Self::default();
            return;
        }
        let rows = height as usize / raster.height as usize;
        let cursor = screen.cursor_visible && screen.cursor.1 < rows;
        if self.state.grid() == (screen.cols, rows)
            && self.cell == (raster.width, raster.height)
            && (self.width, self.height) == (width, height)
            && screen.rows == rows
            && dirty.len() == rows
            && !dirty.iter().any(|row| *row)
            && !self.cursor
            && !cursor
        {
            return;
        }
        self.cached = None;
        let mut pixels = match std::mem::take(&mut self.rgba).try_into_mut() {
            Ok(pixels) => pixels,
            Err(shared) => {
                let pixels = BytesMut::from(shared.as_ref());
                self.state.rebind(&pixels);
                pixels
            }
        };
        let len = width as usize * height as usize * 4;
        if pixels.len() != len {
            pixels.resize(len, 0);
            self.state.invalidate();
        }
        self.bands.extend_from_slice(raster.paint(
            screen,
            &mut pixels,
            width as usize * 4,
            &mut self.state,
            dirty,
        ));
        self.rgba = pixels.freeze();
        self.width = width;
        self.height = height;
        self.cell = (raster.width, raster.height);
        self.cursor = cursor && screen.cursor.0 < screen.cols;
    }

    pub fn cache_handle(&mut self, generation: u64) {
        if self.rgba.is_empty() {
            self.cached = None;
        } else if self.cached.as_ref().map(|(current, _)| *current) != Some(generation) {
            self.cached = Some((
                generation,
                Handle::from_rgba(self.width, self.height, self.rgba.clone()),
            ));
        }
    }
}
