//! Pure adapter from the shell's Bus verbs to its existing command ingress.

use std::time::Duration;

use crate::core::{Edge, OutputKey, PanelInput};

use super::{CarouselInput, ShellCommand, ShellCommandKind};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShellSemanticVerb {
    PanelShow,
    PanelHide,
    PanelToggle,
    PanelPin,
    PanelUnpin,
    PageNext,
    PagePrevious,
    PageSet(String),
}

/// Produce the same [`ShellCommand`] used by pointer and keyboard input.
///
/// Deliberately takes no frame snapshot: every verb (toggle included) binds
/// its direction inside the core at Model time, so a stale snapshot cannot
/// mis-route a verb and two toggles drained in one batch net to identity.
pub fn semantic_shell_command(
    output: OutputKey,
    at: Duration,
    edge: Edge,
    verb: ShellSemanticVerb,
) -> ShellCommand {
    let kind = match verb {
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
            input: PanelInput::Pin,
        },
        ShellSemanticVerb::PanelUnpin => ShellCommandKind::Panel {
            edge,
            input: PanelInput::Unpin,
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
    };
    ShellCommand { output, at, kind }
}
