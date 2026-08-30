//! MIDI 2.0 UMP → canonical MIDI 1.0 event translation.
//!
//! Representable Channel Voice messages use the default truncating scales.
//! Each absolute Registered/Assignable Controller expands to the full
//! four-CC sequence, deliberately without selector caching. MIDI 2-only
//! semantics are counted in [`Dropped`] rather than disappearing silently.

use super::{
    bytestream::{Event, SysEx},
    cv1::Midi1Cv,
    cv2::Midi2Cv,
    msg::{Data128, Message, SysEx7, System, Utility},
    scale::{down16to7, down32to7, down32to14},
    translate::Translation,
};

/// Loss counters accumulated during down-translation and SMF projection.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Dropped {
    /// Registered or Assignable Per-Note Controllers.
    pub per_note_controllers: u64,
    /// Per-Note Pitch Bend messages.
    pub per_note_pitch_bend: u64,
    /// Per-Note Management messages.
    pub per_note_management: u64,
    /// Note attribute type/data discarded while retaining the note.
    pub note_attributes: u64,
    /// Relative Registered or Assignable Controllers.
    pub relative_controllers: u64,
    /// Data128 packets, including SysEx8 and Mixed Data Set framing.
    pub data128: u64,
    /// JR Clock and JR Timestamp Utility messages.
    pub jr_utility: u64,
    /// Other Utility messages without a MIDI 1.0 wire representation.
    pub other_utility: u64,
    /// Multipart SysEx runs whose timing or legal Real-Time ordering changes
    /// when projected atomically at their Start tick.
    pub sysex_timing: u64,
    /// Non-zero UMP groups projected onto groupless SMF/MIDI 1.0 output.
    pub group_routing: u64,
    /// Unknown status or Message Type packets.
    pub unknown: u64,
}

/// Stateful only in its loss counters; MIDI 2.0 → MIDI 1.0 translation does
/// not cache parameter selections.
#[derive(Debug, Default, Clone)]
pub struct DownTranslator {
    dropped: Dropped,
}

impl DownTranslator {
    /// Construct a translator with zero loss counters.
    pub const fn new() -> Self {
        Self {
            dropped: Dropped {
                per_note_controllers: 0,
                per_note_pitch_bend: 0,
                per_note_management: 0,
                note_attributes: 0,
                relative_controllers: 0,
                data128: 0,
                jr_utility: 0,
                other_utility: 0,
                sysex_timing: 0,
                group_routing: 0,
                unknown: 0,
            },
        }
    }

    /// Translate one UMP message to zero through four MIDI 1.0 events.
    pub fn translate(&mut self, message: Message) -> Translation<Event> {
        match message {
            Message::Utility(utility) => self.translate_utility(utility),
            Message::System(system) => self.translate_system(system),
            // Decision 6's MT2 passthrough: no scaling or semantic folding.
            Message::Midi1Cv(message) => {
                if matches!(message, Midi1Cv::Unknown(_)) {
                    increment(&mut self.dropped.unknown);
                    Translation::new()
                } else {
                    Translation::one(Event::Midi1Cv(message))
                }
            }
            Message::SysEx7(message) => self.translate_sysex(message),
            Message::Midi2Cv(message) => self.translate_cv(message),
            Message::Data128(message) => {
                self.drop_data128(message);
                Translation::new()
            }
            Message::Unknown(_) => {
                increment(&mut self.dropped.unknown);
                Translation::new()
            }
        }
    }

    /// Current cumulative loss counters.
    pub const fn dropped(&self) -> Dropped {
        self.dropped
    }

    /// Return and clear the cumulative loss counters.
    pub fn take_dropped(&mut self) -> Dropped {
        std::mem::take(&mut self.dropped)
    }

    fn translate_utility(&mut self, utility: Utility) -> Translation<Event> {
        match utility {
            // Decision 12: JR timing is outside MIDI 1.0 wire semantics.
            Utility::JrClock(_) | Utility::JrTimestamp(_) => {
                increment(&mut self.dropped.jr_utility)
            }
            Utility::Unknown(_) => increment(&mut self.dropped.unknown),
            Utility::NoOp(_) | Utility::DeltaClockstampTpq(_) | Utility::DeltaClockstamp(_) => {
                increment(&mut self.dropped.other_utility)
            }
        }
        Translation::new()
    }

    fn translate_system(&mut self, system: System) -> Translation<Event> {
        if matches!(system, System::Unknown(_)) {
            increment(&mut self.dropped.unknown);
            Translation::new()
        } else {
            Translation::one(Event::System(system))
        }
    }

    fn translate_sysex(&mut self, message: SysEx7) -> Translation<Event> {
        let event = match message {
            SysEx7::Complete(packet) => {
                SysEx::complete_in(message.group(), packet.data()).map(Event::SysEx)
            }
            SysEx7::Start(packet) => {
                SysEx::start_in(message.group(), packet.data()).map(Event::SysEx)
            }
            SysEx7::Continue(packet) => {
                SysEx::continue_in(message.group(), packet.data()).map(Event::SysEx)
            }
            SysEx7::End(packet) => SysEx::end_in(message.group(), packet.data()).map(Event::SysEx),
            SysEx7::Unknown(_) => {
                increment(&mut self.dropped.unknown);
                None
            }
        };
        event.map_or_else(Translation::new, Translation::one)
    }

    fn translate_cv(&mut self, message: Midi2Cv) -> Translation<Event> {
        let group = message.group();
        let channel = message.channel();
        match message {
            Midi2Cv::NoteOff(note) => {
                // Decision 12: MIDI 1.0 cannot carry note attributes.
                self.count_attribute_loss(note.attribute_type(), note.attribute_data());
                Translation::one(Event::Midi1Cv(Midi1Cv::note_off(
                    group,
                    channel,
                    note.note(),
                    down16to7(note.velocity()),
                )))
            }
            Midi2Cv::NoteOn(note) => {
                // Decision 12: MIDI 1.0 cannot carry note attributes.
                self.count_attribute_loss(note.attribute_type(), note.attribute_data());
                let velocity = down16to7(note.velocity()).max(1);
                // AM_MIDI2.0Lib include/umpToMIDI1Protocol.h and ni-midi2
                // convert_to_midi1_note_on_zero_velocity both clamp to one.
                Translation::one(Event::Midi1Cv(Midi1Cv::note_on(
                    group,
                    channel,
                    note.note(),
                    velocity,
                )))
            }
            Midi2Cv::PolyPressure(note) => Translation::one(Event::Midi1Cv(
                Midi1Cv::poly_pressure(group, channel, note.note(), down32to7(note.data())),
            )),
            Midi2Cv::ControlChange(control) => {
                Translation::one(Event::Midi1Cv(Midi1Cv::control_change(
                    group,
                    channel,
                    control.controller(),
                    down32to7(control.data()),
                )))
            }
            Midi2Cv::RegisteredController(controller) => emit_parameter(
                group,
                channel,
                101,
                100,
                controller.bank(),
                controller.index(),
                controller.data(),
            ),
            Midi2Cv::AssignableController(controller) => emit_parameter(
                group,
                channel,
                99,
                98,
                controller.bank(),
                controller.index(),
                controller.data(),
            ),
            Midi2Cv::ProgramChange(program) => {
                let mut output = Translation::new();
                if let Some(bank) = program.bank() {
                    output.push(Event::Midi1Cv(Midi1Cv::control_change(
                        group,
                        channel,
                        0,
                        ((bank >> 7) & 0x7F) as u8,
                    )));
                    output.push(Event::Midi1Cv(Midi1Cv::control_change(
                        group,
                        channel,
                        32,
                        (bank & 0x7F) as u8,
                    )));
                }
                output.push(Event::Midi1Cv(Midi1Cv::program_change(
                    group,
                    channel,
                    program.program(),
                )));
                output
            }
            Midi2Cv::ChannelPressure(pressure) => Translation::one(Event::Midi1Cv(
                Midi1Cv::channel_pressure(group, channel, down32to7(pressure.data())),
            )),
            Midi2Cv::PitchBend(bend) => Translation::one(Event::Midi1Cv(Midi1Cv::pitch_bend(
                group,
                channel,
                down32to14(bend.data()),
            ))),
            // Decision 12: MIDI 1.0 has no equivalent per-note controller.
            Midi2Cv::RegisteredPerNoteController(_) | Midi2Cv::AssignablePerNoteController(_) => {
                increment(&mut self.dropped.per_note_controllers);
                Translation::new()
            }
            Midi2Cv::PerNotePitchBend(_) => {
                // FUTURE(mpe-down): map to member-channel Pitch Bend only
                // when an explicit MPE zone/allocation policy exists.
                increment(&mut self.dropped.per_note_pitch_bend);
                Translation::new()
            }
            // Decision 12: no MIDI 1.0 Per-Note Management message exists.
            Midi2Cv::PerNoteManagement(_) => {
                increment(&mut self.dropped.per_note_management);
                Translation::new()
            }
            // Decision 2 only governs M1 CC96/97 on the up path; M2 relative
            // controllers have no lossless absolute MIDI 1 representation.
            Midi2Cv::RelativeRegisteredController(_) | Midi2Cv::RelativeAssignableController(_) => {
                increment(&mut self.dropped.relative_controllers);
                Translation::new()
            }
            Midi2Cv::Unknown(_) => {
                increment(&mut self.dropped.unknown);
                Translation::new()
            }
        }
    }

    fn count_attribute_loss(&mut self, attribute_type: u8, attribute_data: u16) {
        if attribute_type != 0 || attribute_data != 0 {
            increment(&mut self.dropped.note_attributes);
        }
    }

    fn drop_data128(&mut self, _message: Data128) {
        // Decision 12: Phase 1 has SysEx7 byte-stream support only.
        increment(&mut self.dropped.data128);
    }
}

fn emit_parameter(
    group: u8,
    channel: u8,
    bank_controller: u8,
    index_controller: u8,
    bank: u8,
    index: u8,
    data: u32,
) -> Translation<Event> {
    // Decision 6: always emit all four CCs. AM_MIDI2.0Lib
    // include/umpToMIDI1Protocol.h does the same, avoiding history-dependent
    // output and making the canonical identity proof local.
    // REFERENCE-GAP(M2-115-BT): AM is the sole local stateful witness.
    let value = down32to14(data);
    let mut output = Translation::new();
    for (controller, data) in [
        (bank_controller, bank),
        (index_controller, index),
        (6, ((value >> 7) & 0x7F) as u8),
        (38, (value & 0x7F) as u8),
    ] {
        output.push(Event::Midi1Cv(Midi1Cv::control_change(
            group, channel, controller, data,
        )));
    }
    output
}

fn increment(counter: &mut u64) {
    *counter = counter.saturating_add(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::midi2::{
        bytestream::serialize,
        msg::{Data128, SysEx7Format},
        ump::Ump,
    };

    fn bytes(translator: &mut DownTranslator, message: Message) -> Vec<u8> {
        let events = translator
            .translate(message)
            .into_iter()
            .collect::<Vec<_>>();
        serialize(&events).unwrap()
    }

    #[test]
    fn channel_voice_down_vectors_and_note_on_clamp() {
        let mut down = DownTranslator::new();
        let vectors = [
            (
                Midi2Cv::note_off(0, 2, 60, 0x8000, 0, 0),
                vec![0x82, 60, 64],
            ),
            (Midi2Cv::note_on(0, 2, 61, 0, 0, 0), vec![0x92, 61, 1]),
            (
                Midi2Cv::poly_pressure(0, 2, 62, 0xC000_0000),
                vec![0xA2, 62, 96],
            ),
            (
                Midi2Cv::control_change(0, 2, 7, 0xFE00_0000),
                vec![0xB2, 7, 127],
            ),
            (Midi2Cv::program_change(0, 2, 10, None), vec![0xC2, 10]),
            (Midi2Cv::channel_pressure(0, 2, 0x4000_0000), vec![0xD2, 32]),
            (Midi2Cv::pitch_bend(0, 2, 0x8000_0000), vec![0xE2, 0, 64]),
        ];
        for (message, expected) in vectors {
            assert_eq!(bytes(&mut down, Message::Midi2Cv(message)), expected);
        }
        assert_eq!(
            bytes(
                &mut down,
                Message::Midi2Cv(Midi2Cv::program_change(0, 2, 10, Some(0x1234)))
            ),
            [0xB2, 0, 0x24, 0xB2, 32, 0x34, 0xC2, 10]
        );
    }

    #[test]
    fn registered_and_assignable_controllers_always_expand_to_four_ccs() {
        let mut down = DownTranslator::new();
        assert_eq!(
            bytes(
                &mut down,
                Message::Midi2Cv(Midi2Cv::registered_controller(0, 3, 1, 2, 0xC014_0000,))
            ),
            [0xB3, 101, 1, 0xB3, 100, 2, 0xB3, 6, 96, 0xB3, 38, 5]
        );
        assert_eq!(
            bytes(
                &mut down,
                Message::Midi2Cv(Midi2Cv::assignable_controller(0, 3, 4, 5, 0x0C1C_0000,))
            ),
            [0xB3, 99, 4, 0xB3, 98, 5, 0xB3, 6, 6, 0xB3, 38, 7]
        );
    }

    #[test]
    fn mt2_system_and_sysex7_pass_through() {
        let mut down = DownTranslator::new();
        assert_eq!(
            bytes(&mut down, Message::Midi1Cv(Midi1Cv::note_on(0, 4, 64, 9))),
            [0x94, 64, 9]
        );
        assert_eq!(
            bytes(&mut down, Message::System(System::song_position(0, 0x1234))),
            [0xF2, 0x34, 0x24]
        );
        let packet = SysEx7::new(0, SysEx7Format::Complete, &[0x7D, 1, 2]).unwrap();
        assert_eq!(
            bytes(&mut down, Message::SysEx7(packet)),
            [0xF0, 0x7D, 1, 2, 0xF7]
        );
    }

    #[test]
    fn unknown_mt2_channel_voice_is_counted_and_dropped_instead_of_aborting_export() {
        let packet = Ump::from_words(&[0x2070_0000]).unwrap();
        let message = Message::decode(packet);
        assert!(matches!(message, Message::Midi1Cv(Midi1Cv::Unknown(_))));

        let mut down = DownTranslator::new();
        assert!(down.translate(message).is_empty());
        assert_eq!(down.dropped().unknown, 1);
    }

    #[test]
    fn every_midi2_only_loss_is_counted() {
        let mut down = DownTranslator::new();
        let messages = [
            Message::Midi2Cv(Midi2Cv::registered_per_note_controller(0, 0, 60, 1, 2)),
            Message::Midi2Cv(Midi2Cv::assignable_per_note_controller(0, 0, 60, 1, 2)),
            Message::Midi2Cv(Midi2Cv::per_note_pitch_bend(0, 0, 60, 2)),
            Message::Midi2Cv(Midi2Cv::per_note_management(0, 0, 60, 3)),
            Message::Midi2Cv(Midi2Cv::relative_registered_controller(0, 0, 1, 2, -1)),
            Message::Midi2Cv(Midi2Cv::relative_assignable_controller(0, 0, 1, 2, 1)),
            Message::Utility(Utility::jr_clock(1)),
            Message::Utility(Utility::delta_clockstamp(1)),
            Message::Data128(Data128::sysex8(0, SysEx7Format::Complete, 1, &[2]).unwrap()),
            Message::Midi2Cv(Midi2Cv::note_on(0, 0, 60, 0x8000, 1, 2)),
        ];
        for message in messages {
            let _ = down.translate(message);
        }
        assert_eq!(
            down.dropped(),
            Dropped {
                per_note_controllers: 2,
                per_note_pitch_bend: 1,
                per_note_management: 1,
                note_attributes: 1,
                relative_controllers: 2,
                data128: 1,
                jr_utility: 1,
                other_utility: 1,
                sysex_timing: 0,
                group_routing: 0,
                unknown: 0,
            }
        );
        assert_eq!(down.take_dropped().note_attributes, 1);
        assert_eq!(down.dropped(), Dropped::default());
    }
}
