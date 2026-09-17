//! Glue: one commit for grid and chrome damage.

use cosmix_iced_host::DamageRect;
use cosmix_wl_app::{Damage, Rect};

#[derive(Debug, Clone, PartialEq)]
pub enum Commit {
    Nothing,
    Full,
    Rects(Vec<Rect>),
}

/// Merge the grid's damage (`grid_full` means the whole buffer was
/// repainted) with the chrome's rectangles, which are in the same buffer
/// coordinates (the chrome band starts at the top).
pub fn merge(size: (u32, u32), grid_full: bool, grid: &[Rect], chrome: &[DamageRect]) -> Commit {
    if grid_full {
        return Commit::Full;
    }
    let mut damage = Damage::new(size.0, size.1);
    damage.extend(grid);
    for r in chrome {
        damage.add(Rect::new(
            r.x as i32,
            r.y as i32,
            r.width as i32,
            r.height as i32,
        ));
    }
    match damage.rects() {
        [] => Commit::Nothing,
        _ if damage.is_full() => Commit::Full,
        rects => Commit::Rects(rects.to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dr(x: u32, y: u32, width: u32, height: u32) -> DamageRect {
        DamageRect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn nothing_full_and_merged() {
        assert_eq!(merge((100, 100), false, &[], &[]), Commit::Nothing);
        assert_eq!(
            merge((100, 100), true, &[], &[dr(0, 0, 5, 5)]),
            Commit::Full
        );
        // A caret blink in the chrome plus a typed cell in the grid: two
        // rectangles, one commit.
        let got = merge(
            (1000, 800),
            false,
            &[Rect::new(20, 300, 9, 18)],
            &[dr(700, 10, 2, 16)],
        );
        assert_eq!(
            got,
            Commit::Rects(vec![Rect::new(20, 300, 9, 18), Rect::new(700, 10, 2, 16)])
        );
    }

    #[test]
    fn touching_grid_and_chrome_rects_merge_and_clip() {
        let got = merge(
            (100, 100),
            false,
            &[Rect::new(0, 20, 10, 10)],
            &[dr(0, 10, 10, 10), dr(95, 95, 20, 20)],
        );
        let Commit::Rects(mut rects) = got else {
            panic!("expected rects");
        };
        rects.sort_by_key(|r| r.x);
        assert_eq!(
            rects,
            vec![Rect::new(0, 10, 10, 20), Rect::new(95, 95, 5, 5)]
        );
        // Chrome repainting its whole band plus a full-width grid band that
        // together cover the buffer collapse to a full commit.
        assert_eq!(
            merge(
                (100, 100),
                false,
                &[Rect::new(0, 40, 100, 60)],
                &[dr(0, 0, 100, 40)]
            ),
            Commit::Full
        );
    }
}
