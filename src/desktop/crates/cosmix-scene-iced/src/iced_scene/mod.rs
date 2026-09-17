//! The iced renderer for scene surfaces (feature `iced`).
//!
//! [`IcedRendererPlugin`] replaces the stand-in factory with
//! [`IcedSceneRenderer`], keeps the scenes' look in step with CTK's design
//! and typography, and sends handler calls through CTK's
//! `SceneEvents`, the path CTK-mounted scenes use.

pub mod program;
pub mod renderer;
mod submit;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use bevy::prelude::*;
use cosmix_iced_host::core::Font;
use cosmix_iced_host::core::font::Family;
use cosmix_iced_widgets::Tokens;
use cosmix_scene_bevy::SceneEvents;
use ctk::bus::BusBridge;
use ctk::prelude::CtkDesign;
use ctk::theme::CtkTypography;

pub use program::{Look, Msg, Outbox, SceneAction, SceneProgram};
pub use renderer::{DesignShare, IcedSceneRenderer, SharedDesign};

use crate::bridge::SceneIcedFactory;

/// CTK's body size (`ctk::theme` default) when no typography resource exists.
pub const DEFAULT_TEXT_PX: f32 = 15.333;

/// Handler calls produced by iced surfaces, drained each update.
#[derive(Resource, Clone, Default)]
pub struct IcedOutbox(pub Outbox);

/// The look shared with every iced renderer.
#[derive(Resource, Clone)]
pub struct IcedDesign(pub SharedDesign);

pub struct IcedRendererPlugin;

impl Plugin for IcedRendererPlugin {
    fn build(&self, app: &mut App) {
        let outbox = IcedOutbox::default();
        let design = IcedDesign(Arc::new(RwLock::new(DesignShare {
            revision: 0,
            look: default_look(),
        })));
        let factory_outbox = outbox.0.clone();
        let factory_design = design.0.clone();
        app.insert_non_send(SceneIcedFactory(Box::new(move |_| {
            Box::new(IcedSceneRenderer::new(
                factory_design.clone(),
                factory_outbox.clone(),
            ))
        })));
        app.insert_resource(outbox)
            .insert_resource(design)
            .init_resource::<SceneEvents>()
            // Before mounts, so a new renderer starts with the current look.
            .add_systems(
                Update,
                (sync_look.before(crate::bridge::reconcile), send_actions),
            );
    }
}

/// The compiled embedded default design, as CTK starts with, and the
/// default body size.
pub fn default_look() -> Look {
    let tokens = default_tokens().unwrap_or_default();
    Look {
        tokens,
        font: Font::DEFAULT,
        text_px: DEFAULT_TEXT_PX,
    }
}

fn default_tokens() -> Option<Tokens> {
    let identity = cosmix_design::SourceIdentity::new("ctk:embedded-default");
    let document =
        cosmix_design::parse_design_source(identity, cosmix_design::EMBEDDED_DEFAULT_SOURCE)
            .ok()?;
    // The context CTK compiles its embedded default with.
    let context = cosmix_design::DesignContext {
        scheme: cosmix_design::Scheme::Ocean,
        mode: cosmix_design::Mode::Light,
        contrast: cosmix_design::Contrast::Normal,
        app: None,
    };
    match cosmix_design::compile_design(&document, context) {
        cosmix_design::DesignCompileResult::Success(success) => {
            Tokens::from_dictionary(success.candidate.dictionary()).ok()
        }
        cosmix_design::DesignCompileResult::Fatal(_) => None,
    }
}

/// An iced font naming `family`. iced fonts hold `&'static str`, so each
/// distinct family name is leaked once.
pub fn named_font(family: &str) -> Font {
    static NAMES: Mutex<Option<HashMap<String, &'static str>>> = Mutex::new(None);
    let mut names = NAMES.lock().unwrap();
    let name = *names
        .get_or_insert_with(HashMap::new)
        .entry(family.to_owned())
        .or_insert_with(|| Box::leak(family.to_owned().into_boxed_str()));
    Font {
        family: Family::Name(name),
        ..Font::DEFAULT
    }
}

fn sync_look(
    design: Res<IcedDesign>,
    ctk_design: Option<Res<CtkDesign>>,
    typography: Option<Res<CtkTypography>>,
    mut last: Local<Option<(Option<u64>, Option<String>, u32)>>,
) {
    let revision = ctk_design
        .as_ref()
        .and_then(|d| d.revision())
        .map(|r| r.get());
    let family = typography.as_ref().map(|t| {
        t.effective_family
            .clone()
            .unwrap_or_else(|| t.requested_family.clone())
    });
    let text_px = typography.as_ref().map_or(DEFAULT_TEXT_PX, |t| t.body_px);
    let key = (revision, family.clone(), text_px.to_bits());
    if last.as_ref() == Some(&key) {
        return;
    }
    *last = Some(key);
    let tokens = ctk_design
        .as_ref()
        .and_then(|d| d.live())
        .and_then(|live| Tokens::from_dictionary(live.dictionary()).ok());
    let mut share = design.0.write().unwrap();
    share.revision += 1;
    share.look = Look {
        tokens: tokens.unwrap_or(share.look.tokens),
        font: family.as_deref().map_or(Font::DEFAULT, named_font),
        text_px,
    };
}

fn send_actions(
    outbox: Res<IcedOutbox>,
    bridge: Option<Res<BusBridge>>,
    mut events: ResMut<SceneEvents>,
) {
    let actions = std::mem::take(&mut *outbox.0.lock().unwrap());
    let Some(bridge) = bridge else {
        return;
    };
    for action in actions {
        events.send_handler(
            &bridge,
            &action.scene,
            &action.citizen,
            &action.node,
            action.kind,
            &action.handler,
            action.value,
            action.item,
        );
    }
}
