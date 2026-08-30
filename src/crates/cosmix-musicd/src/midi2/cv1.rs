//! MIDI 1.0 Channel Voice messages carried in UMP Message Type `0x2`.
//!
//! The semantic fields are seven-bit MIDI 1.0 values. Each decoded variant
//! retains its original [`Ump`] privately, so reserved and invalid high bits
//! survive decode/re-encode byte-for-byte while the public accessors remain
//! width-correct. The future MIDI 1.0 byte-stream codec shares these types.

use super::ump::Ump;

/// Note-shaped MIDI 1.0 messages: Note Off, Note On, and Poly Pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoteMessage {
    packet: Ump,
    note: u8,
    value: u8,
}

impl NoteMessage {
    /// Note number, `0..=127`.
    pub const fn note(self) -> u8 {
        self.note
    }

    /// Velocity or pressure, `0..=127`.
    pub const fn value(self) -> u8 {
        self.value
    }
}

/// A MIDI 1.0 Control Change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlChange {
    packet: Ump,
    controller: u8,
    value: u8,
}

impl ControlChange {
    /// Controller index, `0..=127`.
    pub const fn controller(self) -> u8 {
        self.controller
    }

    /// Controller value, `0..=127`.
    pub const fn value(self) -> u8 {
        self.value
    }
}

/// A MIDI 1.0 Program Change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgramChange {
    packet: Ump,
    program: u8,
}

impl ProgramChange {
    /// Program number, `0..=127`.
    pub const fn program(self) -> u8 {
        self.program
    }
}

/// MIDI 1.0 Channel Pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelPressure {
    packet: Ump,
    pressure: u8,
}

impl ChannelPressure {
    /// Pressure value, `0..=127`.
    pub const fn pressure(self) -> u8 {
        self.pressure
    }
}

/// MIDI 1.0 fourteen-bit Pitch Bend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PitchBend {
    packet: Ump,
    value: u16,
}

impl PitchBend {
    /// Pitch-bend value, `0..=16383`, centred at 8192.
    pub const fn value(self) -> u16 {
        self.value
    }
}

/// The complete MIDI 1.0 Channel Voice set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Midi1Cv {
    /// Note Off.
    NoteOff(NoteMessage),
    /// Note On. A zero velocity remains a Note On at this packet layer.
    NoteOn(NoteMessage),
    /// Polyphonic Key Pressure.
    PolyPressure(NoteMessage),
    /// Control Change.
    ControlChange(ControlChange),
    /// Program Change.
    ProgramChange(ProgramChange),
    /// Channel Pressure.
    ChannelPressure(ChannelPressure),
    /// Pitch Bend.
    PitchBend(PitchBend),
    /// An undefined status nibble, retained without interpretation.
    Unknown(Ump),
}

impl Midi1Cv {
    /// Construct Note Off.
    pub fn note_off(group: u8, channel: u8, note: u8, velocity: u8) -> Self {
        Self::note_message(0x8, group, channel, note, velocity)
    }

    /// Construct Note On.
    pub fn note_on(group: u8, channel: u8, note: u8, velocity: u8) -> Self {
        Self::note_message(0x9, group, channel, note, velocity)
    }

    /// Construct Polyphonic Key Pressure.
    pub fn poly_pressure(group: u8, channel: u8, note: u8, pressure: u8) -> Self {
        Self::note_message(0xA, group, channel, note, pressure)
    }

    /// Construct Control Change.
    pub fn control_change(group: u8, channel: u8, controller: u8, value: u8) -> Self {
        let packet = packet(group, 0xB, channel, controller, value);
        Self::ControlChange(ControlChange {
            packet,
            controller: controller & 0x7F,
            value: value & 0x7F,
        })
    }

    /// Construct Program Change.
    pub fn program_change(group: u8, channel: u8, program: u8) -> Self {
        let packet = packet(group, 0xC, channel, program, 0);
        Self::ProgramChange(ProgramChange {
            packet,
            program: program & 0x7F,
        })
    }

    /// Construct Channel Pressure.
    pub fn channel_pressure(group: u8, channel: u8, pressure: u8) -> Self {
        let packet = packet(group, 0xD, channel, pressure, 0);
        Self::ChannelPressure(ChannelPressure {
            packet,
            pressure: pressure & 0x7F,
        })
    }

    /// Construct fourteen-bit Pitch Bend.
    pub fn pitch_bend(group: u8, channel: u8, value: u16) -> Self {
        let value = value & 0x3FFF;
        let packet = packet(
            group,
            0xE,
            channel,
            (value & 0x7F) as u8,
            ((value >> 7) & 0x7F) as u8,
        );
        Self::PitchBend(PitchBend { packet, value })
    }

    /// Decode an MT `0x2` packet. Other MTs are rejected.
    pub fn decode(packet: Ump) -> Option<Self> {
        if packet.mt() != 0x2 {
            return None;
        }
        let word = packet.word0();
        let status = ((word >> 20) & 0xF) as u8;
        let data1 = ((word >> 8) & 0x7F) as u8;
        let data2 = (word & 0x7F) as u8;
        Some(match status {
            0x8 => Self::NoteOff(NoteMessage {
                packet,
                note: data1,
                value: data2,
            }),
            0x9 => Self::NoteOn(NoteMessage {
                packet,
                note: data1,
                value: data2,
            }),
            0xA => Self::PolyPressure(NoteMessage {
                packet,
                note: data1,
                value: data2,
            }),
            0xB => Self::ControlChange(ControlChange {
                packet,
                controller: data1,
                value: data2,
            }),
            0xC => Self::ProgramChange(ProgramChange {
                packet,
                program: data1,
            }),
            0xD => Self::ChannelPressure(ChannelPressure {
                packet,
                pressure: data1,
            }),
            0xE => Self::PitchBend(PitchBend {
                packet,
                value: u16::from(data1) | (u16::from(data2) << 7),
            }),
            _ => Self::Unknown(packet),
        })
    }

    /// Encode to the original or canonically constructed UMP.
    pub const fn encode(self) -> Ump {
        match self {
            Self::NoteOff(message) | Self::NoteOn(message) | Self::PolyPressure(message) => {
                message.packet
            }
            Self::ControlChange(message) => message.packet,
            Self::ProgramChange(message) => message.packet,
            Self::ChannelPressure(message) => message.packet,
            Self::PitchBend(message) => message.packet,
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

    fn note_message(status: u8, group: u8, channel: u8, note: u8, value: u8) -> Self {
        let packet = packet(group, status, channel, note, value);
        let message = NoteMessage {
            packet,
            note: note & 0x7F,
            value: value & 0x7F,
        };
        match status {
            0x8 => Self::NoteOff(message),
            0x9 => Self::NoteOn(message),
            0xA => Self::PolyPressure(message),
            _ => unreachable!("note_message only accepts note-shaped statuses"),
        }
    }
}

fn packet(group: u8, status: u8, channel: u8, data1: u8, data2: u8) -> Ump {
    let word = 0x2000_0000
        | (u32::from(group & 0xF) << 24)
        | (u32::from(status & 0xF) << 20)
        | (u32::from(channel & 0xF) << 16)
        | (u32::from(data1 & 0x7F) << 8)
        | u32::from(data2 & 0x7F);
    Ump::from_words(&[word]).expect("MT 0x2 is one word")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // ni-midi2/tests/midi1_channel_voice_message_tests.cpp:
        // make_midi1_note_off_message, make_note_on_message,
        // make_midi1_poly_pressure_message, make_midi1_control_change_message,
        // make_midi1_program_change_message, make_midi1_channel_pressure_message,
        // and make_midi1_pitch_bend_message.
        let vectors = [
            (Midi1Cv::note_off(4, 9, 0x50, 0x23), 0x2489_5023),
            (Midi1Cv::note_on(8, 4, 0x12, 0x02), 0x2894_1202),
            (Midi1Cv::poly_pressure(6, 9, 64, 100), 0x26A9_4064),
            (Midi1Cv::control_change(9, 4, 0x38, 0x32), 0x29B4_3832),
            (Midi1Cv::program_change(14, 3, 99), 0x2EC3_6300),
            (Midi1Cv::channel_pressure(14, 3, 71), 0x2ED3_4700),
            (Midi1Cv::pitch_bend(12, 9, 0x2000), 0x2CE9_0040),
        ];
        for (message, word) in vectors {
            assert_eq!(message.encode().words(), &[word]);
            assert_eq!(
                Midi1Cv::decode(message.encode()).unwrap().encode(),
                message.encode()
            );
        }
    }

    #[test]
    fn field_domains_round_trip() {
        for group in 0..16 {
            for channel in 0..16 {
                for value in 0..=127 {
                    let messages = [
                        Midi1Cv::note_off(group, channel, value, 127 - value),
                        Midi1Cv::note_on(group, channel, value, value),
                        Midi1Cv::poly_pressure(group, channel, value, value),
                        Midi1Cv::control_change(group, channel, value, value),
                        Midi1Cv::program_change(group, channel, value),
                        Midi1Cv::channel_pressure(group, channel, value),
                    ];
                    for message in messages {
                        assert_eq!(
                            Midi1Cv::decode(message.encode()).unwrap().encode(),
                            message.encode()
                        );
                    }
                }
            }
        }
        for value in 0..=0x3FFF {
            let message = Midi1Cv::pitch_bend(3, 7, value);
            let decoded = Midi1Cv::decode(message.encode()).unwrap();
            assert_eq!(decoded, message);
        }
    }

    #[test]
    fn reserved_bits_round_trip() {
        // Program Change's unused final byte and the high data bit are
        // semantically ignored but retained in the private raw packet.
        let raw = Ump::from_words(&[0x2AC3_FF55]).unwrap();
        let decoded = Midi1Cv::decode(raw).unwrap();
        assert!(matches!(decoded, Midi1Cv::ProgramChange(_)));
        assert_eq!(decoded.encode(), raw);

        let unknown = Ump::from_words(&[0x2A73_4567]).unwrap();
        assert_eq!(Midi1Cv::decode(unknown).unwrap(), Midi1Cv::Unknown(unknown));
    }
}
