//! Cos-internal Universal MIDI Packet container (`.cosump`).
//!
//! **This is not an interchange format and does not claim conformance to the
//! MIDI Association's MIDI Clip File format.** `.cosump` is a small,
//! versioned working container for Cosmix. Standard MIDI File (`.mid`) is the
//! external interchange format; use the [`super::smfio`] bridge at that
//! boundary.
//!
//! Version 1 is entirely little-endian:
//!
//! ```text
//! 8 bytes   magic "cosmixU1"
//! u32       header_len = 28 + tempo_count * 12
//! u32       flags = 0
//! u32       tempo_count
//! u64       word_count
//! repeated  { u64 absolute_tick, u32 microseconds_per_quarter }
//! repeated  u32 UMP words
//! ```
//!
//! Timing deltas remain spec-native Utility Delta Clockstamp messages in the
//! UMP word stream. The header stores only the SMF tempo map that Phase 1
//! deliberately does not model as Flex Data. Unknown non-zero flags are
//! rejected: accepting and then rewriting semantics this implementation does
//! not understand would violate the version boundary. Both [`read`] and
//! [`write`] enforce complete UMP framing and the shared per-group SysEx7
//! topology state machine, so an on-disk `.cosump` is valid by construction.

use std::{error::Error, fmt};

use super::{
    msg::{SysEx7Topology, SysEx7TopologyError, messages},
    ump::NeedMoreWords,
};

/// On-disk magic and version marker.
pub const MAGIC: &[u8; 8] = b"cosmixU1";

const FIXED_HEADER_LEN: usize = 28;
const TEMPO_RECORD_LEN: usize = 12;

/// One absolute SMF tempo change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tempo {
    /// Absolute tick in the file's ticks-per-quarter timebase.
    pub absolute_tick: u64,
    /// Microseconds per quarter note (`1..=0x00FF_FFFF`).
    pub us_per_quarter: u32,
}

/// Decoded `.cosump` content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UmpFile {
    /// Tempo changes ordered by non-decreasing absolute tick.
    pub tempos: Vec<Tempo>,
    /// Complete UMP packet stream as host-endian words.
    pub words: Vec<u32>,
}

/// Container validation or encoding failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UmpFileError {
    /// Input does not contain the complete fixed header.
    ShortHeader {
        /// Bytes required.
        expected: usize,
        /// Bytes available.
        actual: usize,
    },
    /// The eight-byte magic/version marker is not `cosmixU1`.
    BadMagic,
    /// Header length disagrees with the tempo-record count.
    BadHeaderLength {
        /// Length stored in the file.
        declared: u32,
        /// Exact length implied by the version-1 fields.
        expected: u64,
    },
    /// Version 1 defines no flags.
    UnsupportedFlags(u32),
    /// A tempo value cannot be represented by an SMF tempo meta event.
    InvalidTempo {
        /// Tempo-record index.
        index: usize,
        /// Invalid microseconds-per-quarter value.
        us_per_quarter: u32,
    },
    /// Tempo records are not ordered by absolute tick.
    UnsortedTempo {
        /// Tempo-record index at which ordering went backwards.
        index: usize,
        /// Previous absolute tick.
        previous: u64,
        /// Current absolute tick.
        current: u64,
    },
    /// Payload size does not match `word_count`.
    WordCountMismatch {
        /// Word count declared in the header.
        declared: u64,
        /// Complete words physically present after the header.
        actual: u64,
        /// Extra non-word bytes after complete words.
        trailing_bytes: usize,
    },
    /// UMP payload ends part-way through a packet.
    TruncatedUmp(NeedMoreWords),
    /// The UMP stream contains invalid per-group SysEx7 framing.
    InvalidSysEx7Topology {
        /// Zero-based semantic message index.
        message_index: u64,
        /// UMP group whose topology failed.
        group: u8,
        /// Human-readable topology violation.
        detail: &'static str,
    },
    /// Counts cannot fit the version-1 integer fields or address space.
    SizeOverflow,
}

impl fmt::Display for UmpFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShortHeader { expected, actual } => {
                write!(
                    f,
                    "short .cosump header: need {expected} bytes, have {actual}"
                )
            }
            Self::BadMagic => write!(f, "bad .cosump magic (expected \"cosmixU1\")"),
            Self::BadHeaderLength { declared, expected } => write!(
                f,
                "bad .cosump header length {declared} (tempo count requires {expected})"
            ),
            Self::UnsupportedFlags(flags) => {
                write!(f, "unsupported .cosump flags 0x{flags:08X}")
            }
            Self::InvalidTempo {
                index,
                us_per_quarter,
            } => write!(
                f,
                "invalid .cosump tempo #{index}: {us_per_quarter} us/quarter"
            ),
            Self::UnsortedTempo {
                index,
                previous,
                current,
            } => write!(
                f,
                "unsorted .cosump tempo #{index}: tick {current} follows {previous}"
            ),
            Self::WordCountMismatch {
                declared,
                actual,
                trailing_bytes,
            } => write!(
                f,
                ".cosump word count mismatch: declared {declared}, payload has {actual} complete words and {trailing_bytes} trailing bytes"
            ),
            Self::TruncatedUmp(error) => write!(
                f,
                "truncated UMP packet at word 0x{:08X}: expected {} words, have {}",
                error.word0, error.expected, error.actual
            ),
            Self::InvalidSysEx7Topology {
                message_index,
                group,
                detail,
            } => write!(
                f,
                "invalid .cosump SysEx7 topology in group {group} at message {message_index}: {detail}"
            ),
            Self::SizeOverflow => write!(f, ".cosump size exceeds version-1 limits"),
        }
    }
}

impl Error for UmpFileError {}

/// Decode and validate one complete `.cosump` byte slice.
pub fn read(bytes: &[u8]) -> Result<UmpFile, UmpFileError> {
    if bytes.len() < FIXED_HEADER_LEN {
        return Err(UmpFileError::ShortHeader {
            expected: FIXED_HEADER_LEN,
            actual: bytes.len(),
        });
    }
    if &bytes[..MAGIC.len()] != MAGIC {
        return Err(UmpFileError::BadMagic);
    }

    let header_len = read_u32(bytes, 8);
    let flags = read_u32(bytes, 12);
    if flags != 0 {
        return Err(UmpFileError::UnsupportedFlags(flags));
    }
    let tempo_count = read_u32(bytes, 16);
    let word_count = read_u64(bytes, 20);
    let expected_header = (FIXED_HEADER_LEN as u64)
        .checked_add(u64::from(tempo_count) * TEMPO_RECORD_LEN as u64)
        .ok_or(UmpFileError::SizeOverflow)?;
    if u64::from(header_len) != expected_header {
        return Err(UmpFileError::BadHeaderLength {
            declared: header_len,
            expected: expected_header,
        });
    }
    let header_len = usize::try_from(header_len).map_err(|_| UmpFileError::SizeOverflow)?;
    if bytes.len() < header_len {
        return Err(UmpFileError::ShortHeader {
            expected: header_len,
            actual: bytes.len(),
        });
    }

    let mut tempos = Vec::with_capacity(tempo_count as usize);
    let mut offset = FIXED_HEADER_LEN;
    for index in 0..tempo_count as usize {
        let tempo = Tempo {
            absolute_tick: read_u64(bytes, offset),
            us_per_quarter: read_u32(bytes, offset + 8),
        };
        validate_tempo(&tempos, index, tempo)?;
        tempos.push(tempo);
        offset += TEMPO_RECORD_LEN;
    }

    let payload = &bytes[header_len..];
    let actual_words = payload.len() / 4;
    let trailing_bytes = payload.len() % 4;
    if u64::try_from(actual_words).map_err(|_| UmpFileError::SizeOverflow)? != word_count
        || trailing_bytes != 0
    {
        return Err(UmpFileError::WordCountMismatch {
            declared: word_count,
            actual: actual_words as u64,
            trailing_bytes,
        });
    }
    let mut words = Vec::with_capacity(actual_words);
    for chunk in payload.chunks_exact(4) {
        words.push(u32::from_le_bytes(
            chunk.try_into().expect("chunks_exact yields four bytes"),
        ));
    }
    validate_words(&words)?;
    Ok(UmpFile { tempos, words })
}

/// Encode one validated `.cosump` value.
pub fn write(file: &UmpFile) -> Result<Vec<u8>, UmpFileError> {
    for (index, &tempo) in file.tempos.iter().enumerate() {
        validate_tempo(&file.tempos[..index], index, tempo)?;
    }
    validate_words(&file.words)?;

    let tempo_count = u32::try_from(file.tempos.len()).map_err(|_| UmpFileError::SizeOverflow)?;
    let word_count = u64::try_from(file.words.len()).map_err(|_| UmpFileError::SizeOverflow)?;
    let header_len = FIXED_HEADER_LEN
        .checked_add(
            file.tempos
                .len()
                .checked_mul(TEMPO_RECORD_LEN)
                .ok_or(UmpFileError::SizeOverflow)?,
        )
        .ok_or(UmpFileError::SizeOverflow)?;
    let header_len_u32 = u32::try_from(header_len).map_err(|_| UmpFileError::SizeOverflow)?;
    let payload_len = file
        .words
        .len()
        .checked_mul(4)
        .ok_or(UmpFileError::SizeOverflow)?;
    let mut bytes = Vec::with_capacity(
        header_len
            .checked_add(payload_len)
            .ok_or(UmpFileError::SizeOverflow)?,
    );
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&header_len_u32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&tempo_count.to_le_bytes());
    bytes.extend_from_slice(&word_count.to_le_bytes());
    for tempo in &file.tempos {
        bytes.extend_from_slice(&tempo.absolute_tick.to_le_bytes());
        bytes.extend_from_slice(&tempo.us_per_quarter.to_le_bytes());
    }
    for word in &file.words {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    Ok(bytes)
}

fn validate_words(words: &[u32]) -> Result<(), UmpFileError> {
    let mut topology = SysEx7Topology::default();
    let mut message_count = 0u64;
    for (index, result) in messages(words).enumerate() {
        let message = result.map_err(UmpFileError::TruncatedUmp)?;
        let message_index = u64::try_from(index).map_err(|_| UmpFileError::SizeOverflow)?;
        topology
            .push(message, message_index)
            .map_err(map_topology_error)?;
        message_count = message_index
            .checked_add(1)
            .ok_or(UmpFileError::SizeOverflow)?;
    }
    topology.finish(message_count).map_err(map_topology_error)
}

fn map_topology_error(error: SysEx7TopologyError) -> UmpFileError {
    UmpFileError::InvalidSysEx7Topology {
        message_index: error.location,
        group: error.group,
        detail: error.detail,
    }
}

fn validate_tempo(previous: &[Tempo], index: usize, tempo: Tempo) -> Result<(), UmpFileError> {
    if !(1..=0x00FF_FFFF).contains(&tempo.us_per_quarter) {
        return Err(UmpFileError::InvalidTempo {
            index,
            us_per_quarter: tempo.us_per_quarter,
        });
    }
    if let Some(prior) = previous.last()
        && tempo.absolute_tick < prior.absolute_tick
    {
        return Err(UmpFileError::UnsortedTempo {
            index,
            previous: prior.absolute_tick,
            current: tempo.absolute_tick,
        });
    }
    Ok(())
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("fixed header bounds checked"),
    )
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("fixed header bounds checked"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::midi2::{
        cv2::Midi2Cv,
        msg::{Message, SysEx7, SysEx7Format, Utility},
    };

    #[test]
    fn empty_stream_round_trip_and_exact_header() {
        let file = UmpFile {
            tempos: Vec::new(),
            words: Vec::new(),
        };
        let bytes = write(&file).unwrap();
        assert_eq!(bytes.len(), 28);
        assert_eq!(&bytes[..8], MAGIC);
        assert_eq!(&bytes[8..12], &28u32.to_le_bytes());
        assert_eq!(read(&bytes).unwrap(), file);
    }

    #[test]
    fn tempo_map_and_ump_words_round_trip() {
        let message = Message::Midi2Cv(Midi2Cv::note_on(2, 3, 60, 0x8000, 0, 0));
        let file = UmpFile {
            tempos: vec![
                Tempo {
                    absolute_tick: 0,
                    us_per_quarter: 500_000,
                },
                Tempo {
                    absolute_tick: 960,
                    us_per_quarter: 400_000,
                },
            ],
            words: message.encode().words().to_vec(),
        };
        let bytes = write(&file).unwrap();
        assert_eq!(&bytes[8..12], &52u32.to_le_bytes());
        assert_eq!(read(&bytes).unwrap(), file);
    }

    #[test]
    fn corrupt_headers_and_payloads_are_rejected() {
        let base = write(&UmpFile {
            tempos: Vec::new(),
            words: Vec::new(),
        })
        .unwrap();

        assert!(matches!(
            read(&base[..12]),
            Err(UmpFileError::ShortHeader { .. })
        ));
        let mut bad = base.clone();
        bad[0] = b'X';
        assert_eq!(read(&bad), Err(UmpFileError::BadMagic));
        let mut bad = base.clone();
        bad[12..16].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(read(&bad), Err(UmpFileError::UnsupportedFlags(1)));
        let mut bad = base.clone();
        bad[8..12].copy_from_slice(&40u32.to_le_bytes());
        assert!(matches!(
            read(&bad),
            Err(UmpFileError::BadHeaderLength { .. })
        ));
        let mut bad = base;
        bad[20..28].copy_from_slice(&1u64.to_le_bytes());
        assert!(matches!(
            read(&bad),
            Err(UmpFileError::WordCountMismatch { .. })
        ));
    }

    #[test]
    fn raw_read_and_write_reject_standalone_sysex7_end() {
        let mut words = Utility::delta_clockstamp_tpq(480).encode().words().to_vec();
        words.extend_from_slice(
            SysEx7::new(0, SysEx7Format::End, &[1])
                .unwrap()
                .encode()
                .words(),
        );
        let file = UmpFile {
            tempos: Vec::new(),
            words: words.clone(),
        };
        assert!(matches!(
            write(&file),
            Err(UmpFileError::InvalidSysEx7Topology {
                message_index: 1,
                group: 0,
                detail,
            }) if detail.contains("standalone")
        ));

        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&28u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(words.len() as u64).to_le_bytes());
        for word in words {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        assert_eq!(bytes.len(), 40);
        assert!(matches!(
            read(&bytes),
            Err(UmpFileError::InvalidSysEx7Topology {
                message_index: 1,
                group: 0,
                detail,
            }) if detail.contains("standalone")
        ));
    }
}
