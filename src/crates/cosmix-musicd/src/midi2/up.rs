//! Stateful MIDI 1.0 byte-stream event → MIDI 2.0 UMP translation.
//!
//! State is isolated by `(group, channel)`. RPN/NRPN selectors, Data Entry
//! lookahead and Bank Select latches are suppressed until their terminal
//! message proves the intended compound operation. System Real Time passes
//! immediately and does not terminate Data Entry lookahead.

use super::{
    bytestream::{Event, SysEx},
    cv1::Midi1Cv,
    cv2::Midi2Cv,
    msg::{Message, SysEx7, SysEx7Format, System},
    scale::{up7to16, up7to32, up14to32},
    translate::{Pending, Translation},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParameterKind {
    Registered,
    Assignable,
}

#[derive(Debug, Clone, Copy)]
struct Selection {
    kind: ParameterKind,
    bank: u8,
    index: u8,
}

#[derive(Debug, Clone, Copy)]
struct Selector {
    kind: ParameterKind,
    bank: Option<u8>,
    index: Option<u8>,
}

#[derive(Debug, Clone, Copy)]
struct PendingData {
    selection: Selection,
    msb: u8,
}

#[derive(Debug, Default, Clone, Copy)]
struct ChannelState {
    selector: Option<Selector>,
    pending_data: Option<PendingData>,
    bank_msb: Option<u8>,
    bank_lsb: Option<u8>,
}

/// Stateful MIDI 1.0 → MIDI 2.0 translator.
///
/// `translate` accepts raw events from [`super::bytestream::parse_raw`].
/// Call [`Self::flush`] at end-of-stream to emit an MSB-only Data Entry and
/// discard selector/bank state that never acquired a terminal message.
#[derive(Debug, Clone)]
pub struct UpTranslator {
    states: [ChannelState; 256],
}

impl Default for UpTranslator {
    fn default() -> Self {
        Self::new()
    }
}

impl UpTranslator {
    /// Construct an empty translator.
    pub fn new() -> Self {
        Self {
            states: std::array::from_fn(|_| ChannelState::default()),
        }
    }

    /// Translate one typed MIDI 1.0 event.
    pub fn translate(&mut self, event: Event) -> Translation<Message> {
        let mut output = Translation::new();
        if is_realtime_event(&event) {
            let Event::System(system) = event else {
                unreachable!("predicate only accepts System Real Time")
            };
            output.push(Message::System(system));
            return output;
        }

        let completing_data = self.data_lsb_key(&event);
        self.flush_pending_data(completing_data, &mut output);

        match event {
            Event::Midi1Cv(message) => self.translate_cv(message, &mut output),
            Event::System(message) => output.push(Message::System(message)),
            Event::SysEx(message) => output.push(Message::SysEx7(sysex7(message))),
        }
        output
    }

    /// Emit an MSB-only Data Entry, then discard all unterminated state.
    ///
    /// REFERENCE-GAP(M2-115-BT): AM_MIDI2.0Lib
    /// `include/umpToMIDI2Protocol.h::checkRPNOnChannel` is the sole local
    /// witness for treating the missing LSB as zero.
    pub fn flush(&mut self) -> Translation<Message> {
        let mut output = Translation::new();
        self.flush_pending_data(None, &mut output);
        for state in &mut self.states {
            *state = ChannelState::default();
        }
        output
    }

    /// Summarise state that can affect later events.
    pub fn pending(&self) -> Pending {
        let mut parameter = 0;
        let mut data = 0;
        let mut bank = 0;
        for state in &self.states {
            parameter += u16::from(state.selector.is_some());
            data += u16::from(state.pending_data.is_some());
            bank += u16::from(state.bank_msb.is_some() || state.bank_lsb.is_some());
        }
        Pending::new(parameter, data, bank)
    }

    fn translate_cv(&mut self, message: Midi1Cv, output: &mut Translation<Message>) {
        let group = message.group();
        let channel = message.channel();
        let key = state_key(group, channel);

        match message {
            Midi1Cv::NoteOff(note) => output.push(Message::Midi2Cv(Midi2Cv::note_off(
                group,
                channel,
                note.note(),
                up7to16(note.value()),
                0,
                0,
            ))),
            Midi1Cv::NoteOn(note) if note.value() == 0 => {
                // AM_MIDI2.0Lib include/umpToMIDI2Protocol.h first rewrites
                // zero-velocity Note On to Note Off velocity 64; ni-midi2
                // as_midi2 tests the same conversion. up7to16(64) = 0x8000.
                output.push(Message::Midi2Cv(Midi2Cv::note_off(
                    group,
                    channel,
                    note.note(),
                    0x8000,
                    0,
                    0,
                )));
            }
            Midi1Cv::NoteOn(note) => output.push(Message::Midi2Cv(Midi2Cv::note_on(
                group,
                channel,
                note.note(),
                up7to16(note.value()),
                0,
                0,
            ))),
            Midi1Cv::PolyPressure(note) => {
                output.push(Message::Midi2Cv(Midi2Cv::poly_pressure(
                    group,
                    channel,
                    note.note(),
                    up7to32(note.value()),
                )));
            }
            Midi1Cv::ControlChange(control) => {
                self.translate_cc(
                    group,
                    channel,
                    control.controller(),
                    control.value(),
                    output,
                );
            }
            Midi1Cv::ProgramChange(program) => {
                let state = &mut self.states[key];
                // AM_MIDI2.0Lib include/umpToMIDI2Protocol.h initialises
                // both halves to 255 and sets bank-valid only when both are
                // below 128. It clears them only after a banked Program
                // Change, so a lone half survives an unbanked one.
                // REFERENCE-GAP(M2-115-BT): AM is the sole stateful witness.
                let bank = match (state.bank_msb, state.bank_lsb) {
                    (Some(msb), Some(lsb)) => {
                        state.bank_msb = None;
                        state.bank_lsb = None;
                        Some(u16::from(msb) << 7 | u16::from(lsb))
                    }
                    _ => None,
                };
                output.push(Message::Midi2Cv(Midi2Cv::program_change(
                    group,
                    channel,
                    program.program(),
                    bank,
                )));
            }
            Midi1Cv::ChannelPressure(pressure) => {
                output.push(Message::Midi2Cv(Midi2Cv::channel_pressure(
                    group,
                    channel,
                    up7to32(pressure.pressure()),
                )));
            }
            Midi1Cv::PitchBend(bend) => {
                output.push(Message::Midi2Cv(Midi2Cv::pitch_bend(
                    group,
                    channel,
                    up14to32(bend.value()),
                )));
            }
            Midi1Cv::Unknown(_) => output.push(Message::Midi1Cv(message)),
        }
    }

    fn translate_cc(
        &mut self,
        group: u8,
        channel: u8,
        controller: u8,
        value: u8,
        output: &mut Translation<Message>,
    ) {
        let key = state_key(group, channel);
        match controller {
            // REFERENCE-GAP(M2-115-BT): AM_MIDI2.0Lib
            // include/umpToMIDI2Protocol.h is the sole local witness for
            // these stateful selector/data-entry rules and controller IDs.
            101 | 100 => {
                self.update_selector(key, ParameterKind::Registered, controller == 101, value)
            }
            99 | 98 => {
                self.update_selector(key, ParameterKind::Assignable, controller == 99, value)
            }
            6 => {
                if let Some(selection) = current_selection(&self.states[key]) {
                    // REFERENCE-GAP(M2-115-BT): AM
                    // umpToMIDI2Protocol.h stores valueMSB and waits exactly
                    // one non-Real-Time message for CC38.
                    self.states[key].pending_data = Some(PendingData {
                        selection,
                        msb: value,
                    });
                } else {
                    emit_plain_cc(group, channel, controller, value, output);
                }
            }
            38 => {
                if let Some(data) = self.states[key].pending_data.take() {
                    emit_parameter(group, channel, data, value, output);
                } else {
                    emit_plain_cc(group, channel, controller, value, output);
                }
            }
            0 => self.states[key].bank_msb = Some(value),
            32 => self.states[key].bank_lsb = Some(value),
            // Decision 2: Data Increment/Decrement remain ordinary CC.
            _ => emit_plain_cc(group, channel, controller, value, output),
        }
    }

    fn update_selector(&mut self, key: usize, kind: ParameterKind, is_bank: bool, value: u8) {
        let state = &mut self.states[key];
        let mut selector = match state.selector {
            Some(existing) => existing,
            _ => Selector {
                kind,
                bank: None,
                index: None,
            },
        };
        // AM_MIDI2.0Lib `umpToMIDI2Protocol.h` stores the two selector
        // halves independently. Changing either RPN/NRPN half changes the
        // kind but deliberately retains the other half.
        selector.kind = kind;
        if is_bank {
            selector.bank = Some(value);
        } else {
            selector.index = Some(value);
        }
        state.selector = Some(selector);
    }

    fn data_lsb_key(&self, event: &Event) -> Option<usize> {
        let Event::Midi1Cv(message @ Midi1Cv::ControlChange(control)) = event else {
            return None;
        };
        if control.controller() != 38 {
            return None;
        }
        let key = state_key(message.group(), message.channel());
        self.states[key].pending_data.map(|_| key)
    }

    fn flush_pending_data(&mut self, except: Option<usize>, output: &mut Translation<Message>) {
        for (key, state) in self.states.iter_mut().enumerate() {
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
}

fn current_selection(state: &ChannelState) -> Option<Selection> {
    let selector = state.selector?;
    let selection = Selection {
        kind: selector.kind,
        bank: selector.bank?,
        index: selector.index?,
    };
    if selection.kind == ParameterKind::Registered
        && selection.bank == 127
        && selection.index == 127
    {
        None
    } else {
        Some(selection)
    }
}

fn emit_plain_cc(
    group: u8,
    channel: u8,
    controller: u8,
    value: u8,
    output: &mut Translation<Message>,
) {
    output.push(Message::Midi2Cv(Midi2Cv::control_change(
        group,
        channel,
        controller,
        up7to32(value),
    )));
}

fn emit_parameter(
    group: u8,
    channel: u8,
    pending: PendingData,
    lsb: u8,
    output: &mut Translation<Message>,
) {
    let data = up14to32(u16::from(pending.msb) << 7 | u16::from(lsb));
    let message = match pending.selection.kind {
        ParameterKind::Registered => Midi2Cv::registered_controller(
            group,
            channel,
            pending.selection.bank,
            pending.selection.index,
            data,
        ),
        ParameterKind::Assignable => Midi2Cv::assignable_controller(
            group,
            channel,
            pending.selection.bank,
            pending.selection.index,
            data,
        ),
    };
    output.push(Message::Midi2Cv(message));
}

fn sysex7(message: SysEx) -> SysEx7 {
    let format = match message {
        SysEx::Complete(_) => SysEx7Format::Complete,
        SysEx::Start(_) => SysEx7Format::Start,
        SysEx::Continue(_) => SysEx7Format::Continue,
        SysEx::End(_) => SysEx7Format::End,
    };
    SysEx7::new(message.group(), format, message.data())
        .expect("byte-stream SysEx fragments are bounded and seven-bit")
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

const fn state_key(group: u8, channel: u8) -> usize {
    (group as usize & 0xF) * 16 + (channel as usize & 0xF)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        midi2::{
            bytestream::{canonicalize, parse_raw, serialize},
            down::DownTranslator,
            msg::{Data128, Utility},
        },
        smf,
    };

    fn cv(message: Midi1Cv) -> Event {
        Event::Midi1Cv(message)
    }

    fn up_events(events: &[Event]) -> Vec<Message> {
        let mut up = UpTranslator::new();
        let mut messages = Vec::new();
        for &event in events {
            messages.extend(up.translate(event));
        }
        messages.extend(up.flush());
        messages
    }

    fn down_messages(messages: &[Message]) -> Vec<Event> {
        let mut down = DownTranslator::new();
        let mut events = Vec::new();
        for &message in messages {
            events.extend(down.translate(message));
        }
        events
    }

    fn assert_identity(bytes: &[u8]) {
        let raw = parse_raw(bytes);
        assert_eq!(
            raw.stats.skipped_bytes, 0,
            "identity fixture contains invalid or stray bytes"
        );
        assert_eq!(
            raw.stats.aborted_messages, 0,
            "identity fixture has an incomplete message"
        );
        assert_eq!(
            raw.stats.aborted_sysex, 0,
            "identity fixture has an incomplete SysEx"
        );
        let messages = up_events(&raw.events);
        let events = down_messages(&messages);
        assert_eq!(serialize(&events).unwrap(), canonicalize(bytes));
    }

    #[test]
    fn note_on_zero_and_all_direct_channel_voice_messages() {
        let events = [
            cv(Midi1Cv::note_off(3, 2, 60, 5)),
            cv(Midi1Cv::note_on(3, 2, 61, 0)),
            cv(Midi1Cv::note_on(3, 2, 62, 6)),
            cv(Midi1Cv::poly_pressure(3, 2, 63, 7)),
            cv(Midi1Cv::control_change(3, 2, 7, 8)),
            cv(Midi1Cv::channel_pressure(3, 2, 9)),
            cv(Midi1Cv::pitch_bend(3, 2, 0x2345)),
        ];
        let messages = up_events(&events);
        assert!(matches!(
            messages[1],
            Message::Midi2Cv(Midi2Cv::NoteOff(note))
                if note.note() == 61 && note.velocity() == 0x8000
        ));
        assert!(matches!(
            messages[4],
            Message::Midi2Cv(Midi2Cv::ControlChange(control))
                if control.controller() == 7 && control.data() == up7to32(8)
        ));
    }

    #[test]
    fn rpn_nrpn_lookahead_repeats_null_and_flush() {
        let events = [
            cv(Midi1Cv::control_change(0, 1, 101, 3)),
            cv(Midi1Cv::control_change(0, 1, 100, 4)),
            cv(Midi1Cv::control_change(0, 1, 6, 5)),
            cv(Midi1Cv::control_change(0, 1, 38, 6)),
            cv(Midi1Cv::control_change(0, 1, 6, 7)),
            cv(Midi1Cv::control_change(0, 1, 38, 8)),
            cv(Midi1Cv::control_change(0, 2, 99, 9)),
            cv(Midi1Cv::control_change(0, 2, 98, 10)),
            cv(Midi1Cv::control_change(0, 2, 6, 11)),
        ];
        let messages = up_events(&events);
        assert_eq!(messages.len(), 3);
        assert!(matches!(
            messages[0],
            Message::Midi2Cv(Midi2Cv::RegisteredController(controller))
                if controller.bank() == 3
                    && controller.index() == 4
                    && controller.data() == up14to32(5 << 7 | 6)
        ));
        assert!(matches!(
            messages[1],
            Message::Midi2Cv(Midi2Cv::RegisteredController(controller))
                if controller.data() == up14to32(7 << 7 | 8)
        ));
        assert!(matches!(
            messages[2],
            Message::Midi2Cv(Midi2Cv::AssignableController(controller))
                if controller.bank() == 9
                    && controller.index() == 10
                    && controller.data() == up14to32(11 << 7)
        ));

        let null = up_events(&[
            cv(Midi1Cv::control_change(0, 0, 101, 127)),
            cv(Midi1Cv::control_change(0, 0, 100, 127)),
            cv(Midi1Cv::control_change(0, 0, 6, 12)),
            cv(Midi1Cv::control_change(0, 0, 38, 13)),
        ]);
        assert!(matches!(
            null.as_slice(),
            [
                Message::Midi2Cv(Midi2Cv::ControlChange(msb)),
                Message::Midi2Cv(Midi2Cv::ControlChange(lsb))
            ] if msb.controller() == 6 && lsb.controller() == 38
        ));
    }

    #[test]
    fn am_selector_half_update_retains_the_other_half() {
        let messages = up_events(&[
            cv(Midi1Cv::control_change(0, 0, 101, 1)),
            cv(Midi1Cv::control_change(0, 0, 100, 2)),
            cv(Midi1Cv::control_change(0, 0, 6, 3)),
            cv(Midi1Cv::control_change(0, 0, 38, 4)),
            cv(Midi1Cv::control_change(0, 0, 101, 4)),
            cv(Midi1Cv::control_change(0, 0, 6, 5)),
        ]);
        assert!(matches!(
            messages.as_slice(),
            [
                Message::Midi2Cv(Midi2Cv::RegisteredController(first)),
                Message::Midi2Cv(Midi2Cv::RegisteredController(second))
            ] if first.bank() == 1
                && first.index() == 2
                && second.bank() == 4
                && second.index() == 2
                && second.data() == up14to32(5 << 7)
        ));
    }

    #[test]
    fn realtime_passes_without_terminating_data_entry() {
        let mut up = UpTranslator::new();
        let events = [
            cv(Midi1Cv::control_change(0, 0, 101, 1)),
            cv(Midi1Cv::control_change(0, 0, 100, 2)),
            cv(Midi1Cv::control_change(0, 0, 6, 3)),
        ];
        for event in events {
            assert!(up.translate(event).is_empty());
        }
        assert_eq!(up.pending().data_entries(), 1);
        let clock = up.translate(Event::System(System::timing_clock(0)));
        assert!(matches!(
            clock.get(0),
            Some(Message::System(System::TimingClock(_)))
        ));
        assert_eq!(up.pending().data_entries(), 1);
        let completed = up.translate(cv(Midi1Cv::control_change(0, 0, 38, 4)));
        assert!(matches!(
            completed.get(0),
            Some(Message::Midi2Cv(Midi2Cv::RegisteredController(controller)))
                if controller.data() == up14to32(3 << 7 | 4)
        ));
    }

    #[test]
    fn bank_requires_both_halves_and_state_is_channel_local() {
        let messages = up_events(&[
            cv(Midi1Cv::control_change(0, 0, 0, 5)),
            cv(Midi1Cv::program_change(0, 0, 10)),
            cv(Midi1Cv::control_change(0, 1, 0, 6)),
            cv(Midi1Cv::control_change(0, 2, 32, 7)),
            cv(Midi1Cv::control_change(0, 1, 32, 8)),
            cv(Midi1Cv::program_change(0, 2, 11)),
            cv(Midi1Cv::program_change(0, 1, 12)),
            cv(Midi1Cv::program_change(0, 3, 13)),
        ]);
        assert!(matches!(
            messages[0],
            Message::Midi2Cv(Midi2Cv::ProgramChange(program))
                if program.program() == 10 && program.bank().is_none()
        ));
        assert!(matches!(
            messages[1],
            Message::Midi2Cv(Midi2Cv::ProgramChange(program))
                if program.program() == 11 && program.bank().is_none()
        ));
        assert!(matches!(
            messages[2],
            Message::Midi2Cv(Midi2Cv::ProgramChange(program))
                if program.program() == 12 && program.bank() == Some(6 << 7 | 8)
        ));
        assert!(matches!(
            messages[3],
            Message::Midi2Cv(Midi2Cv::ProgramChange(program))
                if program.program() == 13 && program.bank().is_none()
        ));
    }

    #[test]
    fn am_lone_bank_half_survives_an_unbanked_program_change() {
        let messages = up_events(&[
            cv(Midi1Cv::control_change(0, 0, 0, 5)),
            cv(Midi1Cv::program_change(0, 0, 10)),
            cv(Midi1Cv::control_change(0, 0, 32, 7)),
            cv(Midi1Cv::program_change(0, 0, 11)),
        ]);
        assert!(matches!(
            messages.as_slice(),
            [
                Message::Midi2Cv(Midi2Cv::ProgramChange(first)),
                Message::Midi2Cv(Midi2Cv::ProgramChange(second))
            ] if first.program() == 10
                && first.bank().is_none()
                && second.program() == 11
                && second.bank() == Some(5 << 7 | 7)
        ));
    }

    #[test]
    fn interleaved_parameter_channels_never_share_state() {
        let messages = up_events(&[
            cv(Midi1Cv::control_change(0, 0, 101, 1)),
            cv(Midi1Cv::control_change(0, 0, 100, 2)),
            cv(Midi1Cv::control_change(0, 1, 99, 3)),
            cv(Midi1Cv::control_change(0, 1, 98, 4)),
            cv(Midi1Cv::control_change(0, 0, 6, 5)),
            cv(Midi1Cv::control_change(0, 1, 38, 6)),
            cv(Midi1Cv::control_change(0, 1, 6, 7)),
            cv(Midi1Cv::control_change(0, 1, 38, 8)),
        ]);
        assert!(matches!(
            messages[0],
            Message::Midi2Cv(Midi2Cv::RegisteredController(controller))
                if controller.bank() == 1
                    && controller.index() == 2
                    && controller.data() == up14to32(5 << 7)
        ));
        assert!(matches!(
            messages[1],
            Message::Midi2Cv(Midi2Cv::ControlChange(controller))
                if controller.controller() == 38
                    && controller.data() == up7to32(6)
        ));
        assert!(matches!(
            messages[2],
            Message::Midi2Cv(Midi2Cv::AssignableController(controller))
                if controller.bank() == 3
                    && controller.index() == 4
                    && controller.data() == up14to32(7 << 7 | 8)
        ));
    }

    #[test]
    fn sysex_framing_and_group_are_preserved() {
        let events = [
            Event::SysEx(SysEx::start_in(7, &[1, 2, 3, 4, 5, 6]).unwrap()),
            Event::SysEx(SysEx::continue_in(7, &[7]).unwrap()),
            Event::SysEx(SysEx::end_in(7, &[8, 9]).unwrap()),
            Event::SysEx(SysEx::complete_in(7, &[10]).unwrap()),
        ];
        let messages = up_events(&events);
        assert!(matches!(messages[0], Message::SysEx7(SysEx7::Start(_))));
        assert!(matches!(messages[1], Message::SysEx7(SysEx7::Continue(_))));
        assert!(matches!(messages[2], Message::SysEx7(SysEx7::End(_))));
        assert!(matches!(messages[3], Message::SysEx7(SysEx7::Complete(_))));
        assert!(messages.iter().all(|message| message.group() == Some(7)));
        assert_eq!(down_messages(&messages), events);
    }

    #[test]
    fn pending_reports_and_flush_clears_every_kind() {
        let mut up = UpTranslator::new();
        let _ = up.translate(cv(Midi1Cv::control_change(0, 2, 0, 5)));
        let _ = up.translate(cv(Midi1Cv::control_change(0, 0, 101, 1)));
        let _ = up.translate(cv(Midi1Cv::control_change(0, 1, 99, 2)));
        let _ = up.translate(cv(Midi1Cv::control_change(0, 1, 98, 3)));
        let _ = up.translate(cv(Midi1Cv::control_change(0, 1, 6, 4)));
        let pending = up.pending();
        assert_eq!(pending.parameter_selections(), 2);
        assert_eq!(pending.data_entries(), 1);
        assert_eq!(pending.bank_selects(), 1);
        let flushed = up.flush();
        assert_eq!(flushed.len(), 1);
        assert!(up.pending().is_empty());
    }

    #[test]
    fn hand_built_every_message_and_idiom_identity() {
        let bytes = [
            0x80, 60, 1, 0x90, 61, 0, 0x90, 62, 2, 0xA0, 63, 3, 0xB0, 7, 4, 0xC0, 5, 0xD0, 6, 0xE0,
            7, 8, 0xB1, 101, 1, 0xB1, 100, 2, 0xB1, 6, 3, 0xB1, 38, 4, 0xB1, 6, 5, 0xF8, 0x91, 64,
            1, 0xB2, 99, 6, 0xB2, 98, 7, 0xB2, 6, 8, 0xB3, 101, 127, 0xB3, 100, 127, 0xB3, 96, 1,
            0xB3, 97, 2, 0xB4, 0, 9, 0xB4, 32, 10, 0xC4, 11, 0xB5, 0, 12, 0xC5, 13, 0xF1, 1, 0xF2,
            2, 3, 0xF3, 4, 0xF6, 0xFA, 0xFB, 0xFC, 0xFE, 0xFF, 0xF0, 0x7D, 1, 2, 3, 4, 5, 6, 7,
            0xF7,
        ];
        assert_identity(&bytes);
    }

    #[test]
    fn every_smf_fixture_track_satisfies_the_identity() {
        let fixture_dir = std::path::PathBuf::from(
            std::env::var_os("CARGO_MANIFEST_DIR")
                .unwrap_or_else(|| env!("CARGO_MANIFEST_DIR").into()),
        )
        .join("tests/fixtures");
        let mut fixture_count = 0;
        let mut track_count = 0;
        for entry in std::fs::read_dir(fixture_dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|value| value.to_str()) != Some("mid") {
                continue;
            }
            fixture_count += 1;
            let file = std::fs::read(&path).unwrap();
            let parsed = smf::parse(&file).unwrap();
            for track in &parsed.tracks {
                let wire = smf_track_wire(track);
                assert_identity(&wire);
                track_count += 1;
            }
        }
        assert!(fixture_count > 0);
        assert!(track_count > 0);
    }

    #[test]
    fn ten_thousand_seeded_random_midi1_messages_satisfy_identity() {
        let mut state = 0x4D49_4449_u32;
        let mut bytes = Vec::with_capacity(30_000);
        let mut messages = 0;
        while messages < 10_000 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let channel = (state >> 28) as u8;
            let a = ((state >> 8) & 0x7F) as u8;
            let b = ((state >> 16) & 0x7F) as u8;
            let choice = (state as usize >> 4) % 13;
            let needed = match choice {
                7 => 4,
                8 => 6,
                9 => 4,
                10 => 4,
                11 => 5,
                _ => 1,
            };
            if messages + needed > 10_000 {
                bytes.extend_from_slice(&[0x90 | channel, a, b]);
                messages += 1;
                continue;
            }
            match choice {
                0 => bytes.extend_from_slice(&[0x80 | channel, a, b]),
                1 => bytes.extend_from_slice(&[0x90 | channel, a, b]),
                2 => bytes.extend_from_slice(&[0xA0 | channel, a, b]),
                3 => bytes.extend_from_slice(&[0xB0 | channel, a, b]),
                4 => bytes.extend_from_slice(&[0xC0 | channel, a]),
                5 => bytes.extend_from_slice(&[0xD0 | channel, a]),
                6 => bytes.extend_from_slice(&[0xE0 | channel, a, b]),
                7 => bytes.extend_from_slice(&[
                    0xB0 | channel,
                    101,
                    a,
                    0xB0 | channel,
                    100,
                    b,
                    0xB0 | channel,
                    6,
                    a,
                    0xB0 | channel,
                    38,
                    b,
                ]),
                8 => bytes.extend_from_slice(&[
                    0xB0 | channel,
                    99,
                    a,
                    0xB0 | channel,
                    98,
                    b,
                    0xB0 | channel,
                    6,
                    a,
                    0xB0 | channel,
                    38,
                    b,
                    0xB0 | channel,
                    99,
                    b,
                    0xB0 | channel,
                    6,
                    a,
                ]),
                9 => bytes.extend_from_slice(&[
                    0xB0 | channel,
                    101,
                    127,
                    0xB0 | channel,
                    100,
                    127,
                    0xB0 | channel,
                    6,
                    a,
                    0xB0 | channel,
                    38,
                    b,
                ]),
                10 => bytes.extend_from_slice(&[
                    0xB0 | channel,
                    0,
                    a,
                    0xC0 | channel,
                    b,
                    0xB0 | channel,
                    32,
                    b,
                    0xC0 | channel,
                    a,
                ]),
                11 => bytes.extend_from_slice(&[
                    0xB0 | channel,
                    38,
                    b,
                    0xB0 | channel,
                    101,
                    a,
                    0xB0 | channel,
                    100,
                    b,
                    0xB0 | channel,
                    6,
                    a,
                    0xB0 | channel,
                    38,
                    b,
                ]),
                _ => {
                    bytes.extend_from_slice(&[0xF0, 0x7D, a, b, 0xF7]);
                }
            }
            messages += needed;
        }
        assert_identity(&bytes);
    }

    #[test]
    fn down_up_projection_is_fixed_after_the_first_cycle() {
        let source = [
            Message::Midi2Cv(Midi2Cv::note_on(0, 0, 60, 1, 1, 2)),
            Message::Midi2Cv(Midi2Cv::note_off(0, 0, 60, 0x9234, 0, 0)),
            Message::Midi2Cv(Midi2Cv::registered_controller(0, 0, 1, 2, 0x9234_5678)),
            Message::Midi2Cv(Midi2Cv::program_change(0, 0, 10, Some(0x1234))),
            Message::Midi2Cv(Midi2Cv::per_note_pitch_bend(0, 0, 60, 1)),
            Message::Utility(Utility::jr_timestamp(1)),
            Message::Data128(Data128::sysex8(0, SysEx7Format::Complete, 1, &[2, 3]).unwrap()),
            Message::SysEx7(SysEx7::new(0, SysEx7Format::Complete, &[0x7D, 1]).unwrap()),
        ];
        let first = up_events(&down_messages(&source));
        let second = up_events(&down_messages(&first));
        assert_eq!(second, first);
    }

    /// Convert one raw SMF track body to the wire-MIDI bytes it contains.
    ///
    /// Delta times and meta events (including tempo and End Of Track) are
    /// skipped because SMF `FF` is length-delimited metadata, not MIDI wire
    /// System Reset. F0 chunks retain F0; F7 continuation/escape chunks
    /// contribute only their length-delimited payload.
    fn smf_track_wire(track: &[u8]) -> Vec<u8> {
        let mut position = 0;
        let mut running = None;
        let mut output = Vec::new();
        while position < track.len() {
            read_varlen(track, &mut position);
            let first = track[position];
            let status = if first & 0x80 != 0 {
                position += 1;
                first
            } else {
                running.expect("SMF running status without a channel status")
            };
            match status {
                0x80..=0xEF => {
                    running = Some(status);
                    let len = if status & 0xE0 == 0xC0 { 1 } else { 2 };
                    output.push(status);
                    output.extend_from_slice(&track[position..position + len]);
                    position += len;
                }
                0xFF => {
                    running = None;
                    let kind = track[position];
                    position += 1;
                    let len = read_varlen(track, &mut position);
                    position += len;
                    if kind == 0x2F {
                        break;
                    }
                }
                0xF0 | 0xF7 => {
                    running = None;
                    let len = read_varlen(track, &mut position);
                    if status == 0xF0 {
                        output.push(0xF0);
                    }
                    output.extend_from_slice(&track[position..position + len]);
                    position += len;
                }
                _ => panic!("unsupported SMF status 0x{status:02X}"),
            }
        }
        output
    }

    fn read_varlen(bytes: &[u8], position: &mut usize) -> usize {
        let mut value = 0;
        loop {
            let byte = bytes[*position];
            *position += 1;
            value = (value << 7) | usize::from(byte & 0x7F);
            if byte & 0x80 == 0 {
                return value;
            }
        }
    }
}
