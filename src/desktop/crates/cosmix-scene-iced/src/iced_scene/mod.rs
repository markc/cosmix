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

/// Distinct font families this process will ever name to iced. Each one
/// costs a leaked string, so the cache is bounded; a later family falls back
/// to iced's default family with a warning.
pub const MAX_INTERNED_FAMILIES: usize = 8;

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
        // Order-independent: the factory this plugin installs is only read
        // through the bridge, and the bridge only keeps a factory it did not
        // find. Adding it here means a host cannot mount iced scenes without
        // the bridge, and adding both in the wrong order fails loudly in
        // Bevy rather than falling back to the stand-in.
        if !app.is_plugin_added::<crate::SceneIcedPlugin>() {
            app.add_plugins(crate::SceneIcedPlugin);
        }
        let outbox = IcedOutbox::default();
        let design = IcedDesign(Arc::new(RwLock::new(DesignShare {
            revision: 0,
            look: default_look(),
        })));
        let factory_outbox = outbox.0.clone();
        let factory_design = design.0.clone();
        let assets = asset_root();
        app.insert_non_send(SceneIcedFactory(Box::new(move |_| {
            Box::new(IcedSceneRenderer::new(
                factory_design.clone(),
                factory_outbox.clone(),
                assets.clone(),
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
        dark: is_dark(tokens.surface),
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

/// Bevy's asset directory, which is what CTK resolves an `image` src
/// against.
pub fn asset_root() -> std::sync::Arc<std::path::Path> {
    let base = bevy::asset::io::file::FileAssetReader::get_base_path();
    std::sync::Arc::from(base.join("assets").as_path())
}

/// Whether a surface colour reads as dark, by relative luminance.
pub fn is_dark(surface: cosmix_iced_host::core::Color) -> bool {
    0.2126 * surface.r + 0.7152 * surface.g + 0.0722 * surface.b < 0.5
}

/// An iced font naming `family`. iced fonts hold `&'static str`, so each
/// distinct family name is leaked once.
pub fn named_font(family: &str) -> Font {
    static NAMES: Mutex<Option<HashMap<String, &'static str>>> = Mutex::new(None);
    let mut names = NAMES.lock().unwrap();
    let names = names.get_or_insert_with(HashMap::new);
    let name = match names.get(family) {
        Some(name) => *name,
        None if names.len() < MAX_INTERNED_FAMILIES => {
            // iced fonts hold `&'static str`, so a family name must be leaked
            // to be usable. The cache bounds how many can ever be.
            let name: &'static str = Box::leak(family.to_owned().into_boxed_str());
            names.insert(family.to_owned(), name);
            name
        }
        None => {
            static WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                bevy::log::warn!(
                    "scene-iced: more than {MAX_INTERNED_FAMILIES} font families requested; \
                     {family:?} and any later one fall back to the default family"
                );
            }
            return Font::DEFAULT;
        }
    };
    Font {
        family: Family::Name(name),
        ..Font::DEFAULT
    }
}

/// CTK design revision, font family and body size bits.
type LookKey = (Option<u64>, Option<String>, u32);

fn sync_look(
    design: Res<IcedDesign>,
    ctk_design: Option<Res<CtkDesign>>,
    typography: Option<Res<CtkTypography>>,
    mut last: Local<Option<LookKey>>,
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
    let tokens = tokens.unwrap_or(share.look.tokens);
    share.look = Look {
        tokens,
        dark: is_dark(tokens.surface),
        font: family.as_deref().map_or(Font::DEFAULT, named_font),
        text_px,
    };
}

fn send_actions(
    outbox: Res<IcedOutbox>,
    bridge: Option<Res<BusBridge>>,
    mut events: ResMut<SceneEvents>,
) {
    // Keep the queue until there is a bridge: a click before the Bus is up
    // must not vanish.
    let Some(bridge) = bridge else {
        return;
    };
    let actions = std::mem::take(&mut *outbox.0.lock().unwrap());
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
