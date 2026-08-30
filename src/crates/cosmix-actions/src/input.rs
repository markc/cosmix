use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde::de::{SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Maximum strokes accepted in one chord.
///
/// Eight supports conventional multi-stage command palettes while bounding
/// strict-data decoding and resolver state.
pub const MAX_CHORD_STROKES: usize = 8;

/// A caller-supplied monotonic timestamp in milliseconds.
///
/// The resolver compares ticks but never reads a clock, so replayed input and
/// timeout tests are deterministic.  Wrapping clocks should be converted to a
/// wider monotonic epoch by the engine adapter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Tick(pub u64);

impl Tick {
    /// Return a later tick, saturating rather than wrapping.
    pub const fn saturating_add(self, milliseconds: u64) -> Self {
        Self(self.0.saturating_add(milliseconds))
    }
}

/// Engine-independent physical key names used by a [`KeyStroke`].
///
/// Alphabetic and numeric keys use [`Key::character`]. Engine adapters should
/// translate the physical key code rather than locale-produced text; text
/// editing remains the focused widget's job. Programmatically constructed
/// values are checked by [`Key::validate`] and [`crate::Keymap::validate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Key {
    /// An ASCII letter or digit, normalised to uppercase for letters.
    Character(char),
    /// Space bar.
    Space,
    /// Enter or Return.
    Enter,
    /// Escape.
    Escape,
    /// Tab.
    Tab,
    /// Backspace.
    Backspace,
    /// Forward delete.
    Delete,
    /// Insert.
    Insert,
    /// Home.
    Home,
    /// End.
    End,
    /// Page Up.
    PageUp,
    /// Page Down.
    PageDown,
    /// Up arrow.
    ArrowUp,
    /// Down arrow.
    ArrowDown,
    /// Left arrow.
    ArrowLeft,
    /// Right arrow.
    ArrowRight,
    /// Function key F1 through F24.
    Function(u8),
    /// Minus key.
    Minus,
    /// Equals key.
    Equal,
    /// Comma key.
    Comma,
    /// Period key.
    Period,
    /// Slash key.
    Slash,
    /// Backslash key.
    Backslash,
    /// Semicolon key.
    Semicolon,
    /// Quote key.
    Quote,
    /// Left bracket key.
    BracketLeft,
    /// Right bracket key.
    BracketRight,
    /// Backquote key.
    Backquote,
}

impl Key {
    /// Construct a normalised ASCII letter or digit key.
    pub fn character(character: char) -> Result<Self, KeyParseError> {
        let key = Self::Character(character.to_ascii_uppercase());
        key.validate()?;
        Ok(key)
    }

    /// Construct an F1 through F24 function key.
    pub fn function(number: u8) -> Result<Self, KeyParseError> {
        let key = Self::Function(number);
        key.validate()?;
        Ok(key)
    }

    /// Enforce the stable parse/serialisation vocabulary.
    pub fn validate(self) -> Result<(), KeyParseError> {
        match self {
            Self::Character(character)
                if !character.is_ascii_alphanumeric()
                    || (character.is_ascii_alphabetic() && !character.is_ascii_uppercase()) =>
            {
                Err(KeyParseError::InvalidCharacter(character))
            }
            Self::Function(number) if !(1..=24).contains(&number) => {
                Err(KeyParseError::InvalidFunction(number))
            }
            _ => Ok(()),
        }
    }
}

impl fmt::Display for Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Character(character) => character.to_ascii_uppercase().fmt(formatter),
            Self::Space => formatter.write_str("Space"),
            Self::Enter => formatter.write_str("Enter"),
            Self::Escape => formatter.write_str("Escape"),
            Self::Tab => formatter.write_str("Tab"),
            Self::Backspace => formatter.write_str("Backspace"),
            Self::Delete => formatter.write_str("Delete"),
            Self::Insert => formatter.write_str("Insert"),
            Self::Home => formatter.write_str("Home"),
            Self::End => formatter.write_str("End"),
            Self::PageUp => formatter.write_str("PageUp"),
            Self::PageDown => formatter.write_str("PageDown"),
            Self::ArrowUp => formatter.write_str("ArrowUp"),
            Self::ArrowDown => formatter.write_str("ArrowDown"),
            Self::ArrowLeft => formatter.write_str("ArrowLeft"),
            Self::ArrowRight => formatter.write_str("ArrowRight"),
            Self::Function(number) => write!(formatter, "F{number}"),
            Self::Minus => formatter.write_str("Minus"),
            Self::Equal => formatter.write_str("Equal"),
            Self::Comma => formatter.write_str("Comma"),
            Self::Period => formatter.write_str("Period"),
            Self::Slash => formatter.write_str("Slash"),
            Self::Backslash => formatter.write_str("Backslash"),
            Self::Semicolon => formatter.write_str("Semicolon"),
            Self::Quote => formatter.write_str("Quote"),
            Self::BracketLeft => formatter.write_str("BracketLeft"),
            Self::BracketRight => formatter.write_str("BracketRight"),
            Self::Backquote => formatter.write_str("Backquote"),
        }
    }
}

impl FromStr for Key {
    type Err = KeyParseError;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        if source.len() == 1 {
            let character = source.chars().next().expect("one-byte string has a char");
            if character.is_ascii_alphanumeric() {
                return Self::character(character);
            }
        }
        let lower = source.to_ascii_lowercase();
        let key = match lower.as_str() {
            "space" => Self::Space,
            "enter" | "return" => Self::Enter,
            "escape" | "esc" => Self::Escape,
            "tab" => Self::Tab,
            "backspace" => Self::Backspace,
            "delete" | "del" => Self::Delete,
            "insert" | "ins" => Self::Insert,
            "home" => Self::Home,
            "end" => Self::End,
            "pageup" => Self::PageUp,
            "pagedown" => Self::PageDown,
            "arrowup" | "up" => Self::ArrowUp,
            "arrowdown" | "down" => Self::ArrowDown,
            "arrowleft" | "left" => Self::ArrowLeft,
            "arrowright" | "right" => Self::ArrowRight,
            "minus" => Self::Minus,
            "equal" | "equals" => Self::Equal,
            "comma" => Self::Comma,
            "period" => Self::Period,
            "slash" => Self::Slash,
            "backslash" => Self::Backslash,
            "semicolon" => Self::Semicolon,
            "quote" => Self::Quote,
            "bracketleft" => Self::BracketLeft,
            "bracketright" => Self::BracketRight,
            "backquote" => Self::Backquote,
            _ if lower.starts_with('f') => {
                let number = lower[1..]
                    .parse::<u8>()
                    .map_err(|_| KeyParseError::UnknownKey(source.to_owned()))?;
                if !(1..=24).contains(&number) {
                    return Err(KeyParseError::UnknownKey(source.to_owned()));
                }
                return Self::function(number);
            }
            _ => return Err(KeyParseError::UnknownKey(source.to_owned())),
        };
        key.validate()?;
        Ok(key)
    }
}

/// Modifier state accompanying a physical key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Modifiers {
    /// Control is held.
    pub control: bool,
    /// Alt/Option is held.
    pub alt: bool,
    /// Shift is held.
    pub shift: bool,
    /// Super/Command/Windows is held.
    pub super_key: bool,
}

impl Modifiers {
    /// No modifiers.
    pub const NONE: Self = Self {
        control: false,
        alt: false,
        shift: false,
        super_key: false,
    };
}

/// One physical key plus its required modifier state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyStroke {
    /// Physical key.
    pub key: Key,
    /// Exact required modifier state.
    pub modifiers: Modifiers,
}

impl KeyStroke {
    /// Construct a validated key stroke.
    pub fn new(key: Key, modifiers: Modifiers) -> Result<Self, KeyParseError> {
        key.validate()?;
        Ok(Self { key, modifiers })
    }

    /// Construct an unmodified key stroke.
    pub fn plain(key: Key) -> Result<Self, KeyParseError> {
        Self::new(key, Modifiers::NONE)
    }

    /// Validate a programmatically constructed stroke.
    pub fn validate(self) -> Result<(), KeyParseError> {
        self.key.validate()
    }
}

impl fmt::Display for KeyStroke {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.modifiers.control {
            formatter.write_str("Ctrl+")?;
        }
        if self.modifiers.alt {
            formatter.write_str("Alt+")?;
        }
        if self.modifiers.shift {
            formatter.write_str("Shift+")?;
        }
        if self.modifiers.super_key {
            formatter.write_str("Super+")?;
        }
        self.key.fmt(formatter)
    }
}

impl FromStr for KeyStroke {
    type Err = KeyParseError;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        let parts: Vec<_> = source.split('+').map(str::trim).collect();
        let (key_name, modifier_names) = parts
            .split_last()
            .ok_or_else(|| KeyParseError::EmptyStroke(source.to_owned()))?;
        if key_name.is_empty() {
            return Err(KeyParseError::EmptyStroke(source.to_owned()));
        }
        let mut modifiers = Modifiers::NONE;
        for name in modifier_names {
            let slot = match name.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => &mut modifiers.control,
                "alt" | "option" => &mut modifiers.alt,
                "shift" => &mut modifiers.shift,
                "super" | "cmd" | "command" | "meta" => &mut modifiers.super_key,
                _ => return Err(KeyParseError::UnknownModifier((*name).to_owned())),
            };
            if *slot {
                return Err(KeyParseError::DuplicateModifier((*name).to_owned()));
            }
            *slot = true;
        }
        Self::new(key_name.parse()?, modifiers)
    }
}

impl Serialize for KeyStroke {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for KeyStroke {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// A non-empty multi-stroke keyboard sequence.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Chord(Vec<KeyStroke>);

impl Chord {
    /// Construct and validate a non-empty chord.
    pub fn new(strokes: Vec<KeyStroke>) -> Result<Self, KeyParseError> {
        if strokes.is_empty() {
            return Err(KeyParseError::EmptyChord);
        }
        if strokes.len() > MAX_CHORD_STROKES {
            return Err(KeyParseError::TooManyChordStrokes {
                count: strokes.len(),
                maximum: MAX_CHORD_STROKES,
            });
        }
        for stroke in &strokes {
            stroke.validate()?;
        }
        Ok(Self(strokes))
    }

    /// Construct a one-stroke chord.
    pub fn single(stroke: KeyStroke) -> Result<Self, KeyParseError> {
        Self::new(vec![stroke])
    }

    /// Return the strokes in order.
    pub fn strokes(&self) -> &[KeyStroke] {
        &self.0
    }

    /// Whether this chord has no strokes and is therefore invalid in a keymap.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Validate this chord's non-empty and key-vocabulary invariants.
    pub fn validate(&self) -> Result<(), KeyParseError> {
        if self.0.is_empty() {
            return Err(KeyParseError::EmptyChord);
        }
        if self.0.len() > MAX_CHORD_STROKES {
            return Err(KeyParseError::TooManyChordStrokes {
                count: self.0.len(),
                maximum: MAX_CHORD_STROKES,
            });
        }
        for stroke in &self.0 {
            stroke.validate()?;
        }
        Ok(())
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, stroke) in self.0.iter().enumerate() {
            if index != 0 {
                formatter.write_str(", ")?;
            }
            stroke.fmt(formatter)?;
        }
        Ok(())
    }
}

impl FromStr for Chord {
    type Err = KeyParseError;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        let strokes = source
            .split(',')
            .map(str::trim)
            .map(str::parse)
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(strokes)
    }
}

impl<'de> Deserialize<'de> for Chord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ChordVisitor;

        impl<'de> Visitor<'de> for ChordVisitor {
            type Value = Chord;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "one to {MAX_CHORD_STROKES} key strokes in a sequence"
                )
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut strokes =
                    Vec::with_capacity(sequence.size_hint().unwrap_or(1).min(MAX_CHORD_STROKES));
                while let Some(stroke) = sequence.next_element()? {
                    if strokes.len() == MAX_CHORD_STROKES {
                        return Err(serde::de::Error::custom(
                            KeyParseError::TooManyChordStrokes {
                                count: MAX_CHORD_STROKES + 1,
                                maximum: MAX_CHORD_STROKES,
                            },
                        ));
                    }
                    strokes.push(stroke);
                }
                Chord::new(strokes).map_err(serde::de::Error::custom)
            }
        }

        deserializer.deserialize_seq(ChordVisitor)
    }
}

/// A binding's input scope.
///
/// Modal scope is exclusive while a modal is captured.  Focus-tag bindings are
/// active only when the app supplies that tag and outrank globals.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BindingScope {
    /// Available whenever no modal owns input.
    #[default]
    Global,
    /// Available only to the named captured modal.
    Modal(String),
    /// Available when the app-defined focus tag is present.
    FocusTag(String),
}

impl BindingScope {
    /// Construct a validated modal scope.
    pub fn modal(name: impl Into<String>) -> Result<Self, KeyParseError> {
        let name = name.into();
        validate_scope_name(&name)?;
        Ok(Self::Modal(name))
    }

    /// Construct a validated focus-tag scope.
    pub fn focus_tag(name: impl Into<String>) -> Result<Self, KeyParseError> {
        let name = name.into();
        validate_scope_name(&name)?;
        Ok(Self::FocusTag(name))
    }

    /// Enforce the scope-name parse vocabulary.
    pub fn validate(&self) -> Result<(), KeyParseError> {
        match self {
            Self::Global => Ok(()),
            Self::Modal(name) | Self::FocusTag(name) => validate_scope_name(name),
        }
    }

    pub(crate) fn rank(&self) -> u8 {
        match self {
            Self::Global => 0,
            Self::FocusTag(_) => 1,
            Self::Modal(_) => 2,
        }
    }
}

impl fmt::Display for BindingScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Global => formatter.write_str("global"),
            Self::Modal(name) => write!(formatter, "modal:{name}"),
            Self::FocusTag(name) => write!(formatter, "focus:{name}"),
        }
    }
}

impl FromStr for BindingScope {
    type Err = KeyParseError;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        if source == "global" {
            return Ok(Self::Global);
        }
        if let Some(name) = source
            .strip_prefix("modal:")
            .filter(|name| !name.is_empty())
        {
            return Self::modal(name);
        }
        if let Some(name) = source
            .strip_prefix("focus:")
            .filter(|name| !name.is_empty())
        {
            return Self::focus_tag(name);
        }
        Err(KeyParseError::UnknownScope(source.to_owned()))
    }
}

impl Serialize for BindingScope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for BindingScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// What a binding does with an operating-system key-repeat press.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepeatPolicy {
    /// Ignore repeated presses; suitable for toggles, dialogs and file actions.
    #[default]
    Ignore,
    /// Resolve repeated presses; suitable for incremental navigation or zoom.
    Allow,
}

/// Press or release state of raw engine input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawInputState {
    /// Key went down.
    Pressed,
    /// Key went up.
    Released,
}

/// One engine-normalised keyboard event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawInput {
    /// Physical key.
    pub key: Key,
    /// Modifier state at this event.
    pub modifiers: Modifiers,
    /// Press or release.
    pub state: RawInputState,
    /// Whether this press was generated by key repeat.
    pub repeat: bool,
}

impl RawInput {
    /// Construct a non-repeating press.
    pub const fn pressed(key: Key, modifiers: Modifiers) -> Self {
        Self {
            key,
            modifiers,
            state: RawInputState::Pressed,
            repeat: false,
        }
    }

    pub(crate) const fn stroke(self) -> KeyStroke {
        KeyStroke {
            key: self.key,
            modifiers: self.modifiers,
        }
    }
}

/// Input ownership and focus information supplied by the app.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FocusContext {
    /// Whether the focused widget edits text or another literal value.
    pub focused_editable: bool,
    /// Name of the modal that exclusively owns keyboard input, if any.
    pub modal_scope: Option<String>,
    /// App-defined tags describing the focused surface or control.
    pub focus_tags: BTreeSet<String>,
}

impl FocusContext {
    /// Construct the ordinary non-editable, non-modal context.
    pub fn global() -> Self {
        Self::default()
    }

    /// Construct a context captured by a validated modal scope.
    pub fn modal(name: impl Into<String>) -> Result<Self, KeyParseError> {
        let name = name.into();
        validate_scope_name(&name)?;
        Ok(Self {
            modal_scope: Some(name),
            ..Self::default()
        })
    }

    /// Set whether the focused widget owns editable input.
    pub fn with_editable(mut self, focused_editable: bool) -> Self {
        self.focused_editable = focused_editable;
        self
    }

    /// Add one validated app-defined focus tag.
    pub fn with_focus_tag(mut self, tag: impl Into<String>) -> Result<Self, KeyParseError> {
        let tag = tag.into();
        validate_scope_name(&tag)?;
        self.focus_tags.insert(tag);
        Ok(self)
    }

    /// Validate modal and focus-tag names supplied through public fields.
    pub fn validate(&self) -> Result<(), KeyParseError> {
        if let Some(modal) = &self.modal_scope {
            validate_scope_name(modal)?;
        }
        for tag in &self.focus_tags {
            validate_scope_name(tag)?;
        }
        Ok(())
    }

    /// Test whether a scope is eligible before precedence is applied.
    pub fn admits(&self, scope: &BindingScope) -> bool {
        match (&self.modal_scope, scope) {
            (Some(active), BindingScope::Modal(required)) => active == required,
            (Some(_), _) => false,
            (None, BindingScope::Global) => true,
            (None, BindingScope::FocusTag(required)) => self.focus_tags.contains(required),
            (None, BindingScope::Modal(_)) => false,
        }
    }
}

fn validate_scope_name(name: &str) -> Result<(), KeyParseError> {
    if name.is_empty()
        || name.len() > 64
        || !name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':' | b'/')
        })
    {
        return Err(KeyParseError::InvalidScopeName(name.to_owned()));
    }
    Ok(())
}

/// Error parsing a stable key, stroke, chord or scope spelling.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyParseError {
    /// A key name is not in the engine-independent vocabulary.
    #[error("unknown key {0:?}")]
    UnknownKey(String),
    /// A programmatic character was not a normalised ASCII letter or digit.
    #[error("invalid character key {0:?}; use an uppercase ASCII letter or digit")]
    InvalidCharacter(char),
    /// A programmatic function-key number was outside F1 through F24.
    #[error("invalid function key F{0}; expected F1 through F24")]
    InvalidFunction(u8),
    /// A modifier name is unknown.
    #[error("unknown modifier {0:?}")]
    UnknownModifier(String),
    /// A modifier appeared twice.
    #[error("duplicate modifier {0:?}")]
    DuplicateModifier(String),
    /// The stroke had no key.
    #[error("key stroke has no key: {0:?}")]
    EmptyStroke(String),
    /// A chord had no strokes.
    #[error("chord must contain at least one stroke")]
    EmptyChord,
    /// A chord exceeded [`MAX_CHORD_STROKES`].
    #[error("chord has {count} strokes; maximum is {maximum}")]
    TooManyChordStrokes {
        /// Actual stroke count.
        count: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// A binding scope did not use `global`, `modal:<name>` or `focus:<tag>`.
    #[error("unknown binding scope {0:?}")]
    UnknownScope(String),
    /// A programmatic scope name was empty, too long, or outside the id grammar.
    #[error("invalid scope name {0:?}")]
    InvalidScopeName(String),
}
