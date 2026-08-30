//! Typed UMP message families above the raw [`Ump`](super::ump::Ump) layer.
//!
//! Decoding is total for complete packets: reserved MTs become
//! [`Message::Unknown`], and undefined statuses inside a known family become
//! that family's `Unknown(Ump)` variant. Only a truncated word-stream tail can
//! fail, through [`messages`].

use super::{
    cv1::Midi1Cv,
    cv2::Midi2Cv,
    ump::{NeedMoreWords, Packets, Ump, packets},
};

/// UMP Utility data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtilityValue {
    packet: Ump,
    value: u32,
}

impl UtilityValue {
    /// Utility payload. Delta Clockstamp uses 20 bits; the other defined
    /// Utility messages expose their low 16 bits.
    pub const fn value(self) -> u32 {
        self.value
    }
}

/// Utility messages (MT `0x0`, groupless since UMP 1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Utility {
    /// No Operation.
    NoOp(UtilityValue),
    /// Jitter Reduction Clock.
    JrClock(UtilityValue),
    /// Jitter Reduction Timestamp.
    JrTimestamp(UtilityValue),
    /// Delta Clockstamp Ticks Per Quarter Note (16-bit payload).
    DeltaClockstampTpq(UtilityValue),
    /// Delta Clockstamp (20-bit payload).
    DeltaClockstamp(UtilityValue),
    /// Undefined Utility status.
    Unknown(Ump),
}

impl Utility {
    /// Construct No Operation.
    pub fn no_op(data: u16) -> Self {
        Self::NoOp(utility_value(0, u32::from(data), false))
    }

    /// Construct Jitter Reduction Clock.
    pub fn jr_clock(data: u16) -> Self {
        Self::JrClock(utility_value(1, u32::from(data), false))
    }

    /// Construct Jitter Reduction Timestamp.
    pub fn jr_timestamp(data: u16) -> Self {
        Self::JrTimestamp(utility_value(2, u32::from(data), false))
    }

    /// Construct Delta Clockstamp Ticks Per Quarter Note.
    pub fn delta_clockstamp_tpq(ticks: u16) -> Self {
        Self::DeltaClockstampTpq(utility_value(3, u32::from(ticks), false))
    }

    /// Construct a 20-bit Delta Clockstamp.
    pub fn delta_clockstamp(ticks: u32) -> Self {
        Self::DeltaClockstamp(utility_value(4, ticks, true))
    }

    fn decode(packet: Ump) -> Self {
        let word = packet.word0();
        let status = ((word >> 20) & 0xF) as u8;
        let value = if status == 4 {
            word & 0x000F_FFFF
        } else {
            word & 0x0000_FFFF
        };
        let message = UtilityValue { packet, value };
        match status {
            0 => Self::NoOp(message),
            1 => Self::JrClock(message),
            2 => Self::JrTimestamp(message),
            3 => Self::DeltaClockstampTpq(message),
            4 => Self::DeltaClockstamp(message),
            _ => Self::Unknown(packet),
        }
    }

    /// Encode to the original or canonically constructed UMP.
    pub const fn encode(self) -> Ump {
        match self {
            Self::NoOp(message)
            | Self::JrClock(message)
            | Self::JrTimestamp(message)
            | Self::DeltaClockstampTpq(message)
            | Self::DeltaClockstamp(message) => message.packet,
            Self::Unknown(packet) => packet,
        }
    }
}

fn utility_value(status: u8, value: u32, twenty_bits: bool) -> UtilityValue {
    let mask = if twenty_bits {
        0x000F_FFFF
    } else {
        0x0000_FFFF
    };
    let value = value & mask;
    let word = (u32::from(status & 0xF) << 20) | value;
    UtilityValue {
        packet: Ump::from_words(&[word]).expect("MT 0x0 is one word"),
        value,
    }
}

/// A one-data-byte System Common message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemData {
    packet: Ump,
    value: u8,
}

impl SystemData {
    /// Seven-bit data value.
    pub const fn value(self) -> u8 {
        self.value
    }
}

/// Song Position Pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SongPosition {
    packet: Ump,
    value: u16,
}

impl SongPosition {
    /// Fourteen-bit song position.
    pub const fn value(self) -> u16 {
        self.value
    }
}

/// A no-data System Common or Real Time message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemSignal {
    packet: Ump,
}

/// System Common and System Real Time messages (MT `0x1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum System {
    /// MIDI Time Code Quarter Frame.
    MtcQuarterFrame(SystemData),
    /// Song Position Pointer.
    SongPosition(SongPosition),
    /// Song Select.
    SongSelect(SystemData),
    /// Tune Request.
    TuneRequest(SystemSignal),
    /// Timing Clock.
    TimingClock(SystemSignal),
    /// Start.
    Start(SystemSignal),
    /// Continue.
    Continue(SystemSignal),
    /// Stop.
    Stop(SystemSignal),
    /// Active Sensing.
    ActiveSensing(SystemSignal),
    /// System Reset.
    Reset(SystemSignal),
    /// Undefined System status.
    Unknown(Ump),
}

impl System {
    /// Construct MIDI Time Code Quarter Frame.
    pub fn mtc_quarter_frame(group: u8, data: u8) -> Self {
        Self::MtcQuarterFrame(system_data(group, 0xF1, data))
    }

    /// Construct Song Position Pointer.
    pub fn song_position(group: u8, value: u16) -> Self {
        let value = value & 0x3FFF;
        let packet = system_packet(
            group,
            0xF2,
            (value & 0x7F) as u8,
            ((value >> 7) & 0x7F) as u8,
        );
        Self::SongPosition(SongPosition { packet, value })
    }

    /// Construct Song Select.
    pub fn song_select(group: u8, song: u8) -> Self {
        Self::SongSelect(system_data(group, 0xF3, song))
    }

    /// Construct Tune Request.
    pub fn tune_request(group: u8) -> Self {
        Self::TuneRequest(system_signal(group, 0xF6))
    }

    /// Construct Timing Clock.
    pub fn timing_clock(group: u8) -> Self {
        Self::TimingClock(system_signal(group, 0xF8))
    }

    /// Construct Start.
    pub fn start(group: u8) -> Self {
        Self::Start(system_signal(group, 0xFA))
    }

    /// Construct Continue.
    pub fn continue_(group: u8) -> Self {
        Self::Continue(system_signal(group, 0xFB))
    }

    /// Construct Stop.
    pub fn stop(group: u8) -> Self {
        Self::Stop(system_signal(group, 0xFC))
    }

    /// Construct Active Sensing.
    pub fn active_sensing(group: u8) -> Self {
        Self::ActiveSensing(system_signal(group, 0xFE))
    }

    /// Construct System Reset.
    pub fn reset(group: u8) -> Self {
        Self::Reset(system_signal(group, 0xFF))
    }

    fn decode(packet: Ump) -> Self {
        let word = packet.word0();
        let status = ((word >> 16) & 0xFF) as u8;
        let data1 = ((word >> 8) & 0x7F) as u8;
        let data2 = (word & 0x7F) as u8;
        match status {
            0xF1 => Self::MtcQuarterFrame(SystemData {
                packet,
                value: data1,
            }),
            0xF2 => Self::SongPosition(SongPosition {
                packet,
                value: u16::from(data1) | (u16::from(data2) << 7),
            }),
            0xF3 => Self::SongSelect(SystemData {
                packet,
                value: data1,
            }),
            0xF6 => Self::TuneRequest(SystemSignal { packet }),
            0xF8 => Self::TimingClock(SystemSignal { packet }),
            0xFA => Self::Start(SystemSignal { packet }),
            0xFB => Self::Continue(SystemSignal { packet }),
            0xFC => Self::Stop(SystemSignal { packet }),
            0xFE => Self::ActiveSensing(SystemSignal { packet }),
            0xFF => Self::Reset(SystemSignal { packet }),
            _ => Self::Unknown(packet),
        }
    }

    /// Encode to the original or canonically constructed UMP.
    pub const fn encode(self) -> Ump {
        match self {
            Self::MtcQuarterFrame(message) | Self::SongSelect(message) => message.packet,
            Self::SongPosition(message) => message.packet,
            Self::TuneRequest(message)
            | Self::TimingClock(message)
            | Self::Start(message)
            | Self::Continue(message)
            | Self::Stop(message)
            | Self::ActiveSensing(message)
            | Self::Reset(message) => message.packet,
            Self::Unknown(packet) => packet,
        }
    }

    /// Semantic UMP group.
    pub const fn group(self) -> u8 {
        self.encode().routing_nibble()
    }
}

fn system_data(group: u8, status: u8, value: u8) -> SystemData {
    SystemData {
        packet: system_packet(group, status, value, 0),
        value: value & 0x7F,
    }
}

fn system_signal(group: u8, status: u8) -> SystemSignal {
    SystemSignal {
        packet: system_packet(group, status, 0, 0),
    }
}

fn system_packet(group: u8, status: u8, data1: u8, data2: u8) -> Ump {
    let word = 0x1000_0000
        | (u32::from(group & 0xF) << 24)
        | (u32::from(status) << 16)
        | (u32::from(data1 & 0x7F) << 8)
        | u32::from(data2 & 0x7F);
    Ump::from_words(&[word]).expect("MT 0x1 is one word")
}

/// SysEx7 packet payload and framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SysEx7Packet {
    packet: Ump,
    data: [u8; 6],
    len: u8,
}

impl SysEx7Packet {
    /// Payload bytes, each `0..=127`.
    pub fn data(&self) -> &[u8] {
        &self.data[..usize::from(self.len)]
    }
}

/// SysEx7 packet framing (MT `0x3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SysEx7 {
    /// Complete SysEx7 message in one packet.
    Complete(SysEx7Packet),
    /// Start packet.
    Start(SysEx7Packet),
    /// Continue packet.
    Continue(SysEx7Packet),
    /// End packet.
    End(SysEx7Packet),
    /// Undefined format or invalid byte count.
    Unknown(Ump),
}

impl SysEx7 {
    /// Construct a packet. Payloads longer than six bytes or containing a
    /// non-seven-bit byte are rejected.
    pub fn new(group: u8, format: SysEx7Format, data: &[u8]) -> Option<Self> {
        if data.len() > 6 || data.iter().any(|byte| byte & 0x80 != 0) {
            return None;
        }
        let mut payload = [0u8; 6];
        for (target, source) in payload.iter_mut().zip(data) {
            *target = *source;
        }
        let mut bytes = [0u8; 8];
        bytes[0] = 0x30 | (group & 0xF);
        bytes[1] = (format as u8) << 4 | data.len() as u8;
        bytes[2..].copy_from_slice(&payload);
        let packet = Ump::from_words(&[
            u32::from_be_bytes(bytes[..4].try_into().expect("four bytes")),
            u32::from_be_bytes(bytes[4..].try_into().expect("four bytes")),
        ])
        .expect("MT 0x3 is two words");
        let message = SysEx7Packet {
            packet,
            data: payload,
            len: data.len() as u8,
        };
        Some(match format {
            SysEx7Format::Complete => Self::Complete(message),
            SysEx7Format::Start => Self::Start(message),
            SysEx7Format::Continue => Self::Continue(message),
            SysEx7Format::End => Self::End(message),
        })
    }

    fn decode(packet: Ump) -> Self {
        let bytes = packet_bytes(packet);
        let format = bytes[1] >> 4;
        let len = bytes[1] & 0xF;
        if format > 3 || len > 6 {
            return Self::Unknown(packet);
        }
        if bytes[2..2 + usize::from(len)]
            .iter()
            .any(|byte| byte & 0x80 != 0)
        {
            return Self::Unknown(packet);
        }
        let mut data = [0u8; 6];
        for (target, source) in data.iter_mut().zip(&bytes[2..]) {
            *target = *source;
        }
        let message = SysEx7Packet { packet, data, len };
        match format {
            0 => Self::Complete(message),
            1 => Self::Start(message),
            2 => Self::Continue(message),
            3 => Self::End(message),
            _ => unreachable!("format checked above"),
        }
    }

    /// Encode to the original or canonically constructed UMP.
    pub const fn encode(self) -> Ump {
        match self {
            Self::Complete(message)
            | Self::Start(message)
            | Self::Continue(message)
            | Self::End(message) => message.packet,
            Self::Unknown(packet) => packet,
        }
    }

    /// Semantic UMP group.
    pub const fn group(self) -> u8 {
        self.encode().routing_nibble()
    }
}

/// SysEx7 packet format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SysEx7Format {
    /// Complete.
    Complete = 0,
    /// Start.
    Start = 1,
    /// Continue.
    Continue = 2,
    /// End.
    End = 3,
}

/// SysEx8 packet payload and framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SysEx8Packet {
    packet: Ump,
    stream_id: u8,
    data: [u8; 13],
    len: u8,
}

impl SysEx8Packet {
    /// SysEx8 stream identifier.
    pub const fn stream_id(self) -> u8 {
        self.stream_id
    }

    /// Payload bytes.
    pub fn data(&self) -> &[u8] {
        &self.data[..usize::from(self.len)]
    }
}

/// Mixed Data Set packet payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixedDataSetPacket {
    packet: Ump,
    id: u8,
    data: [u8; 14],
}

impl MixedDataSetPacket {
    /// Four-bit Mixed Data Set identifier.
    pub const fn id(self) -> u8 {
        self.id
    }

    /// Fourteen uninterpreted framing bytes.
    pub const fn data(self) -> [u8; 14] {
        self.data
    }
}

/// Data128 packet framing (MT `0x5`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Data128 {
    /// Complete SysEx8 packet.
    SysEx8Complete(SysEx8Packet),
    /// SysEx8 start packet.
    SysEx8Start(SysEx8Packet),
    /// SysEx8 continue packet.
    SysEx8Continue(SysEx8Packet),
    /// SysEx8 end packet.
    SysEx8End(SysEx8Packet),
    /// Mixed Data Set header.
    MixedDataSetHeader(MixedDataSetPacket),
    /// Mixed Data Set payload.
    MixedDataSetPayload(MixedDataSetPacket),
    /// Undefined Data128 framing.
    Unknown(Ump),
}

impl Data128 {
    /// Construct a SysEx8 packet. Payloads longer than thirteen bytes fail.
    pub fn sysex8(group: u8, format: SysEx7Format, stream_id: u8, data: &[u8]) -> Option<Self> {
        if data.len() > 13 {
            return None;
        }
        let mut bytes = [0u8; 16];
        bytes[0] = 0x50 | (group & 0xF);
        bytes[1] = (format as u8) << 4 | (data.len() as u8 + 1);
        bytes[2] = stream_id;
        bytes[3..3 + data.len()].copy_from_slice(data);
        let packet = ump_from_bytes(bytes);
        let mut payload = [0u8; 13];
        payload[..data.len()].copy_from_slice(data);
        let message = SysEx8Packet {
            packet,
            stream_id,
            data: payload,
            len: data.len() as u8,
        };
        Some(match format {
            SysEx7Format::Complete => Self::SysEx8Complete(message),
            SysEx7Format::Start => Self::SysEx8Start(message),
            SysEx7Format::Continue => Self::SysEx8Continue(message),
            SysEx7Format::End => Self::SysEx8End(message),
        })
    }

    /// Construct a Mixed Data Set header or payload packet.
    pub fn mixed_data_set(group: u8, header: bool, id: u8, data: [u8; 14]) -> Self {
        let mut bytes = [0u8; 16];
        bytes[0] = 0x50 | (group & 0xF);
        bytes[1] = (if header { 0x80 } else { 0x90 }) | (id & 0xF);
        bytes[2..].copy_from_slice(&data);
        let message = MixedDataSetPacket {
            packet: ump_from_bytes(bytes),
            id: id & 0xF,
            data,
        };
        if header {
            Self::MixedDataSetHeader(message)
        } else {
            Self::MixedDataSetPayload(message)
        }
    }

    fn decode(packet: Ump) -> Self {
        let bytes = packet_bytes(packet);
        let status = bytes[1] >> 4;
        match status {
            0..=3 if (1..=14).contains(&(bytes[1] & 0xF)) => {
                let len = (bytes[1] & 0xF) - 1;
                let mut data = [0u8; 13];
                data.copy_from_slice(&bytes[3..]);
                let message = SysEx8Packet {
                    packet,
                    stream_id: bytes[2],
                    data,
                    len,
                };
                match status {
                    0 => Self::SysEx8Complete(message),
                    1 => Self::SysEx8Start(message),
                    2 => Self::SysEx8Continue(message),
                    3 => Self::SysEx8End(message),
                    _ => unreachable!("status constrained above"),
                }
            }
            8 | 9 => {
                let mut data = [0u8; 14];
                data.copy_from_slice(&bytes[2..]);
                let message = MixedDataSetPacket {
                    packet,
                    id: bytes[1] & 0xF,
                    data,
                };
                if status == 8 {
                    Self::MixedDataSetHeader(message)
                } else {
                    Self::MixedDataSetPayload(message)
                }
            }
            _ => Self::Unknown(packet),
        }
    }

    /// Encode to the original or canonically constructed UMP.
    pub const fn encode(self) -> Ump {
        match self {
            Self::SysEx8Complete(message)
            | Self::SysEx8Start(message)
            | Self::SysEx8Continue(message)
            | Self::SysEx8End(message) => message.packet,
            Self::MixedDataSetHeader(message) | Self::MixedDataSetPayload(message) => {
                message.packet
            }
            Self::Unknown(packet) => packet,
        }
    }

    /// Semantic UMP group.
    pub const fn group(self) -> u8 {
        self.encode().routing_nibble()
    }
}

/// A decoded UMP message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
    /// Utility.
    Utility(Utility),
    /// System Common or Real Time.
    System(System),
    /// MIDI 1.0 Channel Voice.
    Midi1Cv(Midi1Cv),
    /// SysEx7 packet.
    SysEx7(SysEx7),
    /// MIDI 2.0 Channel Voice.
    Midi2Cv(Midi2Cv),
    /// Data128 (SysEx8 or Mixed Data Set framing).
    Data128(Data128),
    /// Reserved or currently unmodelled MT, retained exactly.
    Unknown(Ump),
}

impl Message {
    /// Decode any complete UMP. This operation is total.
    pub fn decode(packet: Ump) -> Self {
        match packet.mt() {
            0x0 => Self::Utility(Utility::decode(packet)),
            0x1 => Self::System(System::decode(packet)),
            0x2 => Self::Midi1Cv(Midi1Cv::decode(packet).expect("MT checked")),
            0x3 => Self::SysEx7(SysEx7::decode(packet)),
            0x4 => Self::Midi2Cv(Midi2Cv::decode(packet).expect("MT checked")),
            0x5 => Self::Data128(Data128::decode(packet)),
            _ => Self::Unknown(packet),
        }
    }

    /// Encode to the original or canonically constructed UMP.
    pub const fn encode(self) -> Ump {
        match self {
            Self::Utility(message) => message.encode(),
            Self::System(message) => message.encode(),
            Self::Midi1Cv(message) => message.encode(),
            Self::SysEx7(message) => message.encode(),
            Self::Midi2Cv(message) => message.encode(),
            Self::Data128(message) => message.encode(),
            Self::Unknown(packet) => packet,
        }
    }

    /// Semantic group for grouped families. Utility and unknown MTs return
    /// `None`; their raw routing nibble remains available from [`Self::encode`].
    pub const fn group(self) -> Option<u8> {
        match self {
            Self::Utility(_) | Self::Unknown(_) => None,
            Self::System(message) => Some(message.group()),
            Self::Midi1Cv(message) => Some(message.group()),
            Self::SysEx7(message) => Some(message.group()),
            Self::Midi2Cv(message) => Some(message.group()),
            Self::Data128(message) => Some(message.group()),
        }
    }
}

/// Location-independent SysEx7 topology failure.
///
/// Callers choose whether `location` means an absolute tick or a message
/// index. The state machine itself is shared by raw `.cosump` validation and
/// timed SMF conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SysEx7TopologyError {
    pub(crate) location: u64,
    pub(crate) group: u8,
    pub(crate) detail: &'static str,
}

/// Canonical SysEx7 topology validator for the 16 independent UMP groups.
#[derive(Debug, Default)]
pub(crate) struct SysEx7Topology {
    open: [Option<u64>; 16],
}

impl SysEx7Topology {
    pub(crate) fn push(
        &mut self,
        message: Message,
        location: u64,
    ) -> Result<(), SysEx7TopologyError> {
        if let Message::SysEx7(sysex) = message {
            let group = sysex.group();
            let open = &mut self.open[usize::from(group)];
            match sysex {
                SysEx7::Complete(_) if open.is_some() => {
                    return Err(SysEx7TopologyError {
                        location,
                        group,
                        detail: "nested SysEx7 Complete while a SysEx7 run is open",
                    });
                }
                SysEx7::Start(_) if open.is_some() => {
                    return Err(SysEx7TopologyError {
                        location,
                        group,
                        detail: "nested SysEx7 Start while a SysEx7 run is open",
                    });
                }
                SysEx7::Start(_) => *open = Some(location),
                SysEx7::Continue(_) | SysEx7::End(_) if open.is_none() => {
                    return Err(SysEx7TopologyError {
                        location,
                        group,
                        detail: "standalone SysEx7 Continue/End without an open Start",
                    });
                }
                SysEx7::End(_) => *open = None,
                SysEx7::Complete(_) | SysEx7::Continue(_) => {}
                SysEx7::Unknown(_) => {
                    if open.is_some() {
                        return Err(SysEx7TopologyError {
                            location,
                            group,
                            detail: "unknown SysEx7 packet interrupts an open SysEx7 run",
                        });
                    }
                }
            }
            return Ok(());
        }

        // NI data_message_tests.cpp carries group on every SysEx7 fragment.
        // AM docs/umpProcessor.md declares Utility MT 0x0 groupless, so it
        // cannot target an open group. NI midi1_byte_stream_tests.cpp permits
        // System Real-Time inside SysEx; every other same-group message is an
        // interruption, whether or not down-translation would later drop it.
        if let Some(group) = message.group()
            && self.open[usize::from(group)].is_some()
            && !is_realtime(message)
        {
            return Err(SysEx7TopologyError {
                location,
                group,
                detail: "same-group non-Real-Time message interrupts an open SysEx7 run",
            });
        }
        Ok(())
    }

    pub(crate) fn finish(&self, location: u64) -> Result<(), SysEx7TopologyError> {
        if let Some((group, _)) = self
            .open
            .iter()
            .enumerate()
            .find(|(_, start)| start.is_some())
        {
            return Err(SysEx7TopologyError {
                location,
                group: group as u8,
                detail: "SysEx7 Start is unterminated at end of UMP stream",
            });
        }
        Ok(())
    }
}

fn is_realtime(message: Message) -> bool {
    matches!(
        message,
        Message::System(
            System::TimingClock(_)
                | System::Start(_)
                | System::Continue(_)
                | System::Stop(_)
                | System::ActiveSensing(_)
                | System::Reset(_)
        )
    )
}

/// Iterate and decode a UMP word stream.
pub fn messages(words: &[u32]) -> Messages<'_> {
    Messages {
        packets: packets(words),
    }
}

/// Iterator returned by [`messages`].
#[derive(Debug, Clone)]
pub struct Messages<'a> {
    packets: Packets<'a>,
}

impl Iterator for Messages<'_> {
    type Item = Result<Message, NeedMoreWords>;

    fn next(&mut self) -> Option<Self::Item> {
        self.packets
            .next()
            .map(|result| result.map(Message::decode))
    }
}

fn packet_bytes(packet: Ump) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    for (chunk, word) in bytes.chunks_exact_mut(4).zip(packet.words()) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    bytes
}

fn ump_from_bytes(bytes: [u8; 16]) -> Ump {
    let words = [
        u32::from_be_bytes(bytes[0..4].try_into().expect("four bytes")),
        u32::from_be_bytes(bytes[4..8].try_into().expect("four bytes")),
        u32::from_be_bytes(bytes[8..12].try_into().expect("four bytes")),
        u32::from_be_bytes(bytes[12..16].try_into().expect("four bytes")),
    ];
    Ump::from_words(&words).expect("byte array starts with MT 0x5")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utility_vectors_and_groupless_contract() {
        // rust-midi2/midi2/src/utility.rs:
        // delta_clock_stamp_u20 / delta_clock_stamp_tpq_try_from.
        let dcs = Utility::delta_clockstamp(0xABCDE);
        assert_eq!(dcs.encode().words(), &[0x004A_BCDE]);
        let tpq = Utility::delta_clockstamp_tpq(480);
        assert_eq!(tpq.encode().words(), &[0x0030_01E0]);
        assert_eq!(Message::Utility(dcs).group(), None);
        for message in [
            Utility::no_op(0x1234),
            Utility::jr_clock(0x5678),
            Utility::jr_timestamp(0x9ABC),
            tpq,
            dcs,
        ] {
            let decoded = Message::decode(message.encode());
            assert_eq!(decoded.encode(), message.encode());
        }
    }

    #[test]
    fn system_vectors() {
        // ni-midi2/tests/system_message_tests.cpp:
        // make_song_position_message / make_system_message.
        let position = System::song_position(9, 0x34F4);
        assert_eq!(position.encode().words(), &[0x19F2_7469]);
        let quarter = System::mtc_quarter_frame(4, 0x43);
        assert_eq!(quarter.encode().words(), &[0x14F1_4300]);
        let clock = System::timing_clock(12);
        assert_eq!(clock.encode().words(), &[0x1CF8_0000]);
        for message in [
            position,
            quarter,
            clock,
            System::song_select(3, 42),
            System::tune_request(3),
            System::start(3),
            System::continue_(3),
            System::stop(3),
            System::active_sensing(3),
            System::reset(3),
        ] {
            assert_eq!(Message::decode(message.encode()).encode(), message.encode());
        }
    }

    #[test]
    fn sysex7_framing_and_reserved_status() {
        // ni-midi2/tests/data_message_tests.cpp:
        // sysex7_payload_byte / sysex7_packet_format.
        let data = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        for format in [
            SysEx7Format::Complete,
            SysEx7Format::Start,
            SysEx7Format::Continue,
            SysEx7Format::End,
        ] {
            let message = SysEx7::new(9, format, &data).unwrap();
            assert_eq!(Message::decode(message.encode()).encode(), message.encode());
        }
        assert_eq!(
            SysEx7::new(9, SysEx7Format::Complete, &data)
                .unwrap()
                .encode()
                .words(),
            &[0x3906_1122, 0x3344_5566]
        );
        let unknown = Ump::from_words(&[0x3947_1122, 0x3344_5566]).unwrap();
        assert_eq!(
            Message::decode(unknown),
            Message::SysEx7(SysEx7::Unknown(unknown))
        );
    }

    #[test]
    fn sysex7_high_bit_payload_decodes_unknown_and_reencodes_byte_exact() {
        let raw = Ump::from_words(&[0x3901_8000, 0x0000_0000]).unwrap();
        let decoded = Message::decode(raw);
        assert_eq!(decoded, Message::SysEx7(SysEx7::Unknown(raw)));
        assert_eq!(decoded.encode(), raw);
        assert!(SysEx7::new(9, SysEx7Format::Complete, &[0x80]).is_none());
    }

    #[test]
    fn data128_framing() {
        // ni-midi2/tests/extended_data_message_tests.cpp:
        // sysex8_packet_payload_byte / mixed_data_set_*_packet.
        let payload = [
            0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
        ];
        let sysex = Data128::sysex8(12, SysEx7Format::End, 0xAC, &payload).unwrap();
        assert_eq!(Message::decode(sysex.encode()).encode(), sysex.encode());
        assert_eq!(sysex.encode().word0(), 0x5C3E_AC11);

        let mds_data = [0x5A; 14];
        for header in [true, false] {
            let message = Data128::mixed_data_set(4, header, 7, mds_data);
            assert_eq!(Message::decode(message.encode()).encode(), message.encode());
        }
    }

    #[test]
    fn total_decode_unknown_mts_and_statuses() {
        for mt in 0u8..16 {
            let len = super::super::ump::mt_words(mt);
            let mut words = [0u32; 4];
            words[0] = u32::from(mt) << 28 | 0x00AB_CDEF;
            let packet = Ump::from_words(&words[..len]).unwrap();
            assert_eq!(Message::decode(packet).encode(), packet, "MT {mt:#x}");
        }
    }

    #[test]
    fn stream_decode_only_fails_on_truncated_tail() {
        let words = [
            0x2090_3C40,
            0x004A_BCDE,
            0x4090_3C00,
            0x1234_5678,
            0x5001_0000,
        ];
        let decoded: Vec<_> = messages(&words).collect();
        assert_eq!(decoded.len(), 4);
        assert!(decoded[..3].iter().all(Result::is_ok));
        assert_eq!(
            decoded[3],
            Err(NeedMoreWords {
                word0: 0x5001_0000,
                expected: 4,
                actual: 1,
            })
        );
    }

    #[test]
    fn semantic_group_is_optional() {
        let grouped = [
            Message::System(System::start(6)),
            Message::Midi1Cv(Midi1Cv::note_on(6, 1, 60, 100)),
            Message::SysEx7(SysEx7::new(6, SysEx7Format::Complete, &[]).unwrap()),
            Message::Midi2Cv(Midi2Cv::note_on(6, 1, 60, 0x8000, 0, 0)),
            Message::Data128(Data128::sysex8(6, SysEx7Format::Complete, 0, &[]).unwrap()),
        ];
        for message in grouped {
            assert_eq!(message.group(), Some(6));
        }
        let mut raw = Ump::from_words(&[0x0000_0000]).unwrap();
        raw.set_routing_nibble(0xA);
        let utility = Message::decode(raw);
        assert_eq!(utility.group(), None);
        assert_eq!(utility.encode().routing_nibble(), 0xA);
    }
}
