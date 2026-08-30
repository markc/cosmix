//! Strict MIDI 1.0 byte-stream parsing, serialisation and canonicalisation.
//!
//! [`Parser`] is the raw streaming state machine. It expands running status,
//! permits System Real Time bytes between any two bytes (including inside a
//! channel message or SysEx), and appends zero or more typed [`Event`] values
//! for each input byte. Invalid bytes and abandoned partial messages are
//! counted in [`ParseStats`], never surfaced as panics.
//!
//! [`parse_raw`] preserves the parsed MIDI semantics. [`parse`] additionally
//! applies the canonical form required by the MIDI 1.0 → MIDI 2.0 → MIDI 1.0
//! identity theorem. [`serialize`] always emits explicit channel status bytes;
//! it never emits running status.
//!
//! A raw-wire `0xF7` outside an active SysEx is skipped and counted. The byte
//! alone cannot distinguish an SMF F7-continuation event from a stray EOX;
//! SMF has a length field that the wire stream lacks. Explicit
//! [`SysEx::Continue`] and [`SysEx::End`] values remain serialisable for that
//! future length-aware adapter. This also follows AM_MIDI2.0Lib
//! `tests/tests.cpp` Test 12's stated rule that an extra F7 should be ignored,
//! and ni-midi2 `midi1_byte_stream_tests.cpp` rejects F7 as a standalone
//! System message.

use super::{cv1::Midi1Cv, msg::System};

/// One parsed MIDI 1.0 byte-stream event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// MIDI 1.0 Channel Voice.
    Midi1Cv(Midi1Cv),
    /// System Common or System Real Time.
    System(System),
    /// A complete or partial SysEx fragment.
    SysEx(SysEx),
}

/// One MIDI 1.0 event with its absolute musical tick.
///
/// Canonicalisation retains the source tick of a pending CC6 Data Entry,
/// including when its CC38 arrives later or the entry drains at end-of-stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedEvent {
    /// Absolute tick in the containing timeline's ticks-per-quarter unit.
    pub tick: u64,
    /// Typed MIDI 1.0 event.
    pub event: Event,
}

/// One UMP-sized SysEx7 payload.
///
/// MIDI 1.0 wire SysEx is unbounded, but the typed stream deliberately
/// fragments it into payloads of at most six bytes. One [`SysEx`] event can
/// therefore always translate to one MT `0x3` packet without allocation or
/// an unbounded translator return type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SysExData {
    group: u8,
    data: [u8; 6],
    len: u8,
}

impl SysExData {
    /// Construct a payload. More than six bytes or any non-seven-bit byte is
    /// rejected.
    pub fn new(data: &[u8]) -> Option<Self> {
        Self::with_group(0, data)
    }

    /// Construct a payload in `group & 0x0F`.
    pub fn with_group(group: u8, data: &[u8]) -> Option<Self> {
        if data.len() > 6 || data.iter().any(|byte| byte & 0x80 != 0) {
            return None;
        }
        let mut payload = [0; 6];
        payload[..data.len()].copy_from_slice(data);
        Some(Self {
            group: group & 0xF,
            data: payload,
            len: data.len() as u8,
        })
    }

    /// UMP group assigned by the parser or translator.
    pub const fn group(self) -> u8 {
        self.group
    }

    /// Seven-bit payload bytes.
    pub fn data(&self) -> &[u8] {
        &self.data[..usize::from(self.len)]
    }
}

/// SysEx framing retained by the byte-stream codec.
///
/// Fragment variants make an interleaved Real Time event representable
/// without moving it before or after the surrounding SysEx data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SysEx {
    /// `F0`, payload, `F7`.
    Complete(SysExData),
    /// `F0`, payload; more fragments follow.
    Start(SysExData),
    /// Payload only; an enclosing length-aware transport supplies framing.
    Continue(SysExData),
    /// Payload followed by `F7`.
    End(SysExData),
}

impl SysEx {
    /// Construct a complete fragment.
    pub fn complete(data: &[u8]) -> Option<Self> {
        SysExData::new(data).map(Self::Complete)
    }

    /// Construct a complete fragment in a UMP group.
    pub fn complete_in(group: u8, data: &[u8]) -> Option<Self> {
        SysExData::with_group(group, data).map(Self::Complete)
    }

    /// Construct a start fragment.
    pub fn start(data: &[u8]) -> Option<Self> {
        SysExData::new(data).map(Self::Start)
    }

    /// Construct a start fragment in a UMP group.
    pub fn start_in(group: u8, data: &[u8]) -> Option<Self> {
        SysExData::with_group(group, data).map(Self::Start)
    }

    /// Construct a continuation fragment.
    pub fn continue_(data: &[u8]) -> Option<Self> {
        SysExData::new(data).map(Self::Continue)
    }

    /// Construct a continuation fragment in a UMP group.
    pub fn continue_in(group: u8, data: &[u8]) -> Option<Self> {
        SysExData::with_group(group, data).map(Self::Continue)
    }

    /// Construct an end fragment.
    pub fn end(data: &[u8]) -> Option<Self> {
        SysExData::new(data).map(Self::End)
    }

    /// Construct an end fragment in a UMP group.
    pub fn end_in(group: u8, data: &[u8]) -> Option<Self> {
        SysExData::with_group(group, data).map(Self::End)
    }

    /// UMP group assigned to this fragment.
    pub const fn group(self) -> u8 {
        match self {
            Self::Complete(data) | Self::Start(data) | Self::Continue(data) | Self::End(data) => {
                data.group()
            }
        }
    }

    /// Seven-bit payload bytes, excluding `F0` and `F7`.
    pub fn data(&self) -> &[u8] {
        match self {
            Self::Complete(data) | Self::Start(data) | Self::Continue(data) | Self::End(data) => {
                data.data()
            }
        }
    }
}

/// Counts of malformed input discarded while resynchronising.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ParseStats {
    /// Data bytes without a usable status, reserved statuses, and stray EOX
    /// bytes skipped individually.
    pub skipped_bytes: u64,
    /// Incomplete Channel Voice or System Common messages abandoned by a new
    /// status or end-of-input.
    pub aborted_messages: u64,
    /// Unterminated SysEx messages abandoned by a non-Real-Time status or
    /// end-of-input.
    pub aborted_sysex: u64,
}

/// Result of parsing a complete byte slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseResult {
    /// Parsed events in wire order.
    pub events: Vec<Event>,
    /// Recovery counters.
    pub stats: ParseStats,
}

#[derive(Debug, Clone, Copy)]
struct PendingMessage {
    status: u8,
    data: [u8; 2],
    len: u8,
    need: u8,
}

impl PendingMessage {
    const fn new(status: u8, need: u8) -> Self {
        Self {
            status,
            data: [0; 2],
            len: 0,
            need,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct PendingSysEx {
    data: Vec<u8>,
    start_emitted: bool,
}

/// Stateful MIDI 1.0 wire parser.
///
/// The parser can span arbitrary input chunks. Call [`Self::finish`] only at
/// the actual end of the stream; it discards and counts incomplete tails.
#[derive(Debug, Clone)]
pub struct Parser {
    group: u8,
    running_status: Option<u8>,
    pending: Option<PendingMessage>,
    sysex: Option<PendingSysEx>,
    stats: ParseStats,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Parser {
    /// Create a parser whose typed events use `group & 0x0F`.
    pub const fn new(group: u8) -> Self {
        Self {
            group: group & 0xF,
            running_status: None,
            pending: None,
            sysex: None,
            stats: ParseStats {
                skipped_bytes: 0,
                aborted_messages: 0,
                aborted_sysex: 0,
            },
        }
    }

    /// Feed one byte, appending any completed events to `output`.
    ///
    /// A Real Time byte inside SysEx can append a SysEx fragment and the
    /// System event, hence the output parameter rather than `Option<Event>`.
    pub fn push(&mut self, byte: u8, output: &mut Vec<Event>) {
        if byte >= 0xF8 {
            if let Some(message) = realtime(self.group, byte) {
                self.split_sysex_for_realtime(output);
                output.push(Event::System(message));
            } else {
                // Reserved Real Time bytes do not disturb any parser state.
                self.stats.skipped_bytes += 1;
            }
            return;
        }

        if self.sysex.is_some() {
            match byte {
                0x00..=0x7F => {
                    if self.sysex.as_ref().expect("checked above").data.len() == 6 {
                        self.split_full_sysex(output);
                    }
                    self.sysex.as_mut().expect("checked above").data.push(byte);
                    return;
                }
                0xF7 => {
                    let sysex = self.sysex.take().expect("checked above");
                    let fragment = if sysex.start_emitted {
                        SysEx::end_in(self.group, &sysex.data)
                            .expect("parser fragments at six bytes")
                    } else {
                        SysEx::complete_in(self.group, &sysex.data)
                            .expect("parser fragments at six bytes")
                    };
                    output.push(Event::SysEx(fragment));
                    return;
                }
                _ => self.abort_sysex(output),
            }
        }

        if byte < 0x80 {
            self.push_data(byte, output);
            return;
        }

        match byte {
            0x80..=0xEF => {
                self.abort_pending();
                self.running_status = Some(byte);
                self.pending = Some(PendingMessage::new(byte, data_bytes(byte)));
            }
            0xF0 => {
                self.abort_pending();
                self.running_status = None;
                self.sysex = Some(PendingSysEx::default());
            }
            0xF1..=0xF3 => {
                self.abort_pending();
                self.running_status = None;
                self.pending = Some(PendingMessage::new(byte, data_bytes(byte)));
            }
            0xF4 | 0xF5 => {
                self.abort_pending();
                self.running_status = None;
                self.stats.skipped_bytes += 1;
            }
            0xF6 => {
                self.abort_pending();
                self.running_status = None;
                output.push(Event::System(System::tune_request(self.group)));
            }
            0xF7 => {
                self.abort_pending();
                self.running_status = None;
                // Outside SysEx, EOX has no inferable continuation payload.
                self.stats.skipped_bytes += 1;
            }
            _ => unreachable!("Real Time statuses returned above"),
        }
    }

    /// Feed a byte slice.
    pub fn feed(&mut self, bytes: &[u8], output: &mut Vec<Event>) {
        for &byte in bytes {
            self.push(byte, output);
        }
    }

    /// Finish the stream, discarding and counting incomplete state.
    pub fn finish(&mut self) {
        if self.pending.take().is_some() {
            self.stats.aborted_messages += 1;
        }
        if self.sysex.take().is_some() {
            self.stats.aborted_sysex += 1;
        }
        self.running_status = None;
    }

    /// Current recovery counters.
    pub const fn stats(&self) -> ParseStats {
        self.stats
    }

    /// Reset parser state and counters.
    pub fn reset(&mut self) {
        *self = Self::new(self.group);
    }

    fn push_data(&mut self, byte: u8, output: &mut Vec<Event>) {
        if self.pending.is_none() {
            let Some(status) = self.running_status else {
                self.stats.skipped_bytes += 1;
                return;
            };
            self.pending = Some(PendingMessage::new(status, data_bytes(status)));
        }

        let pending = self.pending.as_mut().expect("created above");
        pending.data[usize::from(pending.len)] = byte;
        pending.len += 1;
        if pending.len != pending.need {
            return;
        }

        let pending = self.pending.take().expect("complete pending message");
        output.push(decode_complete(self.group, pending));
    }

    fn abort_pending(&mut self) {
        if self.pending.take().is_some() {
            self.stats.aborted_messages += 1;
        }
    }

    fn abort_sysex(&mut self, output: &mut Vec<Event>) {
        let sysex = self.sysex.take().expect("called only while in SysEx");
        if sysex.start_emitted && !sysex.data.is_empty() {
            output.push(Event::SysEx(
                SysEx::continue_in(self.group, &sysex.data).expect("parser fragments at six bytes"),
            ));
        }
        self.stats.aborted_sysex += 1;
    }

    fn split_full_sysex(&mut self, output: &mut Vec<Event>) {
        let sysex = self.sysex.as_mut().expect("called only while in SysEx");
        let data = std::mem::take(&mut sysex.data);
        let fragment = if sysex.start_emitted {
            SysEx::continue_in(self.group, &data).expect("exactly six bytes")
        } else {
            sysex.start_emitted = true;
            SysEx::start_in(self.group, &data).expect("exactly six bytes")
        };
        output.push(Event::SysEx(fragment));
    }

    fn split_sysex_for_realtime(&mut self, output: &mut Vec<Event>) {
        let Some(sysex) = self.sysex.as_mut() else {
            return;
        };
        let data = std::mem::take(&mut sysex.data);
        if sysex.start_emitted {
            if !data.is_empty() {
                output.push(Event::SysEx(
                    SysEx::continue_in(self.group, &data).expect("parser fragments at six bytes"),
                ));
            }
        } else {
            sysex.start_emitted = true;
            output.push(Event::SysEx(
                SysEx::start_in(self.group, &data).expect("parser fragments at six bytes"),
            ));
        }
    }
}

const fn data_bytes(status: u8) -> u8 {
    match status {
        0x80..=0xBF | 0xE0..=0xEF | 0xF2 => 2,
        0xC0..=0xDF | 0xF1 | 0xF3 => 1,
        _ => 0,
    }
}

fn decode_complete(group: u8, pending: PendingMessage) -> Event {
    let channel = pending.status & 0xF;
    let data1 = pending.data[0];
    let data2 = pending.data[1];
    match pending.status >> 4 {
        0x8 => Event::Midi1Cv(Midi1Cv::note_off(group, channel, data1, data2)),
        0x9 => Event::Midi1Cv(Midi1Cv::note_on(group, channel, data1, data2)),
        0xA => Event::Midi1Cv(Midi1Cv::poly_pressure(group, channel, data1, data2)),
        0xB => Event::Midi1Cv(Midi1Cv::control_change(group, channel, data1, data2)),
        0xC => Event::Midi1Cv(Midi1Cv::program_change(group, channel, data1)),
        0xD => Event::Midi1Cv(Midi1Cv::channel_pressure(group, channel, data1)),
        0xE => Event::Midi1Cv(Midi1Cv::pitch_bend(
            group,
            channel,
            u16::from(data1) | (u16::from(data2) << 7),
        )),
        0xF => match pending.status {
            0xF1 => Event::System(System::mtc_quarter_frame(group, data1)),
            0xF2 => Event::System(System::song_position(
                group,
                u16::from(data1) | (u16::from(data2) << 7),
            )),
            0xF3 => Event::System(System::song_select(group, data1)),
            _ => unreachable!("only defined data-bearing System statuses are queued"),
        },
        _ => unreachable!("status byte has a high bit"),
    }
}

fn realtime(group: u8, status: u8) -> Option<System> {
    Some(match status {
        0xF8 => System::timing_clock(group),
        0xFA => System::start(group),
        0xFB => System::continue_(group),
        0xFC => System::stop(group),
        0xFE => System::active_sensing(group),
        0xFF => System::reset(group),
        0xF9 | 0xFD => return None,
        _ => unreachable!("called only for Real Time status range"),
    })
}

/// Parse a complete slice without semantic canonicalisation.
pub fn parse_raw(bytes: &[u8]) -> ParseResult {
    let mut parser = Parser::default();
    let mut events = Vec::new();
    parser.feed(bytes, &mut events);
    parser.finish();
    ParseResult {
        events,
        stats: parser.stats(),
    }
}

/// Parse a complete slice into canonical events.
///
/// This is the convenience counterpart used by the identity property:
/// `serialize(&parse(bytes).events) == canonicalize(bytes)`.
pub fn parse(bytes: &[u8]) -> ParseResult {
    let raw = parse_raw(bytes);
    ParseResult {
        events: canonicalize_events(&raw.events),
        stats: raw.stats,
    }
}

/// Why a typed event cannot be represented as a MIDI 1.0 byte stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerializeError {
    /// An undefined MT `0x2` Channel Voice status.
    UnknownChannelVoice,
    /// An undefined MT `0x1` System status.
    UnknownSystem,
    /// SysEx payload contained a byte with its high bit set.
    NonSevenBitSysExData,
}

/// Serialise typed events with explicit status bytes.
///
/// Running status is deliberately never emitted: the result is the
/// unambiguous canonical wire form and can be parsed independently at every
/// event boundary.
pub fn serialize(events: &[Event]) -> Result<Vec<u8>, SerializeError> {
    let mut output = Vec::new();
    for event in events {
        serialize_event(event, &mut output)?;
    }
    Ok(output)
}

fn serialize_event(event: &Event, output: &mut Vec<u8>) -> Result<(), SerializeError> {
    match event {
        Event::Midi1Cv(message) => serialize_cv(*message, output),
        Event::System(message) => serialize_system(*message, output),
        Event::SysEx(message) => serialize_sysex(message, output),
    }
}

fn serialize_cv(message: Midi1Cv, output: &mut Vec<u8>) -> Result<(), SerializeError> {
    let channel = message.channel();
    match message {
        Midi1Cv::NoteOff(note) => {
            output.extend_from_slice(&[0x80 | channel, note.note(), note.value()]);
        }
        Midi1Cv::NoteOn(note) => {
            output.extend_from_slice(&[0x90 | channel, note.note(), note.value()]);
        }
        Midi1Cv::PolyPressure(note) => {
            output.extend_from_slice(&[0xA0 | channel, note.note(), note.value()]);
        }
        Midi1Cv::ControlChange(control) => {
            output.extend_from_slice(&[0xB0 | channel, control.controller(), control.value()]);
        }
        Midi1Cv::ProgramChange(program) => {
            output.extend_from_slice(&[0xC0 | channel, program.program()]);
        }
        Midi1Cv::ChannelPressure(pressure) => {
            output.extend_from_slice(&[0xD0 | channel, pressure.pressure()]);
        }
        Midi1Cv::PitchBend(bend) => {
            let value = bend.value();
            output.extend_from_slice(&[
                0xE0 | channel,
                (value & 0x7F) as u8,
                ((value >> 7) & 0x7F) as u8,
            ]);
        }
        Midi1Cv::Unknown(_) => return Err(SerializeError::UnknownChannelVoice),
    }
    Ok(())
}

fn serialize_system(message: System, output: &mut Vec<u8>) -> Result<(), SerializeError> {
    match message {
        System::MtcQuarterFrame(data) => output.extend_from_slice(&[0xF1, data.value()]),
        System::SongPosition(position) => {
            let value = position.value();
            output.extend_from_slice(&[0xF2, (value & 0x7F) as u8, ((value >> 7) & 0x7F) as u8]);
        }
        System::SongSelect(data) => output.extend_from_slice(&[0xF3, data.value()]),
        System::TuneRequest(_) => output.push(0xF6),
        System::TimingClock(_) => output.push(0xF8),
        System::Start(_) => output.push(0xFA),
        System::Continue(_) => output.push(0xFB),
        System::Stop(_) => output.push(0xFC),
        System::ActiveSensing(_) => output.push(0xFE),
        System::Reset(_) => output.push(0xFF),
        System::Unknown(_) => return Err(SerializeError::UnknownSystem),
    }
    Ok(())
}

fn serialize_sysex(message: &SysEx, output: &mut Vec<u8>) -> Result<(), SerializeError> {
    match message {
        SysEx::Complete(data) => {
            output.push(0xF0);
            output.extend_from_slice(data.data());
            output.push(0xF7);
        }
        SysEx::Start(data) => {
            output.push(0xF0);
            output.extend_from_slice(data.data());
        }
        SysEx::Continue(data) => output.extend_from_slice(data.data()),
        SysEx::End(data) => {
            output.extend_from_slice(data.data());
            output.push(0xF7);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParameterKind {
    Registered,
    Assignable,
}

#[derive(Debug, Clone, Copy)]
struct Selection {
    kind: ParameterKind,
    msb: u8,
    lsb: u8,
}

#[derive(Debug, Clone, Copy)]
struct Selector {
    kind: ParameterKind,
    msb: Option<u8>,
    lsb: Option<u8>,
}

#[derive(Debug, Clone, Copy)]
struct PendingData {
    selection: Selection,
    msb: u8,
    source_tick: u64,
}

#[derive(Debug, Default, Clone, Copy)]
struct ChannelState {
    selector: Option<Selector>,
    pending_data: Option<PendingData>,
    bank_msb: Option<u8>,
    bank_lsb: Option<u8>,
}

/// Canonicalise already parsed events.
///
/// The canonical form:
///
/// - rewrites Note On velocity zero to Note Off velocity 64;
/// - folds RPN/NRPN selectors and data entry, then emits each complete entry
///   as selector MSB, selector LSB, CC6 and CC38 in that order;
/// - supplies CC38 value zero for an MSB-only data entry;
/// - passes System Real Time in place without closing CC6 lookahead;
/// - drops null-RPN and dangling selectors, while leaving CC6/38 without a
///   complete selection and CC96/97 as ordinary Control Change messages;
/// - moves a complete latched bank MSB/LSB pair immediately before Program
///   Change; a lone half survives unbanked Program Changes and is dropped
///   only at end-of-stream;
/// - otherwise preserves event order.
pub fn canonicalize_events(events: &[Event]) -> Vec<Event> {
    canonicalize_timed(
        &events
            .iter()
            .copied()
            .map(|event| TimedEvent { tick: 0, event })
            .collect::<Vec<_>>(),
    )
    .into_iter()
    .map(|event| event.event)
    .collect()
}

/// Canonicalise timestamped events without routing through the MIDI 2
/// translator.
///
/// This is the independent MIDI 1.0 oracle used by the semantic round-trip
/// check. Events introduced by bank folding use the Program Change tick;
/// events introduced by Data Entry folding use the source CC6 tick.
pub fn canonicalize_timed(events: &[TimedEvent]) -> Vec<TimedEvent> {
    let mut states: [ChannelState; 256] = std::array::from_fn(|_| ChannelState::default());
    let mut output = Vec::with_capacity(events.len());

    for timed in events {
        let event = timed.event;
        if !is_realtime_event(&event) {
            let completing_data = data_lsb_key(&event, &states);
            flush_pending_data(&mut states, completing_data, &mut output);
        }

        match event {
            Event::Midi1Cv(message) => {
                canonicalize_cv(message, timed.tick, &mut states, &mut output);
            }
            Event::System(_) | Event::SysEx(_) => output.push(*timed),
        }
    }
    flush_pending_data(&mut states, None, &mut output);
    // A folded Data Entry belongs to its source CC6 tick even when Real Time
    // events passed through during lookahead. Stable sorting restores
    // chronological order while retaining order among equal-tick events.
    output.sort_by_key(|event| event.tick);
    output
}

fn canonicalize_cv(
    message: Midi1Cv,
    tick: u64,
    states: &mut [ChannelState; 256],
    output: &mut Vec<TimedEvent>,
) {
    let group = message.group();
    let channel = message.channel();
    let key = state_key(group, channel);

    match message {
        Midi1Cv::NoteOn(note) if note.value() == 0 => {
            output.push(TimedEvent {
                tick,
                event: Event::Midi1Cv(Midi1Cv::note_off(group, channel, note.note(), 64)),
            });
        }
        Midi1Cv::ControlChange(control) => {
            let controller = control.controller();
            let value = control.value();
            match controller {
                101 | 100 => update_selector(
                    &mut states[key],
                    ParameterKind::Registered,
                    controller == 101,
                    value,
                ),
                99 | 98 => update_selector(
                    &mut states[key],
                    ParameterKind::Assignable,
                    controller == 99,
                    value,
                ),
                6 => {
                    if let Some(selection) = current_selection(&states[key]) {
                        states[key].pending_data = Some(PendingData {
                            selection,
                            msb: value,
                            source_tick: tick,
                        });
                    } else {
                        output.push(TimedEvent {
                            tick,
                            event: Event::Midi1Cv(message),
                        });
                    }
                }
                38 => {
                    if let Some(data) = states[key].pending_data.take() {
                        emit_parameter(group, channel, data, value, output);
                    } else {
                        output.push(TimedEvent {
                            tick,
                            event: Event::Midi1Cv(message),
                        });
                    }
                }
                0 => states[key].bank_msb = Some(value),
                32 => states[key].bank_lsb = Some(value),
                _ => output.push(TimedEvent {
                    tick,
                    event: Event::Midi1Cv(message),
                }),
            }
        }
        Midi1Cv::ProgramChange(_) => {
            let state = &mut states[key];
            // AM_MIDI2.0Lib include/umpToMIDI2Protocol.h requires both
            // sentinel-initialised halves to be below 128 before setting
            // bank-valid, and clears the sentinels only in that branch.
            // REFERENCE-GAP(M2-115-BT): AM is the sole stateful local
            // witness.
            if let (Some(msb), Some(lsb)) = (state.bank_msb, state.bank_lsb) {
                output.push(TimedEvent {
                    tick,
                    event: Event::Midi1Cv(Midi1Cv::control_change(group, channel, 0, msb)),
                });
                output.push(TimedEvent {
                    tick,
                    event: Event::Midi1Cv(Midi1Cv::control_change(group, channel, 32, lsb)),
                });
                state.bank_msb = None;
                state.bank_lsb = None;
            }
            output.push(TimedEvent {
                tick,
                event: Event::Midi1Cv(message),
            });
        }
        _ => output.push(TimedEvent {
            tick,
            event: Event::Midi1Cv(message),
        }),
    }
}

fn update_selector(state: &mut ChannelState, kind: ParameterKind, is_msb: bool, value: u8) {
    let mut selector = match state.selector {
        Some(existing) => existing,
        _ => Selector {
            kind,
            msb: None,
            lsb: None,
        },
    };
    // AM_MIDI2.0Lib `umpToMIDI2Protocol.h` updates one shared selector half
    // at a time. A kind or MSB change deliberately retains the previous LSB.
    selector.kind = kind;
    if is_msb {
        selector.msb = Some(value);
    } else {
        selector.lsb = Some(value);
    }
    state.selector = Some(selector);
}

fn current_selection(state: &ChannelState) -> Option<Selection> {
    let selector = state.selector?;
    let selection = Selection {
        kind: selector.kind,
        msb: selector.msb?,
        lsb: selector.lsb?,
    };
    if selection.kind == ParameterKind::Registered && selection.msb == 127 && selection.lsb == 127 {
        None
    } else {
        Some(selection)
    }
}

fn data_lsb_key(event: &Event, states: &[ChannelState; 256]) -> Option<usize> {
    let Event::Midi1Cv(Midi1Cv::ControlChange(control)) = event else {
        return None;
    };
    if control.controller() != 38 {
        return None;
    }
    let message = match event {
        Event::Midi1Cv(message) => *message,
        _ => unreachable!("matched above"),
    };
    let key = state_key(message.group(), message.channel());
    states[key].pending_data.map(|_| key)
}

fn is_realtime_event(event: &Event) -> bool {
    matches!(
        event,
        Event::System(
            System::TimingClock(_)
                | System::Start(_)
                | System::Continue(_)
                | System::Stop(_)
                | System::ActiveSensing(_)
                | System::Reset(_)
        )
    )
}

fn flush_pending_data(
    states: &mut [ChannelState; 256],
    except: Option<usize>,
    output: &mut Vec<TimedEvent>,
) {
    for (key, state) in states.iter_mut().enumerate() {
        if Some(key) == except {
            continue;
        }
        let Some(data) = state.pending_data.take() else {
            continue;
        };
        let group = (key / 16) as u8;
        let channel = (key % 16) as u8;
        emit_parameter(group, channel, data, 0, output);
    }
}

fn emit_parameter(
    group: u8,
    channel: u8,
    data: PendingData,
    lsb: u8,
    output: &mut Vec<TimedEvent>,
) {
    let (msb_controller, lsb_controller) = match data.selection.kind {
        ParameterKind::Registered => (101, 100),
        ParameterKind::Assignable => (99, 98),
    };
    for (controller, value) in [
        (msb_controller, data.selection.msb),
        (lsb_controller, data.selection.lsb),
        (6, data.msb),
        (38, lsb),
    ] {
        output.push(TimedEvent {
            tick: data.source_tick,
            event: Event::Midi1Cv(Midi1Cv::control_change(group, channel, controller, value)),
        });
    }
}

fn state_key(group: u8, channel: u8) -> usize {
    usize::from(group & 0xF) * 16 + usize::from(channel & 0xF)
}

/// Canonicalise a MIDI 1.0 byte stream.
///
/// Malformed input is treated exactly as by [`parse_raw`]: invalid material
/// is dropped, while the next usable status resynchronises the stream.
pub fn canonicalize(bytes: &[u8]) -> Vec<u8> {
    serialize(&parse(bytes).events).expect("the parser only creates serialisable events")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cv(message: Midi1Cv) -> Event {
        Event::Midi1Cv(message)
    }

    #[test]
    fn ni_channel_voice_vector_and_partial_message_resync() {
        // ni-midi2/tests/midi1_byte_stream_tests.cpp:
        // parser_channel_voice_messages.
        let bytes = [
            0x83, 0x45, 0x6E, 0x9E, 0x30, 0x7F, 0xAA, 0x44, 0x03, 0x77, 0xB0, 0x07, 0x70, 0xC5,
            0x11, 0x12, 0xDB, 0x14, 0xE5, 0x03, 0x72,
        ];
        let parsed = parse_raw(&bytes);
        assert_eq!(
            parsed.events,
            vec![
                cv(Midi1Cv::note_off(0, 3, 0x45, 0x6E)),
                cv(Midi1Cv::note_on(0, 14, 0x30, 0x7F)),
                cv(Midi1Cv::poly_pressure(0, 10, 0x44, 0x03)),
                cv(Midi1Cv::control_change(0, 0, 0x07, 0x70)),
                cv(Midi1Cv::program_change(0, 5, 0x11)),
                cv(Midi1Cv::program_change(0, 5, 0x12)),
                cv(Midi1Cv::channel_pressure(0, 11, 0x14)),
                cv(Midi1Cv::pitch_bend(0, 5, 0x3903)),
            ]
        );
        assert_eq!(parsed.stats.aborted_messages, 1);
    }

    #[test]
    fn parser_assigns_the_configured_group() {
        // ni-midi2/tests/midi1_byte_stream_tests.cpp: parser_group.
        let mut parser = Parser::new(13);
        let mut events = Vec::new();
        parser.feed(&[0x90, 60, 1, 0xF8], &mut events);
        parser.finish();
        assert_eq!(events[0], cv(Midi1Cv::note_on(13, 0, 60, 1)));
        let Event::System(clock) = events[1] else {
            panic!("expected Timing Clock");
        };
        assert_eq!(clock.group(), 13);
    }

    #[test]
    fn ni_realtime_can_split_a_data_pair_without_cancelling_running_status() {
        // ni-midi2/tests/midi1_byte_stream_tests.cpp:
        // parser_system_realtime_intersperes_running_status.
        let bytes = [0xA5, 0x44, 0xFA, 0x03, 0x44, 0x77];
        let parsed = parse_raw(&bytes);
        assert_eq!(
            parsed.events,
            vec![
                Event::System(System::start(0)),
                cv(Midi1Cv::poly_pressure(0, 5, 0x44, 0x03)),
                cv(Midi1Cv::poly_pressure(0, 5, 0x44, 0x77)),
            ]
        );
        assert_eq!(
            serialize(&parsed.events).unwrap(),
            [0xFA, 0xA5, 0x44, 0x03, 0xA5, 0x44, 0x77]
        );
    }

    #[test]
    fn ni_system_common_cancels_running_status_and_resyncs() {
        // ni-midi2/tests/midi1_byte_stream_tests.cpp:
        // parser_system_common_cancels_running_status.
        let bytes = [
            0xC6, 0x11, 0x12, 0xD2, 0x14, 0x64, 0xF6, 0x00, 0xE5, 0x03, 0x72,
        ];
        let parsed = parse_raw(&bytes);
        assert_eq!(
            parsed.events,
            vec![
                cv(Midi1Cv::program_change(0, 6, 0x11)),
                cv(Midi1Cv::program_change(0, 6, 0x12)),
                cv(Midi1Cv::channel_pressure(0, 2, 0x14)),
                cv(Midi1Cv::channel_pressure(0, 2, 0x64)),
                Event::System(System::tune_request(0)),
                cv(Midi1Cv::pitch_bend(0, 5, 0x3903)),
            ]
        );
        assert_eq!(parsed.stats.skipped_bytes, 1);
    }

    #[test]
    fn ni_full_system_common_and_realtime_vector() {
        // ni-midi2/tests/midi1_byte_stream_tests.cpp:
        // parser_system_messages.
        let bytes = [
            0xF8, 0xF9, 0xF1, 0x09, 0xFA, 0xF2, 0x11, 0x44, 0xFB, 0xF3, 0x75, 0xFC, 0xF4, 0x02,
            0xFD, 0xF5, 0x27, 0xFE, 0xF6, 0xFF, 0x33, 0xFA, 0xFE,
        ];
        let parsed = parse_raw(&bytes);
        assert_eq!(
            parsed.events,
            vec![
                Event::System(System::timing_clock(0)),
                Event::System(System::mtc_quarter_frame(0, 0x09)),
                Event::System(System::start(0)),
                Event::System(System::song_position(0, 0x2211)),
                Event::System(System::continue_(0)),
                Event::System(System::song_select(0, 0x75)),
                Event::System(System::stop(0)),
                Event::System(System::active_sensing(0)),
                Event::System(System::tune_request(0)),
                Event::System(System::reset(0)),
                Event::System(System::start(0)),
                Event::System(System::active_sensing(0)),
            ]
        );
        assert_eq!(parsed.stats.skipped_bytes, 7);
        assert_eq!(
            serialize(&parsed.events).unwrap(),
            [
                0xF8, 0xF1, 0x09, 0xFA, 0xF2, 0x11, 0x44, 0xFB, 0xF3, 0x75, 0xFC, 0xFE, 0xF6, 0xFF,
                0xFA, 0xFE,
            ]
        );
    }

    #[test]
    fn ni_sysex_realtime_interleave_retains_wire_position() {
        // ni-midi2/tests/midi1_byte_stream_tests.cpp:
        // parser_sysex_intersperse_system_realtime_packets.
        let bytes = [0xF0, 0x7D, 0xF8, 0x25, 0x50, 0x44, 0xF7, 0xFF];
        let parsed = parse_raw(&bytes);
        assert_eq!(
            parsed.events,
            vec![
                Event::SysEx(SysEx::start(&[0x7D]).unwrap()),
                Event::System(System::timing_clock(0)),
                Event::SysEx(SysEx::end(&[0x25, 0x50, 0x44]).unwrap()),
                Event::System(System::reset(0)),
            ]
        );
        assert_eq!(serialize(&parsed.events).unwrap(), bytes);
    }

    #[test]
    fn stray_f7_is_counted_while_explicit_continuations_remain_serialisable() {
        // AM_MIDI2.0Lib/tests/tests.cpp, Test 12 Scenario 3 says the second
        // F7 should be ignored. ni-midi2 from_system_messages also rejects
        // F7 as a standalone System message.
        let bytes = [0xF0, 1, 2, 0xF7, 0xF7, 0x90, 60, 1];
        let parsed = parse_raw(&bytes);
        assert_eq!(
            parsed.events,
            vec![
                Event::SysEx(SysEx::complete(&[1, 2]).unwrap()),
                cv(Midi1Cv::note_on(0, 0, 60, 1)),
            ]
        );
        assert_eq!(parsed.stats.skipped_bytes, 1);
        assert_eq!(
            serialize(&[
                Event::SysEx(SysEx::continue_(&[3, 4]).unwrap()),
                Event::SysEx(SysEx::end(&[5]).unwrap()),
            ])
            .unwrap(),
            [3, 4, 5, 0xF7]
        );
    }

    #[test]
    fn malformed_stream_resynchronises_and_counts_discards() {
        let bytes = [
            1, 2, 0x90, 60, 0xC1, 7, 0xF4, 9, 0xF0, 1, 2, 0x92, 64, 3, 0xFD, 0xF7,
        ];
        let parsed = parse_raw(&bytes);
        assert_eq!(
            parsed.events,
            vec![
                cv(Midi1Cv::program_change(0, 1, 7)),
                cv(Midi1Cv::note_on(0, 2, 64, 3)),
            ]
        );
        assert_eq!(
            parsed.stats,
            ParseStats {
                skipped_bytes: 6,
                aborted_messages: 1,
                aborted_sysex: 1,
            }
        );
    }

    #[test]
    fn canonicalization_rules_and_exact_parse_serialize_property() {
        let fixtures: &[(&[u8], &[u8])] = &[
            (&[0x90, 60, 0, 61, 2], &[0x80, 60, 64, 0x90, 61, 2]),
            (
                &[0xB0, 100, 2, 101, 1, 6, 64, 38, 5],
                &[0xB0, 101, 1, 0xB0, 100, 2, 0xB0, 6, 64, 0xB0, 38, 5],
            ),
            (
                &[0xB1, 101, 3, 100, 4, 6, 9, 0x91, 60, 1],
                &[
                    0xB1, 101, 3, 0xB1, 100, 4, 0xB1, 6, 9, 0xB1, 38, 0, 0x91, 60, 1,
                ],
            ),
            (
                &[0xB2, 101, 127, 100, 127, 6, 10, 38, 11, 96, 1, 97, 2],
                &[0xB2, 6, 10, 0xB2, 38, 11, 0xB2, 96, 1, 0xB2, 97, 2],
            ),
            (
                &[0xB3, 32, 4, 0x93, 60, 2, 0xB3, 0, 3, 0xC3, 5],
                &[0x93, 60, 2, 0xB3, 0, 3, 0xB3, 32, 4, 0xC3, 5],
            ),
            (&[0xB4, 0, 7, 0xC4, 8], &[0xC4, 8]),
            (
                &[0xB7, 98, 4, 99, 3, 6, 5, 38, 6],
                &[0xB7, 99, 3, 0xB7, 98, 4, 0xB7, 6, 5, 0xB7, 38, 6],
            ),
            (&[0xB5, 99, 1, 98, 2, 0xB6, 0, 4], &[]),
        ];

        for &(input, expected) in fixtures {
            let parsed = parse(input);
            assert_eq!(serialize(&parsed.events).unwrap(), expected);
            assert_eq!(canonicalize(input), expected);
        }
    }

    #[test]
    fn sysex_fragments_at_the_ump_six_byte_boundary() {
        let bytes = [0xF0, 1, 2, 3, 4, 5, 6, 7, 8, 0xF7];
        let parsed = parse_raw(&bytes);
        assert_eq!(
            parsed.events,
            [
                Event::SysEx(SysEx::start(&[1, 2, 3, 4, 5, 6]).unwrap()),
                Event::SysEx(SysEx::end(&[7, 8]).unwrap()),
            ]
        );
        assert_eq!(serialize(&parsed.events).unwrap(), bytes);
        assert!(SysEx::complete(&[1, 2, 3, 4, 5, 6, 7]).is_none());
    }

    #[test]
    fn realtime_does_not_close_cc6_lookahead() {
        let input = [0xB0, 101, 1, 0xB0, 100, 2, 0xB0, 6, 3, 0xF8, 0xB0, 38, 4];
        assert_eq!(
            canonicalize(&input),
            [0xF8, 0xB0, 101, 1, 0xB0, 100, 2, 0xB0, 6, 3, 0xB0, 38, 4,]
        );
    }

    #[test]
    fn am_selector_half_update_is_canonicalised_independently() {
        let input = [
            0xB0, 101, 1, 0xB0, 100, 2, 0xB0, 6, 3, 0xB0, 38, 4, 0xB0, 101, 4, 0xB0, 6, 5,
        ];
        assert_eq!(
            canonicalize(&input),
            [
                0xB0, 101, 1, 0xB0, 100, 2, 0xB0, 6, 3, 0xB0, 38, 4, 0xB0, 101, 4, 0xB0, 100, 2,
                0xB0, 6, 5, 0xB0, 38, 0,
            ]
        );
    }

    #[test]
    fn am_lone_bank_half_survives_unbanked_program_change_canonicalisation() {
        let input = [0xB0, 0, 5, 0xC0, 10, 0xB0, 32, 7, 0xC0, 11];
        assert_eq!(
            canonicalize(&input),
            [0xC0, 10, 0xB0, 0, 5, 0xB0, 32, 7, 0xC0, 11]
        );
    }

    #[test]
    fn canonicalize_is_idempotent() {
        let fixtures: &[&[u8]] = &[
            &[0x90, 60, 0, 61, 2],
            &[0xB0, 100, 2, 101, 1, 6, 64],
            &[0xB3, 32, 4, 0x93, 60, 2, 0xB3, 0, 3, 0xC3, 5],
            &[0xF0, 0x7D, 0xF8, 1, 2, 0xF7],
        ];
        for input in fixtures {
            let once = canonicalize(input);
            assert_eq!(canonicalize(&once), once);
        }
    }

    #[test]
    fn deterministic_byte_soup_never_panics_and_resynchronises() {
        let mut state = 0xC05C_1D12_u32;
        for case in 0..512 {
            let len = (case * 37) % 1024;
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                bytes.push((state >> 24) as u8);
            }

            let raw = parse_raw(&bytes);
            let raw_bytes = serialize(&raw.events).unwrap();
            let canonical = canonicalize(&bytes);
            let parsed = parse(&bytes);
            assert_eq!(serialize(&parsed.events).unwrap(), canonical);
            assert_eq!(canonicalize(&canonical), canonical);

            // Reparse every recovered raw stream: emitted events are always
            // valid and cannot increase malformed-input counters.
            let recovered = parse_raw(&raw_bytes);
            assert_eq!(recovered.stats.skipped_bytes, 0);
            assert_eq!(recovered.stats.aborted_messages, 0);
        }
    }
}
