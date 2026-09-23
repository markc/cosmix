//! Pure adapter from the shell's Bus verbs to its existing command ingress.

use std::time::Duration;

use crate::core::{Edge, OutputKey, PanelInput, PanelMode};

use super::{CarouselInput, ShellCommand, ShellCommandKind};

/// Scene requests are handled by the host's scene adapter, not panel motion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SceneVerb {
    Load,
    Patch,
    Get,
    Describe,
    Unload,
    Watch,
}

impl SceneVerb {
    pub fn parse(command: &str) -> Option<Self> {
        Some(match command {
            "shell.scene.load" => Self::Load,
            "shell.scene.patch" => Self::Patch,
            "shell.scene.get" => Self::Get,
            "shell.scene.describe" => Self::Describe,
            "shell.scene.unload" => Self::Unload,
            "shell.scene.watch" => Self::Watch,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShellSemanticVerb {
    Scene(SceneVerb),
    PanelShow,
    PanelHide,
    PanelToggle,
    PanelPin,
    PanelUnpin,
    /// Precise dock: enter `Docked` regardless of the current mode. Docking
    /// reflows the workspace, so it is never a side effect of another verb.
    PanelDock,
    /// Precise per-edge mode set: drive `Hidden`/`Pinned`/`Docked` explicitly
    /// (shell doc §3.1's verb surface for the menu, keyboard and citizens).
    /// Unlike the legacy verbs this never leaves a transient reveal behind.
    PanelMode(PanelMode),
    PageNext,
    PagePrevious,
    PageSet(String),
    /// Register a sub-panel name on the verb's edge (panel doc §3). Unlike
    /// the panel verbs, identity binds at dispatch: `owner` is the
    /// broker-attested caller, never a caller-supplied field, and the
    /// dispatch reserves the registry seat before acking. Hosts route these
    /// through their registry-aware ingress, not the frame-only verb parser.
    SubRegister {
        name: String,
        owner: String,
    },
    /// Remove a sub-panel (panel doc §3). The name is the address (§5): the
    /// verb's edge, owner and acceptance receipt are the sub-panel's own
    /// seat values, resolved at dispatch — a caller never picks the edge a
    /// removal lands on, and the Model stage applies only against that
    /// exact registration. Routed like [`Self::SubRegister`].
    SubRemove {
        name: String,
        owner: String,
        accepted_at: u64,
    },
}

/// Produce the same [`ShellCommand`] used by pointer and keyboard input.
///
/// Deliberately takes no frame snapshot: every verb (toggle included) binds
/// its direction inside the core at Model time, so a stale snapshot cannot
/// mis-route a verb and two toggles drained in one batch net to identity.
/// The sub-panel verbs are the exception that proves the rule: they carry
/// identity (name, owner), and their dispatch binds that identity against
/// the sub-panel registry before this adapter ever runs.
pub fn semantic_shell_command(
    output: OutputKey,
    at: Duration,
    edge: Edge,
    verb: ShellSemanticVerb,
) -> ShellCommand {
    let kind = match verb {
        ShellSemanticVerb::Scene(verb) => ShellCommandKind::Scene(verb),
        ShellSemanticVerb::PanelShow => ShellCommandKind::Panel {
            edge,
            input: PanelInput::Reveal,
        },
        ShellSemanticVerb::PanelHide => ShellCommandKind::Panel {
            edge,
            input: PanelInput::Hide,
        },
        ShellSemanticVerb::PanelToggle => ShellCommandKind::Panel {
            edge,
            input: PanelInput::Toggle,
        },
        ShellSemanticVerb::PanelPin => ShellCommandKind::Panel {
            edge,
            // Legacy Bus pin keeps its reserving behaviour so popup citizens
            // hold their space; the precise verbs are PanelDock/PanelMode.
            input: PanelInput::Dock,
        },
        ShellSemanticVerb::PanelUnpin => ShellCommandKind::Panel {
            edge,
            // Includes legacy popup records restored as Docked, and new pins.
            input: PanelInput::Release,
        },
        ShellSemanticVerb::PanelDock => ShellCommandKind::Panel {
            edge,
            input: PanelInput::Dock,
        },
        ShellSemanticVerb::PanelMode(mode) => ShellCommandKind::Panel {
            edge,
            input: PanelInput::SetMode(mode),
        },
        ShellSemanticVerb::PageNext => ShellCommandKind::Carousel {
            edge,
            input: CarouselInput::Next,
        },
        ShellSemanticVerb::PagePrevious => ShellCommandKind::Carousel {
            edge,
            input: CarouselInput::Previous,
        },
        ShellSemanticVerb::PageSet(id) => ShellCommandKind::Carousel {
            edge,
            input: CarouselInput::SelectId(id),
        },
        ShellSemanticVerb::SubRegister { name, owner } => {
            ShellCommandKind::SubPanelRegister { edge, name, owner }
        }
        ShellSemanticVerb::SubRemove {
            name,
            owner,
            accepted_at,
        } => ShellCommandKind::SubPanelRemove {
            edge,
            name,
            owner,
            accepted_at,
        },
    };
    ShellCommand { output, at, kind }
}
