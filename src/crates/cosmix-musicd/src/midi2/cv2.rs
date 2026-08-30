//! MIDI 2.0 Protocol Channel Voice messages (UMP Message Type `0x4`).
//!
//! Every defined status is represented. Semantic accessors mask fields to
//! their specified widths, while each variant retains the decoded [`Ump`]
//! privately so reserved bits round-trip unchanged. Constructors emit the
//! canonical form with reserved bits zero.

use super::ump::Ump;

/// Note On or Note Off data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoteMessage {
    packet: Ump,
    note: u8,
    velocity: u16,
    attribute_type: u8,
    attribute_data: u16,
}

impl NoteMessage {
    /// Note number, `0..=127`.
    pub const fn note(self) -> u8 {
        self.note
    }

    /// Sixteen-bit velocity.
    pub const fn velocity(self) -> u16 {
        self.velocity
    }

    /// Attribute type byte.
    pub const fn attribute_type(self) -> u8 {
        self.attribute_type
    }

    /// Sixteen-bit attribute payload.
    pub const fn attribute_data(self) -> u16 {
        self.attribute_data
    }
}

/// A note-indexed 32-bit value: Poly Pressure or Per-Note Pitch Bend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerNoteValue {
    packet: Ump,
    note: u8,
    data: u32,
}

impl PerNoteValue {
    /// Note number, `0..=127`.
    pub const fn note(self) -> u8 {
        self.note
    }

    /// Full-width value.
    pub const fn data(self) -> u32 {
        self.data
    }
}

/// Registered or Assignable Per-Note Controller data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerNoteController {
    packet: Ump,
    note: u8,
    controller: u8,
    data: u32,
}

impl PerNoteController {
    /// Note number, `0..=127`.
    pub const fn note(self) -> u8 {
        self.note
    }

    /// Controller index, `0..=127`.
    pub const fn controller(self) -> u8 {
        self.controller
    }

    /// Full-width controller value.
    pub const fn data(self) -> u32 {
        self.data
    }
}

/// Per-Note Management flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerNoteManagement {
    packet: Ump,
    note: u8,
    flags: u8,
}

impl PerNoteManagement {
    /// Note number, `0..=127`.
    pub const fn note(self) -> u8 {
        self.note
    }

    /// Defined flag bits: bit 0 reset, bit 1 detach.
    pub const fn flags(self) -> u8 {
        self.flags
    }

    /// Whether receivers should reset per-note controllers.
    pub const fn reset(self) -> bool {
        self.flags & 0x1 != 0
    }

    /// Whether receivers should detach controllers from subsequent notes.
    pub const fn detach(self) -> bool {
        self.flags & 0x2 != 0
    }
}

/// MIDI 2.0 Control Change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlChange {
    packet: Ump,
    controller: u8,
    data: u32,
}

impl ControlChange {
    /// Controller index, `0..=127`.
    pub const fn controller(self) -> u8 {
        self.controller
    }

    /// Full-width controller value.
    pub const fn data(self) -> u32 {
        self.data
    }
}

/// Absolute Registered or Assignable Controller data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Controller {
    packet: Ump,
    bank: u8,
    index: u8,
    data: u32,
}

impl Controller {
    /// Controller bank, `0..=127`.
    pub const fn bank(self) -> u8 {
        self.bank
    }

    /// Controller index, `0..=127`.
    pub const fn index(self) -> u8 {
        self.index
    }

    /// Full-width controller value.
    pub const fn data(self) -> u32 {
        self.data
    }
}

/// Relative Registered or Assignable Controller data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelativeController {
    packet: Ump,
    bank: u8,
    index: u8,
    delta: i32,
}

impl RelativeController {
    /// Controller bank, `0..=127`.
    pub const fn bank(self) -> u8 {
        self.bank
    }

    /// Controller index, `0..=127`.
    pub const fn index(self) -> u8 {
        self.index
    }

    /// Signed two's-complement increment/decrement.
    pub const fn delta(self) -> i32 {
        self.delta
    }
}

/// MIDI 2.0 Program Change, optionally carrying a bank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgramChange {
    packet: Ump,
    program: u8,
    bank: Option<u16>,
}

impl ProgramChange {
    /// Program number, `0..=127`.
    pub const fn program(self) -> u8 {
        self.program
    }

    /// Fourteen-bit bank (`MSB << 7 | LSB`) when bank-valid is set.
    pub const fn bank(self) -> Option<u16> {
        self.bank
    }
}

/// A channel-scoped 32-bit value: Channel Pressure or Pitch Bend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelValue {
    packet: Ump,
    data: u32,
}

impl ChannelValue {
    /// Full-width value.
    pub const fn data(self) -> u32 {
        self.data
    }
}

/// The complete MIDI 2.0 Channel Voice message set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Midi2Cv {
    /// Registered Per-Note Controller.
    RegisteredPerNoteController(PerNoteController),
    /// Assignable Per-Note Controller.
    AssignablePerNoteController(PerNoteController),
    /// Registered Controller.
    RegisteredController(Controller),
    /// Assignable Controller.
    AssignableController(Controller),
    /// Relative Registered Controller.
    RelativeRegisteredController(RelativeController),
    /// Relative Assignable Controller.
    RelativeAssignableController(RelativeController),
    /// Per-Note Pitch Bend.
    PerNotePitchBend(PerNoteValue),
    /// Note Off.
    NoteOff(NoteMessage),
    /// Note On.
    NoteOn(NoteMessage),
    /// Polyphonic Key Pressure.
    PolyPressure(PerNoteValue),
    /// Control Change.
    ControlChange(ControlChange),
    /// Program Change.
    ProgramChange(ProgramChange),
    /// Channel Pressure.
    ChannelPressure(ChannelValue),
    /// Channel Pitch Bend.
    PitchBend(ChannelValue),
    /// Per-Note Management.
    PerNoteManagement(PerNoteManagement),
    /// Undefined status `0x7`, retained without interpretation.
    Unknown(Ump),
}

impl Midi2Cv {
    /// Construct Note Off.
    pub fn note_off(
        group: u8,
        channel: u8,
        note: u8,
        velocity: u16,
        attribute_type: u8,
        attribute_data: u16,
    ) -> Self {
        Self::note_message(
            0x8,
            group,
            channel,
            note,
            velocity,
            attribute_type,
            attribute_data,
        )
    }

    /// Construct Note On.
    pub fn note_on(
        group: u8,
        channel: u8,
        note: u8,
        velocity: u16,
        attribute_type: u8,
        attribute_data: u16,
    ) -> Self {
        Self::note_message(
            0x9,
            group,
            channel,
            note,
            velocity,
            attribute_type,
            attribute_data,
        )
    }

    /// Construct Polyphonic Key Pressure.
    pub fn poly_pressure(group: u8, channel: u8, note: u8, data: u32) -> Self {
        let packet = packet(group, 0xA, channel, note, 0, data);
        Self::PolyPressure(PerNoteValue {
            packet,
            note: note & 0x7F,
            data,
        })
    }

    /// Construct Registered Per-Note Controller.
    pub fn registered_per_note_controller(
        group: u8,
        channel: u8,
        note: u8,
        controller: u8,
        data: u32,
    ) -> Self {
        Self::per_note_controller(0x0, group, channel, note, controller, data)
    }

    /// Construct Assignable Per-Note Controller.
    pub fn assignable_per_note_controller(
        group: u8,
        channel: u8,
        note: u8,
        controller: u8,
        data: u32,
    ) -> Self {
        Self::per_note_controller(0x1, group, channel, note, controller, data)
    }

    /// Construct Per-Note Management. Only flag bits 0 and 1 are emitted.
    pub fn per_note_management(group: u8, channel: u8, note: u8, flags: u8) -> Self {
        let flags = flags & 0x3;
        let packet = packet(group, 0xF, channel, note, flags, 0);
        Self::PerNoteManagement(PerNoteManagement {
            packet,
            note: note & 0x7F,
            flags,
        })
    }

    /// Construct Control Change.
    pub fn control_change(group: u8, channel: u8, controller: u8, data: u32) -> Self {
        let packet = packet(group, 0xB, channel, controller, 0, data);
        Self::ControlChange(ControlChange {
            packet,
            controller: controller & 0x7F,
            data,
        })
    }

    /// Construct Registered Controller.
    pub fn registered_controller(group: u8, channel: u8, bank: u8, index: u8, data: u32) -> Self {
        Self::controller(0x2, group, channel, bank, index, data)
    }

    /// Construct Assignable Controller.
    pub fn assignable_controller(group: u8, channel: u8, bank: u8, index: u8, data: u32) -> Self {
        Self::controller(0x3, group, channel, bank, index, data)
    }

    /// Construct Relative Registered Controller.
    pub fn relative_registered_controller(
        group: u8,
        channel: u8,
        bank: u8,
        index: u8,
        delta: i32,
    ) -> Self {
        Self::relative_controller(0x4, group, channel, bank, index, delta)
    }

    /// Construct Relative Assignable Controller.
    pub fn relative_assignable_controller(
        group: u8,
        channel: u8,
        bank: u8,
        index: u8,
        delta: i32,
    ) -> Self {
        Self::relative_controller(0x5, group, channel, bank, index, delta)
    }

    /// Construct Program Change. `bank` is a fourteen-bit `MSB << 7 | LSB`.
    pub fn program_change(group: u8, channel: u8, program: u8, bank: Option<u16>) -> Self {
        let program = program & 0x7F;
        let bank = bank.map(|value| value & 0x3FFF);
        let (options, data) = match bank {
            Some(value) => (
                1,
                u32::from(program) << 24
                    | u32::from((value >> 7) & 0x7F) << 8
                    | u32::from(value & 0x7F),
            ),
            None => (0, u32::from(program) << 24),
        };
        let packet = packet(group, 0xC, channel, 0, options, data);
        Self::ProgramChange(ProgramChange {
            packet,
            program,
            bank,
        })
    }

    /// Construct Channel Pressure.
    pub fn channel_pressure(group: u8, channel: u8, data: u32) -> Self {
        let packet = packet(group, 0xD, channel, 0, 0, data);
        Self::ChannelPressure(ChannelValue { packet, data })
    }

    /// Construct Channel Pitch Bend.
    pub fn pitch_bend(group: u8, channel: u8, data: u32) -> Self {
        let packet = packet(group, 0xE, channel, 0, 0, data);
        Self::PitchBend(ChannelValue { packet, data })
    }

    /// Construct Per-Note Pitch Bend.
    pub fn per_note_pitch_bend(group: u8, channel: u8, note: u8, data: u32) -> Self {
        let packet = packet(group, 0x6, channel, note, 0, data);
        Self::PerNotePitchBend(PerNoteValue {
            packet,
            note: note & 0x7F,
            data,
        })
    }

    /// Decode an MT `0x4` packet. Other MTs are rejected.
    pub fn decode(packet: Ump) -> Option<Self> {
        if packet.mt() != 0x4 {
            return None;
        }
        let word0 = packet.word0();
        let word1 = packet.words()[1];
        let status = ((word0 >> 20) & 0xF) as u8;
        let index1 = ((word0 >> 8) & 0x7F) as u8;
        let index2 = (word0 & 0x7F) as u8;
        Some(match status {
            0x0 | 0x1 => {
                let message = PerNoteController {
                    packet,
                    note: index1,
                    controller: index2,
                    data: word1,
                };
                if status == 0 {
                    Self::RegisteredPerNoteController(message)
                } else {
                    Self::AssignablePerNoteController(message)
                }
            }
            0x2 | 0x3 => {
                let message = Controller {
                    packet,
                    bank: index1,
                    index: index2,
                    data: word1,
                };
                if status == 2 {
                    Self::RegisteredController(message)
                } else {
                    Self::AssignableController(message)
                }
            }
            0x4 | 0x5 => {
                let message = RelativeController {
                    packet,
                    bank: index1,
                    index: index2,
                    delta: word1 as i32,
                };
                if status == 4 {
                    Self::RelativeRegisteredController(message)
                } else {
                    Self::RelativeAssignableController(message)
                }
            }
            0x6 => Self::PerNotePitchBend(PerNoteValue {
                packet,
                note: index1,
                data: word1,
            }),
            0x8 | 0x9 => {
                let message = NoteMessage {
                    packet,
                    note: index1,
                    velocity: (word1 >> 16) as u16,
                    attribute_type: (word0 & 0xFF) as u8,
                    attribute_data: word1 as u16,
                };
                if status == 8 {
                    Self::NoteOff(message)
                } else {
                    Self::NoteOn(message)
                }
            }
            0xA => Self::PolyPressure(PerNoteValue {
                packet,
                note: index1,
                data: word1,
            }),
            0xB => Self::ControlChange(ControlChange {
                packet,
                controller: index1,
                data: word1,
            }),
            0xC => {
                let bank = if word0 & 1 != 0 {
                    Some(((((word1 >> 8) & 0x7F) as u16) << 7) | (word1 & 0x7F) as u16)
                } else {
                    None
                };
                Self::ProgramChange(ProgramChange {
                    packet,
                    program: ((word1 >> 24) & 0x7F) as u8,
                    bank,
                })
            }
            0xD => Self::ChannelPressure(ChannelValue {
                packet,
                data: word1,
            }),
            0xE => Self::PitchBend(ChannelValue {
                packet,
                data: word1,
            }),
            0xF => Self::PerNoteManagement(PerNoteManagement {
                packet,
                note: index1,
                flags: index2 & 0x3,
            }),
            _ => Self::Unknown(packet),
        })
    }

    /// Encode to the original or canonically constructed UMP.
    pub const fn encode(self) -> Ump {
        match self {
            Self::RegisteredPerNoteController(message)
            | Self::AssignablePerNoteController(message) => message.packet,
            Self::RegisteredController(message) | Self::AssignableController(message) => {
                message.packet
            }
            Self::RelativeRegisteredController(message)
            | Self::RelativeAssignableController(message) => message.packet,
            Self::PerNotePitchBend(message) | Self::PolyPressure(message) => message.packet,
            Self::NoteOff(message) | Self::NoteOn(message) => message.packet,
            Self::ControlChange(message) => message.packet,
            Self::ProgramChange(message) => message.packet,
            Self::ChannelPressure(message) | Self::PitchBend(message) => message.packet,
            Self::PerNoteManagement(message) => message.packet,
            Self::Unknown(packet) => packet,
        }
    }

    /// Semantic UMP group.
    pub const fn group(self) -> u8 {
        self.encode().routing_nibble()
    }

    /// MIDI channel, `0..=15`.
    pub const fn channel(self) -> u8 {
        ((self.encode().word0() >> 16) & 0xF) as u8
    }

    fn note_message(
        status: u8,
        group: u8,
        channel: u8,
        note: u8,
        velocity: u16,
        attribute_type: u8,
        attribute_data: u16,
    ) -> Self {
        let packet = packet(
            group,
            status,
            channel,
            note,
            attribute_type,
            (u32::from(velocity) << 16) | u32::from(attribute_data),
        );
        let message = NoteMessage {
            packet,
            note: note & 0x7F,
            velocity,
            attribute_type,
            attribute_data,
        };
        if status == 8 {
            Self::NoteOff(message)
        } else {
            Self::NoteOn(message)
        }
    }

    fn per_note_controller(
        status: u8,
        group: u8,
        channel: u8,
        note: u8,
        controller: u8,
        data: u32,
    ) -> Self {
        let packet = packet(group, status, channel, note, controller & 0x7F, data);
        let message = PerNoteController {
            packet,
            note: note & 0x7F,
            controller: controller & 0x7F,
            data,
        };
        if status == 0 {
            Self::RegisteredPerNoteController(message)
        } else {
            Self::AssignablePerNoteController(message)
        }
    }

    fn controller(status: u8, group: u8, channel: u8, bank: u8, index: u8, data: u32) -> Self {
        let packet = packet(group, status, channel, bank, index & 0x7F, data);
        let message = Controller {
            packet,
            bank: bank & 0x7F,
            index: index & 0x7F,
            data,
        };
        if status == 2 {
            Self::RegisteredController(message)
        } else {
            Self::AssignableController(message)
        }
    }

    fn relative_controller(
        status: u8,
        group: u8,
        channel: u8,
        bank: u8,
        index: u8,
        delta: i32,
    ) -> Self {
        let packet = packet(group, status, channel, bank, index & 0x7F, delta as u32);
        let message = RelativeController {
            packet,
            bank: bank & 0x7F,
            index: index & 0x7F,
            delta,
        };
        if status == 4 {
            Self::RelativeRegisteredController(message)
        } else {
            Self::RelativeAssignableController(message)
        }
    }
}

fn packet(group: u8, status: u8, channel: u8, index1: u8, index2: u8, data: u32) -> Ump {
    let word0 = 0x4000_0000
        | (u32::from(group & 0xF) << 24)
        | (u32::from(status & 0xF) << 20)
        | (u32::from(channel & 0xF) << 16)
        | (u32::from(index1 & 0x7F) << 8)
        | u32::from(index2);
    Ump::from_words(&[word0, data]).expect("MT 0x4 is two words")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_words(message: Midi2Cv, words: [u32; 2]) {
        assert_eq!(message.encode().words(), words);
        assert_eq!(
            Midi2Cv::decode(message.encode()).unwrap().encode(),
            message.encode()
        );
    }

    #[test]
    fn ni_known_word_vectors_every_variant() {
        // ni-midi2/tests/midi2_channel_voice_message_tests.cpp; each citation
        // below names the corresponding TEST_F(make_*_message).
        assert_words(
            Midi2Cv::note_off(13, 14, 0x14, 0xD123, 0x01, 0x9876),
            [0x4D8E_1401, 0xD123_9876],
        ); // make_midi2_note_off_message
        assert_words(
            Midi2Cv::note_on(7, 3, 0x77, 0xF001, 0x03, 0x6135),
            [0x4793_7703, 0xF001_6135],
        ); // make_midi2_note_on_message
        assert_words(
            Midi2Cv::poly_pressure(6, 9, 64, 100_000_000),
            [0x46A9_4000, 0x05F5_E100],
        ); // make_midi2_poly_pressure_message
        assert_words(
            Midi2Cv::registered_per_note_controller(0xF, 0xE, 44, 77, 0x3344_5566),
            [0x4F0E_2C4D, 0x3344_5566],
        ); // make_registered_per_note_controller_message
        assert_words(
            Midi2Cv::assignable_per_note_controller(0xE, 0xD, 0x12, 0x34, 0x4455_6677),
            [0x4E1D_1234, 0x4455_6677],
        ); // make_assignable_per_note_controller_message
        assert_words(
            Midi2Cv::per_note_management(15, 13, 96, 3),
            [0x4FFD_6003, 0],
        ); // make_per_note_management_message
        assert_words(
            Midi2Cv::control_change(9, 4, 0x38, 0x9876_5432),
            [0x49B4_3800, 0x9876_5432],
        ); // make_midi2_control_change_message
        assert_words(
            Midi2Cv::registered_controller(3, 7, 9, 0x45, 0x8010_1010),
            [0x4327_0945, 0x8010_1010],
        ); // make_registered_controller_message
        assert_words(
            Midi2Cv::assignable_controller(3, 7, 9, 0x45, 0x8010_1010),
            [0x4337_0945, 0x8010_1010],
        ); // make_assignable_controller_message
        assert_words(
            Midi2Cv::relative_registered_controller(3, 7, 9, 0x45, -522),
            [0x4347_0945, 0xFFFF_FDF6],
        ); // make_relative_registered_controller_message
        assert_words(
            Midi2Cv::relative_assignable_controller(3, 7, 9, 0x45, -522),
            [0x4357_0945, 0xFFFF_FDF6],
        ); // make_relative_assignable_controller_message
        assert_words(
            Midi2Cv::program_change(1, 8, 5, Some(0x1234)),
            [0x41C8_0001, 0x0500_2434],
        ); // make_midi2_program_change_message
        assert_words(
            Midi2Cv::channel_pressure(14, 3, 0x7927_3847),
            [0x4ED3_0000, 0x7927_3847],
        ); // make_midi2_channel_pressure_message
        assert_words(
            Midi2Cv::pitch_bend(12, 9, 0x8000_0000),
            [0x4CE9_0000, 0x8000_0000],
        ); // make_midi2_pitch_bend_message
        assert_words(
            Midi2Cv::per_note_pitch_bend(12, 9, 77, 0x8000_0000),
            [0x4C69_4D00, 0x8000_0000],
        ); // make_per_note_pitch_bend_message
    }

    #[test]
    fn field_domains_round_trip() {
        const DATA: [u32; 5] = [0, 1, 0x7FFF_FFFF, 0x8000_0000, u32::MAX];
        for group in 0..16 {
            for channel in 0..16 {
                for note in 0..=127 {
                    let data = DATA[note as usize % DATA.len()];
                    let messages = [
                        Midi2Cv::note_off(group, channel, note, note.into(), 0, 0),
                        Midi2Cv::note_on(
                            group,
                            channel,
                            note,
                            u16::MAX - u16::from(note),
                            3,
                            0x1234,
                        ),
                        Midi2Cv::poly_pressure(group, channel, note, data),
                        Midi2Cv::per_note_pitch_bend(group, channel, note, data),
                        Midi2Cv::per_note_management(group, channel, note, note),
                    ];
                    for message in messages {
                        assert_eq!(Midi2Cv::decode(message.encode()).unwrap(), message);
                    }
                }
            }
        }
        for index in 0..=127 {
            let messages = [
                Midi2Cv::registered_per_note_controller(1, 2, index, 127 - index, u32::from(index)),
                Midi2Cv::assignable_per_note_controller(1, 2, index, 127 - index, u32::from(index)),
                Midi2Cv::control_change(1, 2, index, u32::from(index)),
                Midi2Cv::registered_controller(1, 2, index, 127 - index, u32::from(index)),
                Midi2Cv::assignable_controller(1, 2, index, 127 - index, u32::from(index)),
            ];
            for message in messages {
                assert_eq!(Midi2Cv::decode(message.encode()).unwrap(), message);
            }
        }
    }

    #[test]
    fn reserved_and_unknown_bits_round_trip() {
        // Program Change: reserved bytes/bits are non-zero, but semantic
        // accessors still see program 5 and bank 0x1234.
        let raw = Ump::from_words(&[0x41C8_AA81, 0x85FF_A4B4]).unwrap();
        let decoded = Midi2Cv::decode(raw).unwrap();
        let Midi2Cv::ProgramChange(program) = decoded else {
            panic!("expected Program Change");
        };
        assert_eq!(program.program(), 5);
        assert_eq!(program.bank(), Some(0x1234));
        assert_eq!(decoded.encode(), raw);

        let unknown = Ump::from_words(&[0x4173_4567, 0x89AB_CDEF]).unwrap();
        assert_eq!(Midi2Cv::decode(unknown).unwrap(), Midi2Cv::Unknown(unknown));
    }
}
