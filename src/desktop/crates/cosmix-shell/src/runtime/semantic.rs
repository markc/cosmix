//! Pure adapter from the shell's Bus verbs to its existing command ingress.

use std::time::Duration;

use crate::core::{Edge, OutputKey, PanelInput, PanelMode};

use super::{CarouselInput, KeyboardCommand, ShellCommand, ShellCommandKind};

/// Scene requests are handled by the host's scene adapter, not panel motion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SceneVerb {
    Load,
    Validate,
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
            "shell.scene.validate" => Self::Validate,
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
    /// Enter `Pinned`: a persistent overlay that reserves no space.
    PanelPin,
    /// `Pinned` releases into a transient reveal with normal grace; any other
    /// mode enters `Pinned`. The direction binds at Model time.
    PanelPinToggle,
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
    /// `shell.focus.next`: the focus-cycle binding's step (shell doc §5) —
    /// keyboard focus to the next visible pinned or docked panel on the
    /// output, then back to the application. Output-wide, so the verb's edge
    /// is ignored; [`focus_next_command`] builds it without one.
    FocusNext,
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
    /// Named activation (panel doc §6), addressed exactly like
    /// [`Self::SubRemove`]: the name's seat supplies the edge, owner and
    /// receipt at dispatch. Routed like [`Self::SubRegister`].
    SubActivate {
        name: String,
        owner: String,
        accepted_at: u64,
        /// Ask for the keyboard too (the §6 default); `false` reveals or
        /// switches without it.
        focus: bool,
    },
}

/// The focus-cycle step, shared by Quoin's in-panel chord and the
/// `shell.focus.next` verb so both move focus identically.
pub fn focus_next_command(output: OutputKey, at: Duration) -> ShellCommand {
    // The cycle walks every edge of the output; no edge is addressed.
    semantic_shell_command(output, at, Edge::Left, ShellSemanticVerb::FocusNext)
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
            input: PanelInput::ToggleShown,
        },
        ShellSemanticVerb::PanelPin => ShellCommandKind::Panel {
            edge,
            // Pin is an overlay that reserves no space (Mark, 2026-09-26);
            // PanelDock is the reserving verb.
            input: PanelInput::Pin,
        },
        ShellSemanticVerb::PanelPinToggle => ShellCommandKind::Panel {
            edge,
            input: PanelInput::PinToggle,
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
        ShellSemanticVerb::FocusNext => ShellCommandKind::Keyboard(KeyboardCommand::CycleFocus),
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
        ShellSemanticVerb::SubActivate {
            name,
            owner,
            accepted_at,
            focus,
        } => ShellCommandKind::SubPanelActivate {
            edge,
            name,
            owner,
            accepted_at,
            focus,
        },
    };
    ShellCommand { output, at, kind }
}
