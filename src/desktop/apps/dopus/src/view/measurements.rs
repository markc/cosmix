//! App-owned measurement caches; no font shaping for unchanged view inputs.
use super::{Look, alignment::FirstRow, elide};
use cosmix_dopus_core::PaneId;
use iced::advanced::text::Paragraph as _;

#[derive(Clone)]
pub struct Footer {
    pub text: String,
    pub width: f32,
}

#[derive(Default)]
pub struct Measurements {
    first_row: Option<(Look, FirstRow)>,
    footers: [Option<(Look, Footer)>; 2],
}

impl Measurements {
    pub fn first_row(&mut self, look: Look) -> FirstRow {
        self.first_row_with(look, FirstRow::new)
    }

    fn first_row_with(&mut self, look: Look, measure: impl FnOnce(Look) -> FirstRow) -> FirstRow {
        if let Some((key, value)) = self.first_row
            && key == look
        {
            return value;
        }
        let value = measure(look);
        self.first_row = Some((look, value));
        value
    }

    pub fn footer(&mut self, look: Look, pane: PaneId, text: String) -> Footer {
        self.footer_with(look, pane, text, |text| {
            elide::shape(text, look.ui_font, look.small_px)
                .min_bounds()
                .width
                + 2.0 * look.chrome.pad
        })
    }

    fn footer_with(
        &mut self,
        look: Look,
        pane: PaneId,
        text: String,
        measure: impl FnOnce(&str) -> f32,
    ) -> Footer {
        let slot = &mut self.footers[pane.index()];
        if let Some((key, value)) = slot
            && *key == look
            && value.text == text
        {
            return value.clone();
        }
        let value = Footer {
            width: measure(&text),
            text,
        };
        *slot = Some((look, value.clone()));
        value
    }

    #[cfg(test)]
    pub(super) fn assert_cache_invalidation(look: Look) {
        let mut cache = Self::default();
        cache.first_row(look);
        cache.first_row_with(look, |_| panic!("unchanged geometry reshaped"));
        cache.footer_with(look, PaneId::Left, "one".into(), |_| 10.0);
        cache.footer_with(look, PaneId::Right, "two".into(), |_| 20.0);
        cache.footer_with(look, PaneId::Left, "one".into(), |_| {
            panic!("unchanged footer reshaped")
        });
        assert_eq!(
            cache
                .footer_with(look, PaneId::Left, "new".into(), |_| 30.0)
                .width,
            30.0
        );
        let mut changed = look;
        changed.small_px += 1.0;
        let mut measured = false;
        cache.first_row_with(changed, |look| {
            measured = true;
            FirstRow::new(look)
        });
        assert!(measured);
        assert_eq!(
            cache
                .footer_with(changed, PaneId::Left, "new".into(), |_| 40.0)
                .width,
            40.0
        );
    }
}
