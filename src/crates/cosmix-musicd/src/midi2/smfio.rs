//! Standard MIDI File timeline bridge for the `midi2` CLI.
//!
//! Format 0 and 1 files are imported through [`crate::smf`]. Format 1 tracks
//! are merged deterministically by `(absolute tick, track index, event
//! index)`. Meta events are dropped except Set Tempo (`FF 51`), which is
//! retained in the `.cosump` header. SMPTE division is rejected in Phase 1.
//! A per-track SysEx run must remain contiguous in that full ordering,
//! including equal-tick tempo events; a format-1 file whose tracks would
//! interleave such a run is rejected rather than semantically reordered.
//!
//! The UMP stream begins with Delta Clockstamp Ticks Per Quarter Note,
//! followed by 20-bit Delta Clockstamp chunks and translated messages. SMF
//! export is canonical format 0 with explicit channel statuses, tempo metas,
//! and End Of Track. This module is a file/CLI adapter; the packet and
//! translator layers remain independent of SMF.
//! SMF and MIDI 1.0 wire events have no UMP group field: export projects every
//! non-zero group to group zero and counts one `group-routing` loss per source
//! message that produces MIDI 1.0 output. UMP SysEx7 topology is validated
//! independently for all 16 groups before translation. Multipart runs are
//! then coalesced per group into one complete SMF SysEx event at the run's
//! Start tick, so projecting independent groups cannot create an invalid
//! single-stream SMF interleave. If fragment ticks differ from the Start tick,
//! or legal Real-Time inside the run is reordered behind that atomic event,
//! the projection is counted once per run as `sysex-timing` loss.
//!
//! While a SysEx7 run is open, another group and groupless Utility messages
//! may proceed independently; on that same group only System Real-Time and
//! the matching Continue/End packets may interleave. NI's
//! `tests/data_message_tests.cpp` (`make_sysex7_*_packet`) witnesses the group
//! field on every fragment, AM's `docs/umpProcessor.md` states that Utility
//! MT `0x0` is groupless, and NI's `tests/midi1_byte_stream_tests.cpp`
//! witnesses Real-Time interleaving in a MIDI SysEx stream.
//!
//! Sparse timing expansion is deliberately bounded before any timing or
//! filler events are allocated. One shared cap and counting helper cover both
//! Timeline-to-UMP Delta Clockstamps and UMP-to-SMF delta bridging. SMF export
//! additionally limits [`MAX_SMF_ABSOLUTE_TICK`] ticks and
//! [`MAX_SMF_EXPORT_BYTES`] estimated output bytes.

use std::{error::Error, fmt};

use super::{
    bytestream::{self, Event, SysEx, TimedEvent},
    cv1::Midi1Cv,
    cv2::Midi2Cv,
    down::{DownTranslator, Dropped},
    msg::{
        Data128, Message, SysEx7, SysEx7Topology, SysEx7TopologyError, System, Utility, messages,
    },
    umpfile::{Tempo, UmpFile, UmpFileError},
    up::UpTranslator,
};

const DELTA_CLOCKSTAMP_MAX: u64 = 0x000F_FFFF;
const SMF_VARLEN_MAX: u64 = 0x0FFF_FFFF;
/// Shared cap for timing records synthesised from sparse tick distances.
pub const MAX_TIMING_EXPANSION_EVENTS: u64 = 4096;
/// Maximum number of no-op sequencer-specific events inserted solely to
/// bridge SMF's four-byte delta limit.
pub const MAX_SMF_FILLER_EVENTS: u64 = MAX_TIMING_EXPANSION_EVENTS;
/// Maximum absolute tick accepted for SMF export.
pub const MAX_SMF_ABSOLUTE_TICK: u64 = SMF_VARLEN_MAX * (MAX_SMF_FILLER_EVENTS + 1);
/// Maximum estimated canonical SMF output size.
pub const MAX_SMF_EXPORT_BYTES: u64 = 256 * 1024 * 1024;

/// One decoded UMP message at an absolute musical tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedMessage {
    /// Absolute tick in [`Timeline::ticks_per_quarter`].
    pub tick: u64,
    /// Semantic message; timing Utility messages are consumed by the bridge.
    pub message: Message,
}

/// Canonical merged musical timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timeline {
    /// Positive metrical SMF division.
    pub ticks_per_quarter: u16,
    /// Tempo map in deterministic track/event order at equal ticks.
    pub tempos: Vec<Tempo>,
    /// Messages ordered by absolute tick and original merged event order.
    pub events: Vec<TimedMessage>,
}

/// Canonical merged MIDI 1.0 timeline used by the independent round-trip
/// oracle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Midi1Timeline {
    /// Positive metrical SMF division.
    pub ticks_per_quarter: u16,
    /// Tempo map in deterministic track/event order at equal ticks.
    pub tempos: Vec<Tempo>,
    /// Typed MIDI 1.0 events retaining their source absolute ticks.
    pub events: Vec<TimedEvent>,
}

/// Canonical format-0 SMF plus translation loss counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedSmf {
    /// Complete Standard MIDI File bytes.
    pub bytes: Vec<u8>,
    /// MIDI 2-only semantics omitted or reduced during down-translation.
    pub dropped: Dropped,
}

/// Result of the semantic MIDI 1.0 → MIDI 2.0 → MIDI 1.0 round-trip check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundtripReport {
    /// Independently canonicalised merged source MIDI 1.0 timeline.
    pub expected: Midi1Timeline,
    /// Real export then re-import MIDI 1.0 timeline.
    pub actual: Midi1Timeline,
    /// Loss counters from the down boundary.
    pub dropped: Dropped,
}

impl RoundtripReport {
    /// First semantic mismatch, including division and tempo map.
    pub fn first_divergence(&self) -> Option<String> {
        if self.expected.ticks_per_quarter != self.actual.ticks_per_quarter {
            return Some(format!(
                "division: expected {}, got {}",
                self.expected.ticks_per_quarter, self.actual.ticks_per_quarter
            ));
        }
        if self.expected.tempos != self.actual.tempos {
            let limit = self.expected.tempos.len().max(self.actual.tempos.len());
            for index in 0..limit {
                if self.expected.tempos.get(index) != self.actual.tempos.get(index) {
                    return Some(format!(
                        "tempo #{index}: expected {:?}, got {:?}",
                        self.expected.tempos.get(index),
                        self.actual.tempos.get(index)
                    ));
                }
            }
        }
        if self.expected.events != self.actual.events {
            let limit = self.expected.events.len().max(self.actual.events.len());
            for index in 0..limit {
                if self.expected.events.get(index) != self.actual.events.get(index) {
                    return Some(format!(
                        "event #{index}: expected {:?}, got {:?}",
                        self.expected.events.get(index),
                        self.actual.events.get(index)
                    ));
                }
            }
        }
        None
    }
}

/// SMF or UMP timeline conversion error.
#[derive(Debug)]
pub enum SmfIoError {
    /// Existing SMF parser rejected the outer container.
    Smf(String),
    /// Only SMF formats 0 and 1 are supported.
    UnsupportedFormat(u16),
    /// Format 0 must contain exactly one track.
    Format0TrackCount(usize),
    /// SMPTE time division is outside the Phase 1 contract.
    SmpteDivision([u8; 2]),
    /// Metrical division zero is invalid.
    ZeroDivision,
    /// Requested zero-based track is absent.
    TrackOutOfRange {
        /// Requested index.
        requested: usize,
        /// Tracks present.
        tracks: usize,
    },
    /// Malformed track event.
    Track {
        /// Zero-based track index.
        track: usize,
        /// Human-readable failure.
        detail: String,
    },
    /// `.cosump` structure failed validation.
    UmpFile(UmpFileError),
    /// UMP stream has no leading TPQ Utility message.
    MissingTpq,
    /// UMP stream contains more than one TPQ Utility message.
    DuplicateTpq,
    /// TPQ Utility payload is outside `1..=32767`.
    InvalidTpq(u32),
    /// Absolute tick arithmetic overflowed.
    TickOverflow,
    /// Messages are not ordered by non-decreasing tick.
    UnsortedEvents {
        /// Previous tick.
        previous: u64,
        /// Current tick.
        current: u64,
    },
    /// Export would require unreasonable sparse-timeline expansion.
    ExportExpansion {
        /// Largest absolute tick encountered.
        absolute_tick: u64,
        /// No-op filler events that would be generated.
        filler_events: u64,
        /// Estimated complete SMF size in bytes.
        estimated_bytes: u64,
    },
    /// Timeline-to-UMP timing would synthesise too many Delta Clockstamps.
    TimelineExpansion {
        /// Absolute tick that exceeded the shared expansion bound.
        absolute_tick: u64,
        /// Delta Clockstamp packets that would be required.
        timing_events: u64,
    },
    /// SysEx7 packet framing cannot form a valid contiguous SMF SysEx run.
    SysEx7Topology {
        /// Absolute tick at which validation failed.
        tick: u64,
        /// UMP group whose topology failed.
        group: u8,
        /// Human-readable topology violation.
        detail: &'static str,
    },
    /// Valid per-track SysEx runs would interleave in the format-1 merge.
    MergedSysExInterleave {
        /// Track whose SysEx run is open.
        open_track: usize,
        /// Tick at which that run started.
        open_tick: u64,
        /// Track whose event would interrupt the run.
        interfering_track: usize,
        /// Tick of the interfering event.
        interfering_tick: u64,
    },
    /// A typed event unexpectedly failed MIDI 1.0 serialisation.
    Midi1Serialize(bytestream::SerializeError),
}

impl fmt::Display for SmfIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Smf(detail) => write!(f, "SMF parse failed: {detail}"),
            Self::UnsupportedFormat(format) => {
                write!(f, "SMF format {format} is unsupported (expected 0 or 1)")
            }
            Self::Format0TrackCount(tracks) => {
                write!(f, "SMF format 0 must have one track, found {tracks}")
            }
            Self::SmpteDivision(raw) => write!(
                f,
                "SMPTE SMF division 0x{:02X}{:02X} is unsupported in MIDI 2 Phase 1",
                raw[0], raw[1]
            ),
            Self::ZeroDivision => write!(f, "SMF ticks-per-quarter division must be non-zero"),
            Self::TrackOutOfRange { requested, tracks } => write!(
                f,
                "SMF track {requested} is out of range (file has {tracks} tracks)"
            ),
            Self::Track { track, detail } => {
                write!(f, "malformed SMF track {track}: {detail}")
            }
            Self::UmpFile(error) => error.fmt(f),
            Self::MissingTpq => write!(f, ".cosump UMP stream does not begin with TPQ Utility"),
            Self::DuplicateTpq => write!(f, ".cosump UMP stream contains duplicate TPQ Utility"),
            Self::InvalidTpq(tpq) => write!(f, "invalid TPQ Utility value {tpq}"),
            Self::TickOverflow => write!(f, "musical tick arithmetic overflow"),
            Self::UnsortedEvents { previous, current } => write!(
                f,
                "timeline events are unsorted: tick {current} follows {previous}"
            ),
            Self::ExportExpansion {
                absolute_tick,
                filler_events,
                estimated_bytes,
            } => write!(
                f,
                "SMF export expansion rejected: tick {absolute_tick}, {filler_events} filler events, estimated {estimated_bytes} bytes (limits: tick {MAX_SMF_ABSOLUTE_TICK}, {MAX_SMF_FILLER_EVENTS} fillers, {MAX_SMF_EXPORT_BYTES} bytes)"
            ),
            Self::TimelineExpansion {
                absolute_tick,
                timing_events,
            } => write!(
                f,
                "UMP timing expansion rejected: tick {absolute_tick} requires {timing_events} Delta Clockstamps (limit {MAX_TIMING_EXPANSION_EVENTS})"
            ),
            Self::SysEx7Topology {
                tick,
                group,
                detail,
            } => {
                write!(
                    f,
                    "invalid SysEx7 topology in group {group} at tick {tick}: {detail}"
                )
            }
            Self::MergedSysExInterleave {
                open_track,
                open_tick,
                interfering_track,
                interfering_tick,
            } => write!(
                f,
                "format-1 SysEx merge would interleave track {interfering_track} at tick {interfering_tick} inside track {open_track}'s SysEx run opened at tick {open_tick}"
            ),
            Self::Midi1Serialize(error) => write!(f, "MIDI 1.0 serialisation failed: {error:?}"),
        }
    }
}

impl Error for SmfIoError {}

impl From<UmpFileError> for SmfIoError {
    fn from(value: UmpFileError) -> Self {
        Self::UmpFile(value)
    }
}

/// Import format 0/1 SMF and up-translate its merged MIDI events.
///
/// `track = None` merges all tracks. `Some(n)` selects MIDI events from
/// zero-based track `n`; tempo events remain file-global and are collected
/// from every track, matching SMF format-1 conductor-track semantics.
pub fn import_smf(bytes: &[u8], track: Option<usize>) -> Result<Timeline, SmfIoError> {
    let source = import_midi1_smf(bytes, track)?;
    let timeline = up_timeline(&source);
    validate_timeline_sysex(&timeline.events)?;
    Ok(timeline)
}

fn import_midi1_smf(bytes: &[u8], track: Option<usize>) -> Result<Midi1Timeline, SmfIoError> {
    let smf = crate::smf::parse(bytes).map_err(|error| SmfIoError::Smf(error.to_string()))?;
    match smf.format {
        0 if smf.tracks.len() != 1 => {
            return Err(SmfIoError::Format0TrackCount(smf.tracks.len()));
        }
        0 | 1 => {}
        other => return Err(SmfIoError::UnsupportedFormat(other)),
    }
    if smf.division[0] & 0x80 != 0 {
        return Err(SmfIoError::SmpteDivision(smf.division));
    }
    let ticks_per_quarter = u16::from_be_bytes(smf.division);
    if ticks_per_quarter == 0 {
        return Err(SmfIoError::ZeroDivision);
    }
    if let Some(requested) = track
        && requested >= smf.tracks.len()
    {
        return Err(SmfIoError::TrackOutOfRange {
            requested,
            tracks: smf.tracks.len(),
        });
    }

    let mut tempos = Vec::new();
    let mut midi = Vec::new();
    for (track_index, body) in smf.tracks.iter().enumerate() {
        parse_track(
            track_index,
            body,
            track.is_none_or(|selected| selected == track_index),
            &mut tempos,
            &mut midi,
        )?;
    }
    tempos.sort_by_key(|tempo| (tempo.tick, tempo.track, tempo.event));
    midi.sort_by_key(|event| (event.tick, event.track, event.event, event.fragment));
    validate_merged_sysex(&midi, &tempos)?;

    let tempos = tempos
        .into_iter()
        .map(|tempo| Tempo {
            absolute_tick: tempo.tick,
            us_per_quarter: tempo.us_per_quarter,
        })
        .collect();
    let events = midi
        .into_iter()
        .map(|event| TimedEvent {
            tick: event.tick,
            event: event.message,
        })
        .collect();
    Ok(Midi1Timeline {
        ticks_per_quarter,
        tempos,
        events,
    })
}

/// Encode a semantic timeline as a `.cosump` value.
pub fn to_ump_file(timeline: &Timeline) -> Result<UmpFile, SmfIoError> {
    validate_tpq(u32::from(timeline.ticks_per_quarter))?;
    validate_timeline_sysex(&timeline.events)?;
    preflight_ump_timing(&timeline.events)?;
    let mut words = Utility::delta_clockstamp_tpq(timeline.ticks_per_quarter)
        .encode()
        .words()
        .to_vec();
    let mut tick = 0;
    for event in &timeline.events {
        if event.tick < tick {
            return Err(SmfIoError::UnsortedEvents {
                previous: tick,
                current: event.tick,
            });
        }
        let mut delta = event.tick - tick;
        // `preflight_ump_timing` bounds the aggregate iterations of this loop.
        while delta > 0 {
            let chunk = delta.min(DELTA_CLOCKSTAMP_MAX);
            words.extend_from_slice(Utility::delta_clockstamp(chunk as u32).encode().words());
            delta -= chunk;
        }
        words.extend_from_slice(event.message.encode().words());
        tick = event.tick;
    }
    let file = UmpFile {
        tempos: timeline.tempos.clone(),
        words,
    };
    // Reuse the container's validation without retaining the bytes.
    super::umpfile::write(&file)?;
    Ok(file)
}

/// Decode TPQ/Delta Clockstamp timing from a `.cosump` value.
pub fn from_ump_file(file: &UmpFile) -> Result<Timeline, SmfIoError> {
    // Public callers may construct UmpFile directly instead of using read().
    // Apply the same tempo and packet validation at this boundary.
    let _ = super::umpfile::write(file)?;
    let mut decoded = messages(&file.words);
    let Some(first) = decoded.next() else {
        return Err(SmfIoError::MissingTpq);
    };
    let first = first.map_err(|error| SmfIoError::UmpFile(UmpFileError::TruncatedUmp(error)))?;
    let Message::Utility(Utility::DeltaClockstampTpq(value)) = first else {
        return Err(SmfIoError::MissingTpq);
    };
    let ticks_per_quarter = validate_tpq(value.value())?;

    let mut tick = 0u64;
    let mut events = Vec::new();
    for result in decoded {
        let message =
            result.map_err(|error| SmfIoError::UmpFile(UmpFileError::TruncatedUmp(error)))?;
        match message {
            Message::Utility(Utility::DeltaClockstampTpq(_)) => {
                return Err(SmfIoError::DuplicateTpq);
            }
            Message::Utility(Utility::DeltaClockstamp(value)) => {
                tick = tick
                    .checked_add(u64::from(value.value()))
                    .ok_or(SmfIoError::TickOverflow)?;
            }
            _ => {
                events.push(TimedMessage { tick, message });
            }
        }
    }
    validate_timeline_sysex(&events)?;
    Ok(Timeline {
        ticks_per_quarter,
        tempos: file.tempos.clone(),
        events,
    })
}

/// Down-translate a timeline into canonical format-0 SMF.
pub fn export_smf(timeline: &Timeline) -> Result<ExportedSmf, SmfIoError> {
    validate_tpq(u32::from(timeline.ticks_per_quarter))?;
    // Validate the complete source stream before down-translation. A message
    // that will be dropped is still a topology interruption on its UMP group.
    validate_timeline_sysex(&timeline.events)?;
    let mut output = Vec::new();
    for (index, tempo) in timeline.tempos.iter().enumerate() {
        if !(1..=0x00FF_FFFF).contains(&tempo.us_per_quarter) {
            return Err(SmfIoError::UmpFile(UmpFileError::InvalidTempo {
                index,
                us_per_quarter: tempo.us_per_quarter,
            }));
        }
        output.push(OutputEvent {
            tick: tempo.absolute_tick,
            priority: 0,
            sequence: index,
            bytes: tempo_meta(tempo.us_per_quarter),
        });
    }

    let mut down = DownTranslator::new();
    let mut sysex_projection = SmfSysExProjection::default();
    let mut sequence = 0usize;
    let mut previous_tick = 0;
    let mut projected_groups = 0u64;
    let mut collapsed_sysex_timing = 0u64;
    for timed in &timeline.events {
        if timed.tick < previous_tick {
            return Err(SmfIoError::UnsortedEvents {
                previous: previous_tick,
                current: timed.tick,
            });
        }
        previous_tick = timed.tick;
        sysex_projection.observe_ump(timed.message);

        if let Message::SysEx7(
            message @ (SysEx7::Complete(_)
            | SysEx7::Start(_)
            | SysEx7::Continue(_)
            | SysEx7::End(_)),
        ) = timed.message
        {
            if message.group() != 0 {
                projected_groups = projected_groups.saturating_add(1);
            }
            if let Some(projected) = sysex_projection.push(message, timed.tick, &mut sequence)? {
                if projected.timing_collapsed {
                    collapsed_sysex_timing = collapsed_sysex_timing.saturating_add(1);
                }
                output.push(OutputEvent {
                    tick: projected.tick,
                    priority: 1,
                    sequence: projected.sequence,
                    bytes: encode_complete_sysex(&projected.payload),
                });
            }
            continue;
        }

        let translated = down.translate(timed.message);
        if !translated.is_empty() && timed.message.group().is_some_and(|group| group != 0) {
            projected_groups = projected_groups.saturating_add(1);
        }
        for event in translated {
            output.push(OutputEvent {
                tick: timed.tick,
                priority: 1,
                sequence,
                bytes: encode_smf_event(event)?,
            });
            sequence += 1;
        }
    }
    output.sort_by_key(|event| (event.tick, event.priority, event.sequence));
    preflight_smf_export(&output)?;

    let mut track = Vec::new();
    let mut written_tick = 0;
    for event in output {
        write_at_tick(&mut track, &mut written_tick, event.tick, &event.bytes);
    }
    write_varlen(&mut track, 0);
    track.extend_from_slice(&[0xFF, 0x2F, 0]);
    let mut dropped = down.dropped();
    dropped.sysex_timing = dropped.sysex_timing.saturating_add(collapsed_sysex_timing);
    dropped.group_routing = dropped.group_routing.saturating_add(projected_groups);
    Ok(ExportedSmf {
        bytes: crate::smf::assemble_format0(timeline.ticks_per_quarter.to_be_bytes(), &track),
        dropped,
    })
}

/// Run the CLI's semantic invariant check through the real export path.
///
/// The actual side is [`export_smf`] followed by format-0 re-import. The
/// independent MIDI 1.0 expected side shares only the exact SysEx projection
/// state machine, so intentional atomic tick collapse is canonicalised once
/// rather than reimplemented. Projection-internal tick collapse and Real-Time
/// reordering therefore appear in [`Dropped::sysex_timing`], not as structural
/// divergences.
pub fn roundtrip_smf(bytes: &[u8], track: Option<usize>) -> Result<RoundtripReport, SmfIoError> {
    let source = import_midi1_smf(bytes, track)?;
    let expected = canonicalize_for_smf_projection(&source)?;
    let translated = up_timeline(&source);
    let exported = export_smf(&translated)?;
    let actual = import_midi1_smf(&exported.bytes, None)?;
    Ok(RoundtripReport {
        expected,
        actual,
        dropped: exported.dropped,
    })
}

/// Stable human-readable dump, one semantic message per line.
pub fn dump_lines(timeline: &Timeline) -> impl Iterator<Item = String> + '_ {
    timeline.events.iter().copied().map(dump_line)
}

/// Render only non-zero loss counters, suitable for stderr.
pub fn dropped_lines(dropped: Dropped) -> Vec<String> {
    let values = [
        ("per-note-controllers", dropped.per_note_controllers),
        ("per-note-pitch-bend", dropped.per_note_pitch_bend),
        ("per-note-management", dropped.per_note_management),
        ("note-attributes", dropped.note_attributes),
        ("relative-controllers", dropped.relative_controllers),
        ("data128", dropped.data128),
        ("jr-utility", dropped.jr_utility),
        ("other-utility", dropped.other_utility),
        ("sysex-timing", dropped.sysex_timing),
        ("group-routing", dropped.group_routing),
        ("unknown", dropped.unknown),
    ];
    values
        .into_iter()
        .filter(|(_, count)| *count != 0)
        .map(|(name, count)| format!("dropped {name}: {count}"))
        .collect()
}

#[derive(Debug)]
struct TempoEvent {
    tick: u64,
    track: usize,
    event: usize,
    us_per_quarter: u32,
}

#[derive(Debug)]
struct MidiEvent {
    tick: u64,
    track: usize,
    event: usize,
    fragment: usize,
    message: Event,
}

fn validate_merged_sysex(events: &[MidiEvent], tempos: &[TempoEvent]) -> Result<(), SmfIoError> {
    let mut open: Option<(usize, u64)> = None;
    let mut event_index = 0;
    let mut tempo_index = 0;
    while event_index < events.len() || tempo_index < tempos.len() {
        let take_tempo = match (events.get(event_index), tempos.get(tempo_index)) {
            (Some(event), Some(tempo)) => {
                (tempo.tick, tempo.track, tempo.event, 0)
                    < (event.tick, event.track, event.event, event.fragment)
            }
            (None, Some(_)) => true,
            _ => false,
        };
        if take_tempo {
            let tempo = &tempos[tempo_index];
            tempo_index += 1;
            if let Some((open_track, open_tick)) = open {
                return Err(SmfIoError::MergedSysExInterleave {
                    open_track,
                    open_tick,
                    interfering_track: tempo.track,
                    interfering_tick: tempo.tick,
                });
            }
            continue;
        }

        let event = &events[event_index];
        event_index += 1;
        match event.message {
            Event::SysEx(SysEx::Complete(_)) | Event::SysEx(SysEx::Start(_)) if open.is_some() => {
                let (open_track, open_tick) = open.expect("guarded above");
                return Err(SmfIoError::MergedSysExInterleave {
                    open_track,
                    open_tick,
                    interfering_track: event.track,
                    interfering_tick: event.tick,
                });
            }
            Event::SysEx(SysEx::Start(_)) => open = Some((event.track, event.tick)),
            Event::SysEx(SysEx::Continue(_)) | Event::SysEx(SysEx::End(_)) => {
                let Some((open_track, open_tick)) = open else {
                    return Err(SmfIoError::SysEx7Topology {
                        tick: event.tick,
                        group: 0,
                        detail: "merged SysEx7 Continue/End has no open Start",
                    });
                };
                if open_track != event.track {
                    return Err(SmfIoError::MergedSysExInterleave {
                        open_track,
                        open_tick,
                        interfering_track: event.track,
                        interfering_tick: event.tick,
                    });
                }
                if matches!(event.message, Event::SysEx(SysEx::End(_))) {
                    open = None;
                }
            }
            _ if open.is_some() && !is_realtime_event(event.message) => {
                let (open_track, open_tick) = open.expect("guarded above");
                return Err(SmfIoError::MergedSysExInterleave {
                    open_track,
                    open_tick,
                    interfering_track: event.track,
                    interfering_tick: event.tick,
                });
            }
            _ => {}
        }
    }
    if let Some((_, open_tick)) = open {
        Err(SmfIoError::SysEx7Topology {
            tick: open_tick,
            group: 0,
            detail: "merged SysEx7 Start is unterminated",
        })
    } else {
        Ok(())
    }
}

fn validate_timeline_sysex(events: &[TimedMessage]) -> Result<(), SmfIoError> {
    let mut topology = SysEx7Topology::default();
    let mut last_tick = 0;
    for timed in events {
        topology
            .push(timed.message, timed.tick)
            .map_err(map_topology_error)?;
        last_tick = timed.tick;
    }
    topology.finish(last_tick).map_err(map_topology_error)
}

fn map_topology_error(error: SysEx7TopologyError) -> SmfIoError {
    SmfIoError::SysEx7Topology {
        tick: error.location,
        group: error.group,
        detail: error.detail,
    }
}

fn parse_track(
    track_index: usize,
    bytes: &[u8],
    collect_midi: bool,
    tempos: &mut Vec<TempoEvent>,
    midi: &mut Vec<MidiEvent>,
) -> Result<(), SmfIoError> {
    let mut position = 0usize;
    let mut tick = 0u64;
    let mut running = None;
    let mut sysex_open = false;
    let mut event_index = 0usize;
    while position < bytes.len() {
        let delta =
            read_varlen(bytes, &mut position).map_err(|detail| track_error(track_index, detail))?;
        tick = tick
            .checked_add(u64::from(delta))
            .ok_or(SmfIoError::TickOverflow)?;
        let first = *bytes
            .get(position)
            .ok_or_else(|| track_error(track_index, "truncated event"))?;
        let status = if first & 0x80 != 0 {
            position += 1;
            first
        } else {
            running
                .ok_or_else(|| track_error(track_index, "running status without channel status"))?
        };
        if sysex_open && status != 0xF7 {
            let detail = if status == 0xF0 {
                "SysEx Start while a SysEx is already open"
            } else if status == 0xFF && bytes.get(position) == Some(&0x2F) {
                "End Of Track while SysEx is open"
            } else {
                "non-continuation event while SysEx is open"
            };
            return Err(track_error(track_index, detail));
        }
        match status {
            0x80..=0xEF => {
                running = Some(status);
                let len = if matches!(status & 0xF0, 0xC0 | 0xD0) {
                    1
                } else {
                    2
                };
                let data = take(bytes, &mut position, len)
                    .map_err(|detail| track_error(track_index, detail))?;
                if data.iter().any(|byte| byte & 0x80 != 0) {
                    return Err(track_error(track_index, "channel data has high bit set"));
                }
                if collect_midi {
                    midi.push(MidiEvent {
                        tick,
                        track: track_index,
                        event: event_index,
                        fragment: 0,
                        message: decode_channel(status, data),
                    });
                }
            }
            0xFF => {
                running = None;
                let kind = *bytes
                    .get(position)
                    .ok_or_else(|| track_error(track_index, "truncated meta type"))?;
                position += 1;
                let len = read_varlen(bytes, &mut position)
                    .map_err(|detail| track_error(track_index, detail))?
                    as usize;
                let data = take(bytes, &mut position, len)
                    .map_err(|detail| track_error(track_index, detail))?;
                if kind == 0x51 {
                    if data.len() != 3 {
                        return Err(track_error(
                            track_index,
                            "Set Tempo meta payload is not three bytes",
                        ));
                    }
                    let value =
                        u32::from(data[0]) << 16 | u32::from(data[1]) << 8 | u32::from(data[2]);
                    if value == 0 {
                        return Err(track_error(track_index, "Set Tempo value is zero"));
                    }
                    tempos.push(TempoEvent {
                        tick,
                        track: track_index,
                        event: event_index,
                        us_per_quarter: value,
                    });
                }
                if kind == 0x2F {
                    return Ok(());
                }
            }
            0xF0 | 0xF7 => {
                running = None;
                let len = read_varlen(bytes, &mut position)
                    .map_err(|detail| track_error(track_index, detail))?
                    as usize;
                let data = take(bytes, &mut position, len)
                    .map_err(|detail| track_error(track_index, detail))?;
                let escaped = status == 0xF7
                    && data
                        .first()
                        .is_some_and(|byte| *byte >= 0x80 && *byte != 0xF7);
                if escaped {
                    let parsed = bytestream::parse_raw(data);
                    if parsed.stats.skipped_bytes != 0
                        || parsed.stats.aborted_messages != 0
                        || parsed.stats.aborted_sysex != 0
                    {
                        return Err(track_error(
                            track_index,
                            "malformed MIDI bytes in F7 escape event",
                        ));
                    }
                    if sysex_open && parsed.events.iter().any(|event| !is_realtime_event(*event)) {
                        return Err(track_error(
                            track_index,
                            "non-Real-Time F7 escape while SysEx is open",
                        ));
                    }
                    if collect_midi {
                        for (fragment, message) in parsed.events.into_iter().enumerate() {
                            midi.push(MidiEvent {
                                tick,
                                track: track_index,
                                event: event_index,
                                fragment,
                                message,
                            });
                        }
                    }
                } else {
                    append_sysex(
                        track_index,
                        event_index,
                        tick,
                        status,
                        data,
                        &mut sysex_open,
                        if collect_midi { Some(&mut *midi) } else { None },
                    )?;
                }
            }
            _ => {
                return Err(track_error(
                    track_index,
                    format!("unsupported status 0x{status:02X}"),
                ));
            }
        }
        event_index += 1;
    }
    if sysex_open {
        Err(track_error(track_index, "track ended while SysEx is open"))
    } else {
        Ok(())
    }
}

fn append_sysex(
    track: usize,
    event: usize,
    tick: u64,
    status: u8,
    data: &[u8],
    open: &mut bool,
    output: Option<&mut Vec<MidiEvent>>,
) -> Result<(), SmfIoError> {
    if status == 0xF0 && *open {
        return Err(track_error(
            track,
            "SysEx Start while a SysEx is already open",
        ));
    }
    if status == 0xF7 && !*open {
        return Err(track_error(
            track,
            "SysEx Continue/End without an open SysEx",
        ));
    }
    let ended = data.last() == Some(&0xF7);
    let payload = if ended { &data[..data.len() - 1] } else { data };
    if payload.iter().any(|byte| byte & 0x80 != 0) {
        return Err(track_error(track, "SysEx payload has a non-seven-bit byte"));
    }
    let starts = status == 0xF0;
    let mut messages = Vec::new();
    let mut chunks = payload.chunks(6).peekable();
    let mut fragment = 0;
    if payload.is_empty() {
        let message = match (starts, ended) {
            (true, true) => SysEx::complete(&[]),
            (true, false) => SysEx::start(&[]),
            (false, true) => SysEx::end(&[]),
            (false, false) => SysEx::continue_(&[]),
        }
        .expect("empty SysEx payload is valid");
        messages.push((fragment, message));
    } else {
        while let Some(chunk) = chunks.next() {
            let first = fragment == 0;
            let last = chunks.peek().is_none();
            let message = if starts && first && ended && last {
                SysEx::complete(chunk)
            } else if starts && first {
                SysEx::start(chunk)
            } else if ended && last {
                SysEx::end(chunk)
            } else {
                SysEx::continue_(chunk)
            }
            .expect("six-byte seven-bit chunk");
            messages.push((fragment, message));
            fragment += 1;
        }
    }
    if let Some(output) = output {
        output.extend(messages.into_iter().map(|(fragment, message)| MidiEvent {
            tick,
            track,
            event,
            fragment,
            message: Event::SysEx(message),
        }));
    }
    *open = !ended;
    Ok(())
}

fn decode_channel(status: u8, data: &[u8]) -> Event {
    let channel = status & 0xF;
    let first = data[0];
    let second = data.get(1).copied().unwrap_or(0);
    let message = match status >> 4 {
        0x8 => Midi1Cv::note_off(0, channel, first, second),
        0x9 => Midi1Cv::note_on(0, channel, first, second),
        0xA => Midi1Cv::poly_pressure(0, channel, first, second),
        0xB => Midi1Cv::control_change(0, channel, first, second),
        0xC => Midi1Cv::program_change(0, channel, first),
        0xD => Midi1Cv::channel_pressure(0, channel, first),
        0xE => Midi1Cv::pitch_bend(0, channel, u16::from(first) | (u16::from(second) << 7)),
        _ => unreachable!("caller accepts only Channel Voice status"),
    };
    Event::Midi1Cv(message)
}

fn up_timeline(source: &Midi1Timeline) -> Timeline {
    let mut translator = UpTranslator::new();
    let mut pending_tick = None;
    let mut events = Vec::new();
    for timed in &source.events {
        let had_pending = translator.pending().data_entries() != 0;
        let old_pending_tick = pending_tick;
        let realtime = is_realtime_event(timed.event);
        let translated = translator.translate(timed.event);
        for (index, message) in translated.into_iter().enumerate() {
            let tick = if had_pending && !realtime && index == 0 {
                old_pending_tick.expect("pending Data Entry has a source tick")
            } else {
                timed.tick
            };
            events.push(TimedMessage { tick, message });
        }
        if translator.pending().data_entries() != 0 {
            if !had_pending || !realtime {
                pending_tick = Some(timed.tick);
            }
        } else {
            pending_tick = None;
        }
    }
    let flush_tick = pending_tick.unwrap_or(0);
    events.extend(translator.flush().into_iter().map(|message| TimedMessage {
        tick: flush_tick,
        message,
    }));
    events.sort_by_key(|event| event.tick);
    Timeline {
        ticks_per_quarter: source.ticks_per_quarter,
        tempos: source.tempos.clone(),
        events,
    }
}

fn canonicalize_for_smf_projection(source: &Midi1Timeline) -> Result<Midi1Timeline, SmfIoError> {
    let canonical = bytestream::canonicalize_timed(&source.events);
    let mut projection = SmfSysExProjection::default();
    let mut sequence = 0usize;
    let mut ordered = Vec::new();
    for timed in canonical {
        projection.observe_midi1(timed.event);
        if let Event::SysEx(message) = timed.event {
            if let Some(projected) = projection.push_midi1(message, timed.tick, &mut sequence)? {
                for (fragment, event) in projected_sysex_events(&projected.payload)
                    .into_iter()
                    .enumerate()
                {
                    ordered.push(OrderedMidiEvent {
                        tick: projected.tick,
                        sequence: projected.sequence,
                        fragment,
                        event,
                    });
                }
            }
        } else {
            ordered.push(OrderedMidiEvent {
                tick: timed.tick,
                sequence,
                fragment: 0,
                event: timed.event,
            });
            sequence += 1;
        }
    }
    ordered.sort_by_key(|event| (event.tick, event.sequence, event.fragment));
    Ok(Midi1Timeline {
        ticks_per_quarter: source.ticks_per_quarter,
        tempos: source.tempos.clone(),
        events: ordered
            .into_iter()
            .map(|event| TimedEvent {
                tick: event.tick,
                event: event.event,
            })
            .collect(),
    })
}

#[derive(Debug)]
struct OrderedMidiEvent {
    tick: u64,
    sequence: usize,
    fragment: usize,
    event: Event,
}

fn projected_sysex_events(payload: &[u8]) -> Vec<Event> {
    let mut wire = Vec::with_capacity(payload.len() + 2);
    wire.push(0xF0);
    wire.extend_from_slice(payload);
    wire.push(0xF7);
    let parsed = bytestream::parse_raw(&wire);
    debug_assert_eq!(parsed.stats, Default::default());
    parsed.events
}

fn is_realtime_event(event: Event) -> bool {
    matches!(event, Event::System(system) if is_realtime_system(system))
}

fn is_realtime_system(system: System) -> bool {
    matches!(
        system,
        System::TimingClock(_)
            | System::Start(_)
            | System::Continue(_)
            | System::Stop(_)
            | System::ActiveSensing(_)
            | System::Reset(_)
    )
}

fn validate_tpq(value: u32) -> Result<u16, SmfIoError> {
    if !(1..=0x7FFF).contains(&value) {
        return Err(SmfIoError::InvalidTpq(value));
    }
    Ok(value as u16)
}

#[derive(Debug)]
struct OutputEvent {
    tick: u64,
    priority: u8,
    sequence: usize,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct OpenSmfSysEx {
    tick: u64,
    sequence: usize,
    payload: Vec<u8>,
    timing_collapsed: bool,
}

#[derive(Debug)]
struct ProjectedSmfSysEx {
    tick: u64,
    sequence: usize,
    payload: Vec<u8>,
    timing_collapsed: bool,
}

#[derive(Debug, Clone, Copy)]
enum SysExFrame {
    Complete,
    Start,
    Continue,
    End,
}

#[derive(Debug, Default)]
struct SmfSysExProjection {
    open: [Option<OpenSmfSysEx>; 16],
}

impl SmfSysExProjection {
    fn observe_ump(&mut self, message: Message) {
        if let Message::System(system) = message {
            self.observe_system(system);
        }
    }

    fn observe_midi1(&mut self, event: Event) {
        if let Event::System(system) = event {
            self.observe_system(system);
        }
    }

    fn observe_system(&mut self, system: System) {
        if is_realtime_system(system)
            && let Some(open) = self.open[usize::from(system.group())].as_mut()
        {
            // Atomic projection hoists the completed SysEx ahead of this
            // legal Real-Time interleave, including when both share a tick.
            open.timing_collapsed = true;
        }
    }

    fn push(
        &mut self,
        message: SysEx7,
        tick: u64,
        sequence: &mut usize,
    ) -> Result<Option<ProjectedSmfSysEx>, SmfIoError> {
        let group = message.group();
        match message {
            SysEx7::Complete(packet) => {
                self.push_parts(group, SysExFrame::Complete, packet.data(), tick, sequence)
            }
            SysEx7::Start(packet) => {
                self.push_parts(group, SysExFrame::Start, packet.data(), tick, sequence)
            }
            SysEx7::Continue(packet) => {
                self.push_parts(group, SysExFrame::Continue, packet.data(), tick, sequence)
            }
            SysEx7::End(packet) => {
                self.push_parts(group, SysExFrame::End, packet.data(), tick, sequence)
            }
            SysEx7::Unknown(_) => unreachable!("caller handles only known SysEx7 framing"),
        }
    }

    fn push_midi1(
        &mut self,
        message: SysEx,
        tick: u64,
        sequence: &mut usize,
    ) -> Result<Option<ProjectedSmfSysEx>, SmfIoError> {
        let frame = match message {
            SysEx::Complete(_) => SysExFrame::Complete,
            SysEx::Start(_) => SysExFrame::Start,
            SysEx::Continue(_) => SysExFrame::Continue,
            SysEx::End(_) => SysExFrame::End,
        };
        self.push_parts(message.group(), frame, message.data(), tick, sequence)
    }

    fn push_parts(
        &mut self,
        group: u8,
        frame: SysExFrame,
        data: &[u8],
        tick: u64,
        sequence: &mut usize,
    ) -> Result<Option<ProjectedSmfSysEx>, SmfIoError> {
        let slot = &mut self.open[usize::from(group)];
        Ok(match frame {
            SysExFrame::Complete => {
                let output = ProjectedSmfSysEx {
                    tick,
                    sequence: *sequence,
                    payload: data.to_vec(),
                    timing_collapsed: false,
                };
                *sequence += 1;
                Some(output)
            }
            SysExFrame::Start => {
                *slot = Some(OpenSmfSysEx {
                    tick,
                    sequence: *sequence,
                    payload: data.to_vec(),
                    timing_collapsed: false,
                });
                *sequence += 1;
                None
            }
            SysExFrame::Continue => {
                let open = slot.as_mut().expect("source SysEx topology was validated");
                open.timing_collapsed |= tick != open.tick;
                append_projected_sysex(&mut open.payload, data, tick)?;
                None
            }
            SysExFrame::End => {
                let mut open = slot.take().expect("source SysEx topology was validated");
                open.timing_collapsed |= tick != open.tick;
                append_projected_sysex(&mut open.payload, data, tick)?;
                Some(ProjectedSmfSysEx {
                    // A complete SMF SysEx is delivered atomically. Retain the
                    // source run's onset while avoiding cross-group fragment
                    // interleaving after groups are projected away.
                    tick: open.tick,
                    sequence: open.sequence,
                    payload: open.payload,
                    timing_collapsed: open.timing_collapsed,
                })
            }
        })
    }
}

fn append_projected_sysex(
    target: &mut Vec<u8>,
    payload: &[u8],
    tick: u64,
) -> Result<(), SmfIoError> {
    let projected_len = target
        .len()
        .checked_add(payload.len())
        .ok_or(SmfIoError::TickOverflow)?;
    let estimated_bytes = projected_len as u64 + 32;
    if projected_len as u64 >= SMF_VARLEN_MAX || estimated_bytes > MAX_SMF_EXPORT_BYTES {
        return Err(SmfIoError::ExportExpansion {
            absolute_tick: tick,
            filler_events: 0,
            estimated_bytes,
        });
    }
    target.extend_from_slice(payload);
    Ok(())
}

fn encode_complete_sysex(payload: &[u8]) -> Vec<u8> {
    debug_assert!((payload.len() as u64) < SMF_VARLEN_MAX);
    let mut encoded = vec![0xF0];
    write_varlen(&mut encoded, payload.len() as u64 + 1);
    encoded.extend_from_slice(payload);
    encoded.push(0xF7);
    encoded
}

fn encode_smf_event(event: Event) -> Result<Vec<u8>, SmfIoError> {
    match event {
        Event::Midi1Cv(_) => bytestream::serialize(&[event]).map_err(SmfIoError::Midi1Serialize),
        Event::System(_) => {
            let wire = bytestream::serialize(&[event]).map_err(SmfIoError::Midi1Serialize)?;
            let mut encoded = vec![0xF7];
            write_varlen(&mut encoded, wire.len() as u64);
            encoded.extend_from_slice(&wire);
            Ok(encoded)
        }
        Event::SysEx(message) => {
            let mut payload = message.data().to_vec();
            let status = match message {
                SysEx::Complete(_) | SysEx::Start(_) => 0xF0,
                SysEx::Continue(_) | SysEx::End(_) => 0xF7,
            };
            if matches!(message, SysEx::Complete(_) | SysEx::End(_)) {
                payload.push(0xF7);
            }
            let mut encoded = vec![status];
            write_varlen(&mut encoded, payload.len() as u64);
            encoded.extend_from_slice(&payload);
            Ok(encoded)
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct BoundedExpansion {
    additional: u64,
    total: u64,
}

fn bounded_expansion(
    current: u64,
    distance: u64,
    span: u64,
    include_terminal: bool,
) -> Result<BoundedExpansion, u64> {
    debug_assert!(span != 0);
    let additional = if distance == 0 {
        0
    } else {
        (distance - 1) / span + u64::from(include_terminal)
    };
    let total = current.saturating_add(additional);
    if total > MAX_TIMING_EXPANSION_EVENTS {
        Err(total)
    } else {
        Ok(BoundedExpansion { additional, total })
    }
}

fn preflight_ump_timing(events: &[TimedMessage]) -> Result<(), SmfIoError> {
    let mut previous_tick = 0;
    let mut timing_events = 0;
    for event in events {
        if event.tick < previous_tick {
            return Err(SmfIoError::UnsortedEvents {
                previous: previous_tick,
                current: event.tick,
            });
        }
        timing_events = bounded_expansion(
            timing_events,
            event.tick - previous_tick,
            DELTA_CLOCKSTAMP_MAX,
            true,
        )
        .map_err(|timing_events| SmfIoError::TimelineExpansion {
            absolute_tick: event.tick,
            timing_events,
        })?
        .total;
        previous_tick = event.tick;
    }
    Ok(())
}

fn tempo_meta(value: u32) -> Vec<u8> {
    vec![
        0xFF,
        0x51,
        3,
        ((value >> 16) & 0xFF) as u8,
        ((value >> 8) & 0xFF) as u8,
        (value & 0xFF) as u8,
    ]
}

fn preflight_smf_export(events: &[OutputEvent]) -> Result<(), SmfIoError> {
    let mut previous_tick = 0;
    let mut filler_events = 0u64;
    // MThd + MTrk headers and final zero-delta End Of Track.
    let mut estimated_bytes = 14u64 + 8 + 4;
    for event in events {
        let delta = event.tick - previous_tick;
        let expansion = bounded_expansion(filler_events, delta, SMF_VARLEN_MAX, false).map_err(
            |filler_events| SmfIoError::ExportExpansion {
                absolute_tick: event.tick,
                filler_events,
                estimated_bytes: estimated_bytes.saturating_add(filler_events.saturating_mul(7)),
            },
        )?;
        filler_events = expansion.total;
        estimated_bytes = estimated_bytes
            .checked_add(
                expansion
                    .additional
                    .checked_mul(7)
                    .ok_or(SmfIoError::TickOverflow)?,
            )
            .and_then(|value| value.checked_add(4))
            .and_then(|value| value.checked_add(event.bytes.len() as u64))
            .ok_or(SmfIoError::TickOverflow)?;
        previous_tick = event.tick;
    }
    if previous_tick > MAX_SMF_ABSOLUTE_TICK || estimated_bytes > MAX_SMF_EXPORT_BYTES {
        return Err(SmfIoError::ExportExpansion {
            absolute_tick: previous_tick,
            filler_events,
            estimated_bytes,
        });
    }
    Ok(())
}

fn write_at_tick(output: &mut Vec<u8>, written_tick: &mut u64, tick: u64, event: &[u8]) {
    let mut delta = tick - *written_tick;
    // `preflight_smf_export` bounds the aggregate iterations of this loop.
    while delta > SMF_VARLEN_MAX {
        write_varlen(output, SMF_VARLEN_MAX);
        output.extend_from_slice(&[0xFF, 0x7F, 0]);
        *written_tick += SMF_VARLEN_MAX;
        delta -= SMF_VARLEN_MAX;
    }
    write_varlen(output, delta);
    output.extend_from_slice(event);
    *written_tick = tick;
}

fn write_varlen(output: &mut Vec<u8>, mut value: u64) {
    debug_assert!(value <= SMF_VARLEN_MAX);
    let mut bytes = [0u8; 4];
    let mut len = 0;
    loop {
        bytes[len] = (value & 0x7F) as u8;
        value >>= 7;
        len += 1;
        if value == 0 {
            break;
        }
    }
    for index in (0..len).rev() {
        output.push(bytes[index] | if index == 0 { 0 } else { 0x80 });
    }
}

fn read_varlen(bytes: &[u8], position: &mut usize) -> Result<u32, &'static str> {
    let mut value = 0u32;
    for _ in 0..4 {
        let byte = *bytes
            .get(*position)
            .ok_or("truncated variable length value")?;
        *position += 1;
        value = (value << 7) | u32::from(byte & 0x7F);
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("variable length value exceeds four bytes")
}

fn take<'a>(bytes: &'a [u8], position: &mut usize, len: usize) -> Result<&'a [u8], &'static str> {
    let end = position.checked_add(len).ok_or("event length overflow")?;
    let value = bytes.get(*position..end).ok_or("truncated event payload")?;
    *position = end;
    Ok(value)
}

fn track_error(track: usize, detail: impl Into<String>) -> SmfIoError {
    SmfIoError::Track {
        track,
        detail: detail.into(),
    }
}

fn dump_line(timed: TimedMessage) -> String {
    let group = timed
        .message
        .group()
        .map_or_else(|| "-".to_owned(), |value| value.to_string());
    let channel = match timed.message {
        Message::Midi1Cv(message) => message.channel().to_string(),
        Message::Midi2Cv(message) => message.channel().to_string(),
        _ => "-".to_owned(),
    };
    format!(
        "tick={} group={} channel={} {}",
        timed.tick,
        group,
        channel,
        describe_message(timed.message)
    )
}

fn describe_message(message: Message) -> String {
    match message {
        Message::Midi2Cv(message) => describe_cv2(message),
        Message::Midi1Cv(message) => describe_cv1(message),
        Message::System(message) => describe_system(message),
        Message::SysEx7(message) => describe_sysex7(message),
        Message::Utility(message) => match message {
            Utility::NoOp(value) => format!("utility-no-op value={}", value.value()),
            Utility::JrClock(value) => format!("jr-clock value={}", value.value()),
            Utility::JrTimestamp(value) => format!("jr-timestamp value={}", value.value()),
            Utility::DeltaClockstampTpq(value) => format!("tpq value={}", value.value()),
            Utility::DeltaClockstamp(value) => format!("delta-clockstamp value={}", value.value()),
            Utility::Unknown(packet) => describe_unknown("utility-unknown", packet.words()),
        },
        Message::Data128(message) => match message {
            Data128::SysEx8Complete(_) => {
                describe_unknown("sysex8-complete", message.encode().words())
            }
            Data128::SysEx8Start(_) => describe_unknown("sysex8-start", message.encode().words()),
            Data128::SysEx8Continue(_) => {
                describe_unknown("sysex8-continue", message.encode().words())
            }
            Data128::SysEx8End(_) => describe_unknown("sysex8-end", message.encode().words()),
            Data128::MixedDataSetHeader(_) => {
                describe_unknown("mixed-data-set-header", message.encode().words())
            }
            Data128::MixedDataSetPayload(_) => {
                describe_unknown("mixed-data-set-payload", message.encode().words())
            }
            Data128::Unknown(packet) => describe_unknown("data128-unknown", packet.words()),
        },
        Message::Unknown(packet) => describe_unknown("unknown", packet.words()),
    }
}

fn describe_cv2(message: Midi2Cv) -> String {
    match message {
        Midi2Cv::NoteOff(note) => format!(
            "note-off note={} velocity={} velocity-pct={} attribute-type={} attribute-data={}",
            note.note(),
            note.velocity(),
            percent(u64::from(note.velocity()), u64::from(u16::MAX)),
            note.attribute_type(),
            note.attribute_data()
        ),
        Midi2Cv::NoteOn(note) => format!(
            "note-on note={} velocity={} velocity-pct={} attribute-type={} attribute-data={}",
            note.note(),
            note.velocity(),
            percent(u64::from(note.velocity()), u64::from(u16::MAX)),
            note.attribute_type(),
            note.attribute_data()
        ),
        Midi2Cv::PolyPressure(value) => format!(
            "poly-pressure note={} value={} value-pct={}",
            value.note(),
            value.data(),
            percent(u64::from(value.data()), u64::from(u32::MAX))
        ),
        Midi2Cv::ControlChange(value) => format!(
            "control-change controller={} value={} value-pct={}",
            value.controller(),
            value.data(),
            percent(u64::from(value.data()), u64::from(u32::MAX))
        ),
        Midi2Cv::RegisteredController(value) => format!(
            "registered-controller bank={} index={} value={} value-pct={}",
            value.bank(),
            value.index(),
            value.data(),
            percent(u64::from(value.data()), u64::from(u32::MAX))
        ),
        Midi2Cv::AssignableController(value) => format!(
            "assignable-controller bank={} index={} value={} value-pct={}",
            value.bank(),
            value.index(),
            value.data(),
            percent(u64::from(value.data()), u64::from(u32::MAX))
        ),
        Midi2Cv::RelativeRegisteredController(value) => format!(
            "relative-registered-controller bank={} index={} delta={}",
            value.bank(),
            value.index(),
            value.delta()
        ),
        Midi2Cv::RelativeAssignableController(value) => format!(
            "relative-assignable-controller bank={} index={} delta={}",
            value.bank(),
            value.index(),
            value.delta()
        ),
        Midi2Cv::RegisteredPerNoteController(value) => format!(
            "registered-per-note-controller note={} controller={} value={} value-pct={}",
            value.note(),
            value.controller(),
            value.data(),
            percent(u64::from(value.data()), u64::from(u32::MAX))
        ),
        Midi2Cv::AssignablePerNoteController(value) => format!(
            "assignable-per-note-controller note={} controller={} value={} value-pct={}",
            value.note(),
            value.controller(),
            value.data(),
            percent(u64::from(value.data()), u64::from(u32::MAX))
        ),
        Midi2Cv::PerNotePitchBend(value) => format!(
            "per-note-pitch-bend note={} value={} value-pct={}",
            value.note(),
            value.data(),
            percent(u64::from(value.data()), u64::from(u32::MAX))
        ),
        Midi2Cv::ProgramChange(value) => format!(
            "program-change program={} program-pct={} bank={}",
            value.program(),
            percent(u64::from(value.program()), 127),
            value
                .bank()
                .map_or_else(|| "-".to_owned(), |bank| bank.to_string())
        ),
        Midi2Cv::ChannelPressure(value) => format!(
            "channel-pressure value={} value-pct={}",
            value.data(),
            percent(u64::from(value.data()), u64::from(u32::MAX))
        ),
        Midi2Cv::PitchBend(value) => format!(
            "pitch-bend value={} value-pct={}",
            value.data(),
            percent(u64::from(value.data()), u64::from(u32::MAX))
        ),
        Midi2Cv::PerNoteManagement(value) => format!(
            "per-note-management note={} flags={}",
            value.note(),
            value.flags()
        ),
        Midi2Cv::Unknown(packet) => describe_unknown("midi2-cv-unknown", packet.words()),
    }
}

fn describe_cv1(message: Midi1Cv) -> String {
    match message {
        Midi1Cv::NoteOff(note) => format!(
            "midi1-note-off note={} velocity={} velocity-pct={}",
            note.note(),
            note.value(),
            percent(u64::from(note.value()), 127)
        ),
        Midi1Cv::NoteOn(note) => format!(
            "midi1-note-on note={} velocity={} velocity-pct={}",
            note.note(),
            note.value(),
            percent(u64::from(note.value()), 127)
        ),
        Midi1Cv::PolyPressure(note) => format!(
            "midi1-poly-pressure note={} value={} value-pct={}",
            note.note(),
            note.value(),
            percent(u64::from(note.value()), 127)
        ),
        Midi1Cv::ControlChange(value) => format!(
            "midi1-control-change controller={} value={} value-pct={}",
            value.controller(),
            value.value(),
            percent(u64::from(value.value()), 127)
        ),
        Midi1Cv::ProgramChange(value) => format!(
            "midi1-program-change program={} program-pct={}",
            value.program(),
            percent(u64::from(value.program()), 127)
        ),
        Midi1Cv::ChannelPressure(value) => format!(
            "midi1-channel-pressure value={} value-pct={}",
            value.pressure(),
            percent(u64::from(value.pressure()), 127)
        ),
        Midi1Cv::PitchBend(value) => format!(
            "midi1-pitch-bend value={} value-pct={}",
            value.value(),
            percent(u64::from(value.value()), 0x3FFF)
        ),
        Midi1Cv::Unknown(packet) => describe_unknown("midi1-cv-unknown", packet.words()),
    }
}

fn describe_system(message: System) -> String {
    match message {
        System::MtcQuarterFrame(value) => format!(
            "mtc-quarter-frame value={} value-pct={}",
            value.value(),
            percent(u64::from(value.value()), 127)
        ),
        System::SongPosition(value) => format!(
            "song-position value={} value-pct={}",
            value.value(),
            percent(u64::from(value.value()), 0x3FFF)
        ),
        System::SongSelect(value) => format!(
            "song-select value={} value-pct={}",
            value.value(),
            percent(u64::from(value.value()), 127)
        ),
        System::TuneRequest(_) => "tune-request".to_owned(),
        System::TimingClock(_) => "timing-clock".to_owned(),
        System::Start(_) => "start".to_owned(),
        System::Continue(_) => "continue".to_owned(),
        System::Stop(_) => "stop".to_owned(),
        System::ActiveSensing(_) => "active-sensing".to_owned(),
        System::Reset(_) => "reset".to_owned(),
        System::Unknown(packet) => describe_unknown("system-unknown", packet.words()),
    }
}

fn describe_sysex7(message: SysEx7) -> String {
    match message {
        SysEx7::Complete(value) => {
            format!("sysex7-complete bytes={}", hex_bytes(value.data()))
        }
        SysEx7::Start(value) => format!("sysex7-start bytes={}", hex_bytes(value.data())),
        SysEx7::Continue(value) => {
            format!("sysex7-continue bytes={}", hex_bytes(value.data()))
        }
        SysEx7::End(value) => format!("sysex7-end bytes={}", hex_bytes(value.data())),
        SysEx7::Unknown(packet) => describe_unknown("sysex7-unknown", packet.words()),
    }
}

fn describe_unknown(name: &str, words: &[u32]) -> String {
    let words = words
        .iter()
        .map(|word| format!("{word:08X}"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{name} words={words}")
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join("")
}

fn percent(value: u64, maximum: u64) -> String {
    format!("{:.2}%", value as f64 * 100.0 / maximum as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_clockstamps_are_chunked_to_twenty_bits() {
        let timeline = Timeline {
            ticks_per_quarter: 480,
            tempos: Vec::new(),
            events: vec![TimedMessage {
                tick: DELTA_CLOCKSTAMP_MAX * 2 + 7,
                message: Message::Midi2Cv(Midi2Cv::note_on(0, 0, 60, 0x8000, 0, 0)),
            }],
        };
        let file = to_ump_file(&timeline).unwrap();
        let decoded = messages(&file.words)
            .map(|message| message.unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(
            decoded.as_slice(),
            [
                Message::Utility(Utility::DeltaClockstampTpq(_)),
                Message::Utility(Utility::DeltaClockstamp(a)),
                Message::Utility(Utility::DeltaClockstamp(b)),
                Message::Utility(Utility::DeltaClockstamp(c)),
                Message::Midi2Cv(_)
            ] if a.value() == 0xFFFFF && b.value() == 0xFFFFF && c.value() == 7
        ));
        assert_eq!(from_ump_file(&file).unwrap(), timeline);
    }

    #[test]
    fn smpte_division_is_rejected_clearly() {
        let bytes = crate::smf::assemble_format0([0xE7, 40], &[0, 0xFF, 0x2F, 0]);
        assert!(matches!(
            import_smf(&bytes, None),
            Err(SmfIoError::SmpteDivision([0xE7, 40]))
        ));
    }

    #[test]
    fn tiny_dump_is_stable() {
        let timeline = Timeline {
            ticks_per_quarter: 480,
            tempos: Vec::new(),
            events: vec![
                TimedMessage {
                    tick: 0,
                    message: Message::Midi2Cv(Midi2Cv::note_on(2, 3, 60, 0x8000, 0, 0)),
                },
                TimedMessage {
                    tick: 240,
                    message: Message::Midi2Cv(Midi2Cv::control_change(2, 3, 7, 0xFFFF_FFFF)),
                },
            ],
        };
        assert_eq!(
            dump_lines(&timeline).collect::<Vec<_>>(),
            [
                "tick=0 group=2 channel=3 note-on note=60 velocity=32768 velocity-pct=50.00% attribute-type=0 attribute-data=0",
                "tick=240 group=2 channel=3 control-change controller=7 value=4294967295 value-pct=100.00%",
            ]
        );
    }

    #[test]
    fn format1_merge_order_and_track_selection_keep_global_tempo() {
        let conductor = [
            0, 0xFF, 0x51, 3, 0x07, 0xA1, 0x20, 0, 0x90, 60, 1, 0, 0xFF, 0x2F, 0,
        ];
        let part = [0, 0x91, 61, 2, 0, 0xFF, 0x2F, 0];
        let bytes = crate::smf::assemble([1, 0xE0], &[&conductor, &part]);

        let merged = import_smf(&bytes, None).unwrap();
        assert_eq!(
            merged.tempos,
            [Tempo {
                absolute_tick: 0,
                us_per_quarter: 500_000,
            }]
        );
        assert!(matches!(
            merged.events.as_slice(),
            [
                TimedMessage {
                    message: Message::Midi2Cv(first),
                    ..
                },
                TimedMessage {
                    message: Message::Midi2Cv(second),
                    ..
                }
            ] if first.channel() == 0 && second.channel() == 1
        ));

        let selected = import_smf(&bytes, Some(1)).unwrap();
        assert_eq!(selected.tempos, merged.tempos);
        assert_eq!(selected.events.len(), 1);
        assert!(matches!(
            selected.events[0].message,
            Message::Midi2Cv(message) if message.channel() == 1
        ));
    }

    #[test]
    fn format1_merge_rejects_track1_complete_inside_track0_open_sysex_run() {
        let track0 = [0, 0xF0, 1, 1, 10, 0xF7, 2, 2, 0xF7, 0, 0xFF, 0x2F, 0];
        let track1 = [5, 0xF0, 2, 3, 0xF7, 0, 0xFF, 0x2F, 0];
        let bytes = crate::smf::assemble([1, 0xE0], &[&track0, &track1]);
        assert!(matches!(
            import_smf(&bytes, None),
            Err(SmfIoError::MergedSysExInterleave {
                open_track: 0,
                open_tick: 0,
                interfering_track: 1,
                interfering_tick: 5,
            })
        ));
    }

    #[test]
    fn cc6_at_tick_10_stays_before_tick_100_note_and_end_flush_uses_source_tick() {
        let note_track = [
            0, 0xB0, 101, 1, 0, 0xB0, 100, 2, 10, 0xB0, 6, 3, 90, 0x90, 60, 1, 0, 0xFF, 0x2F, 0,
        ];
        let note = crate::smf::assemble_format0([1, 0xE0], &note_track);
        let timeline = import_smf(&note, None).unwrap();
        assert_eq!(
            timeline
                .events
                .iter()
                .map(|event| event.tick)
                .collect::<Vec<_>>(),
            [10, 100]
        );
        let report = roundtrip_smf(&note, None).unwrap();
        assert_eq!(report.first_divergence(), None);
        assert_eq!(report.expected.events[0].tick, 10);
        assert_eq!(report.actual.events[0].tick, 10);

        let completed_track = [
            0, 0xB0, 101, 1, 0, 0xB0, 100, 2, 10, 0xB0, 6, 3, 5, 0xF7, 1, 0xF8, 5, 0xB0, 38, 4, 0,
            0xFF, 0x2F, 0,
        ];
        let completed = crate::smf::assemble_format0([1, 0xE0], &completed_track);
        let timeline = import_smf(&completed, None).unwrap();
        assert_eq!(
            timeline
                .events
                .iter()
                .map(|event| event.tick)
                .collect::<Vec<_>>(),
            [10, 15]
        );
        assert_eq!(
            roundtrip_smf(&completed, None).unwrap().first_divergence(),
            None
        );

        let flushed_track = [
            0, 0xB0, 101, 1, 0, 0xB0, 100, 2, 10, 0xB0, 6, 3, 20, 0xFF, 0x2F, 0,
        ];
        let flushed = crate::smf::assemble_format0([1, 0xE0], &flushed_track);
        let timeline = import_smf(&flushed, None).unwrap();
        assert_eq!(timeline.events[0].tick, 10);
        assert_eq!(
            roundtrip_smf(&flushed, None).unwrap().first_divergence(),
            None
        );
    }

    #[test]
    fn crafted_cosump_huge_absolute_tick_is_rejected_before_filler_expansion() {
        let file = UmpFile {
            tempos: vec![Tempo {
                absolute_tick: u64::MAX,
                us_per_quarter: 500_000,
            }],
            words: Utility::delta_clockstamp_tpq(480).encode().words().to_vec(),
        };
        let crafted = super::super::umpfile::write(&file).unwrap();
        let decoded = super::super::umpfile::read(&crafted).unwrap();
        let timeline = from_ump_file(&decoded).unwrap();
        assert!(matches!(
            export_smf(&timeline),
            Err(SmfIoError::ExportExpansion {
                absolute_tick: u64::MAX,
                ..
            })
        ));
    }

    #[test]
    fn malformed_smf_sysex_transitions_are_rejected() {
        let open_at_eot =
            crate::smf::assemble_format0([1, 0xE0], &[0, 0xF0, 1, 1, 0, 0xFF, 0x2F, 0]);
        assert!(matches!(
            import_smf(&open_at_eot, None),
            Err(SmfIoError::Track { detail, .. }) if detail.contains("End Of Track")
        ));

        let standalone_end =
            crate::smf::assemble_format0([1, 0xE0], &[0, 0xF7, 1, 0xF7, 0, 0xFF, 0x2F, 0]);
        assert!(matches!(
            import_smf(&standalone_end, None),
            Err(SmfIoError::Track { detail, .. }) if detail.contains("without an open SysEx")
        ));

        let nested_start = crate::smf::assemble_format0(
            [1, 0xE0],
            &[0, 0xF0, 1, 1, 0, 0xF0, 1, 2, 0, 0xFF, 0x2F, 0],
        );
        assert!(matches!(
            import_smf(&nested_start, None),
            Err(SmfIoError::Track { detail, .. }) if detail.contains("already open")
        ));
    }

    #[test]
    fn forty_byte_cosump_standalone_sysex7_end_and_other_bad_topologies_are_rejected() {
        let tpq = Utility::delta_clockstamp_tpq(480).encode();
        let end_message = SysEx7::new(0, super::super::msg::SysEx7Format::End, &[1]).unwrap();
        let end = end_message.encode();
        let mut words = tpq.words().to_vec();
        words.extend_from_slice(end.words());
        let file = UmpFile {
            tempos: Vec::new(),
            words,
        };
        assert!(matches!(
            super::super::umpfile::write(&file),
            Err(UmpFileError::InvalidSysEx7Topology {
                message_index: 1,
                group: 0,
                detail
            })
                if detail.contains("standalone")
        ));
        let direct = Timeline {
            ticks_per_quarter: 480,
            tempos: Vec::new(),
            events: vec![TimedMessage {
                tick: 0,
                message: Message::SysEx7(end_message),
            }],
        };
        assert!(matches!(
            to_ump_file(&direct),
            Err(SmfIoError::SysEx7Topology { detail, .. }) if detail.contains("standalone")
        ));
        assert!(matches!(
            export_smf(&direct),
            Err(SmfIoError::SysEx7Topology { detail, .. }) if detail.contains("standalone")
        ));

        let start = SysEx7::new(0, super::super::msg::SysEx7Format::Start, &[1])
            .unwrap()
            .encode();
        let mut nested_words = tpq.words().to_vec();
        nested_words.extend_from_slice(start.words());
        nested_words.extend_from_slice(start.words());
        assert!(matches!(
            from_ump_file(&UmpFile {
                tempos: Vec::new(),
                words: nested_words,
            }),
            Err(SmfIoError::UmpFile(
                UmpFileError::InvalidSysEx7Topology { detail, .. }
            )) if detail.contains("nested")
        ));

        let mut open_words = tpq.words().to_vec();
        open_words.extend_from_slice(start.words());
        assert!(matches!(
            from_ump_file(&UmpFile {
                tempos: Vec::new(),
                words: open_words,
            }),
            Err(SmfIoError::UmpFile(
                UmpFileError::InvalidSysEx7Topology { detail, .. }
            ))
                if detail.contains("unterminated")
        ));
    }

    #[test]
    fn per_group_sysex_interleave_is_accepted_end_to_end() {
        let start =
            Message::SysEx7(SysEx7::new(0, super::super::msg::SysEx7Format::Start, &[1]).unwrap());
        let other_group = Message::SysEx7(
            SysEx7::new(1, super::super::msg::SysEx7Format::Complete, &[2]).unwrap(),
        );
        let end =
            Message::SysEx7(SysEx7::new(0, super::super::msg::SysEx7Format::End, &[3]).unwrap());
        let timeline = Timeline {
            ticks_per_quarter: 480,
            tempos: Vec::new(),
            events: vec![
                TimedMessage {
                    tick: 0,
                    message: start,
                },
                TimedMessage {
                    tick: 0,
                    message: other_group,
                },
                TimedMessage {
                    tick: 10,
                    message: end,
                },
            ],
        };

        let file = to_ump_file(&timeline).unwrap();
        assert_eq!(from_ump_file(&file).unwrap(), timeline);

        let exported = export_smf(&timeline).unwrap();
        assert_eq!(exported.dropped.group_routing, 1);
        let reimported = import_smf(&exported.bytes, None).unwrap();
        assert_eq!(reimported.events.len(), 2);
        assert!(matches!(
            reimported.events.as_slice(),
            [
                TimedMessage {
                    tick: 0,
                    message: Message::SysEx7(SysEx7::Complete(first)),
                },
                TimedMessage {
                    tick: 0,
                    message: Message::SysEx7(SysEx7::Complete(second)),
                }
            ] if first.data() == [1, 3] && second.data() == [2]
        ));
    }

    #[test]
    fn groupless_utility_and_same_group_realtime_are_legal_inside_sysex() {
        let timeline = Timeline {
            ticks_per_quarter: 480,
            tempos: Vec::new(),
            events: vec![
                TimedMessage {
                    tick: 0,
                    message: Message::SysEx7(
                        SysEx7::new(2, super::super::msg::SysEx7Format::Start, &[1]).unwrap(),
                    ),
                },
                TimedMessage {
                    tick: 1,
                    message: Message::Utility(Utility::jr_clock(7)),
                },
                TimedMessage {
                    tick: 2,
                    message: Message::System(System::timing_clock(2)),
                },
                TimedMessage {
                    tick: 3,
                    message: Message::SysEx7(
                        SysEx7::new(3, super::super::msg::SysEx7Format::Complete, &[9]).unwrap(),
                    ),
                },
                TimedMessage {
                    tick: 4,
                    message: Message::SysEx7(
                        SysEx7::new(2, super::super::msg::SysEx7Format::End, &[2]).unwrap(),
                    ),
                },
            ],
        };

        let file = to_ump_file(&timeline).unwrap();
        assert_eq!(from_ump_file(&file).unwrap(), timeline);
        let exported = export_smf(&timeline).unwrap();
        assert!(import_smf(&exported.bytes, None).is_ok());
    }

    #[test]
    fn same_group_dropped_message_interrupting_sysex_is_rejected_before_translation() {
        let timeline = Timeline {
            ticks_per_quarter: 480,
            tempos: Vec::new(),
            events: vec![
                TimedMessage {
                    tick: 0,
                    message: Message::SysEx7(
                        SysEx7::new(0, super::super::msg::SysEx7Format::Start, &[1]).unwrap(),
                    ),
                },
                TimedMessage {
                    tick: 1,
                    message: Message::Midi2Cv(Midi2Cv::per_note_pitch_bend(0, 0, 60, 1)),
                },
                TimedMessage {
                    tick: 2,
                    message: Message::SysEx7(
                        SysEx7::new(0, super::super::msg::SysEx7Format::End, &[2]).unwrap(),
                    ),
                },
            ],
        };

        assert!(matches!(
            export_smf(&timeline),
            Err(SmfIoError::SysEx7Topology {
                tick: 1,
                group: 0,
                detail,
            }) if detail.contains("same-group")
        ));
        assert!(matches!(
            to_ump_file(&timeline),
            Err(SmfIoError::SysEx7Topology {
                tick: 1,
                group: 0,
                ..
            })
        ));
    }

    #[test]
    fn equal_tick_tempo_after_other_track_sysex_start_is_rejected_deterministically() {
        let track0 = [0, 0xF0, 1, 1, 10, 0xF7, 2, 2, 0xF7, 0, 0xFF, 0x2F, 0];
        let track1 = [0, 0xFF, 0x51, 3, 0x07, 0xA1, 0x20, 0, 0xFF, 0x2F, 0];
        let bytes = crate::smf::assemble([1, 0xE0], &[&track0, &track1]);

        assert!(matches!(
            import_smf(&bytes, None),
            Err(SmfIoError::MergedSysExInterleave {
                open_track: 0,
                open_tick: 0,
                interfering_track: 1,
                interfering_tick: 0,
            })
        ));
    }

    #[test]
    fn nonzero_ump_group_projects_to_zero_with_one_reported_loss() {
        let timeline = Timeline {
            ticks_per_quarter: 480,
            tempos: Vec::new(),
            events: vec![TimedMessage {
                tick: 0,
                message: Message::Midi2Cv(Midi2Cv::note_on(1, 2, 60, 0x8000, 0, 0)),
            }],
        };
        assert!(dump_lines(&timeline).next().unwrap().contains("group=1"));
        let exported = export_smf(&timeline).unwrap();
        assert_eq!(exported.dropped.group_routing, 1);
        assert_eq!(
            dropped_lines(exported.dropped),
            ["dropped group-routing: 1"]
        );
        let reimported = import_smf(&exported.bytes, None).unwrap();
        assert_eq!(reimported.events[0].message.group(), Some(0));
    }

    #[test]
    fn roundtrip_report_detects_an_independent_timing_mismatch() {
        let event = Event::Midi1Cv(Midi1Cv::note_on(0, 0, 60, 1));
        let report = RoundtripReport {
            expected: Midi1Timeline {
                ticks_per_quarter: 480,
                tempos: Vec::new(),
                events: vec![TimedEvent { tick: 10, event }],
            },
            actual: Midi1Timeline {
                ticks_per_quarter: 480,
                tempos: Vec::new(),
                events: vec![TimedEvent { tick: 11, event }],
            },
            dropped: Dropped::default(),
        };
        assert_eq!(
            report.first_divergence(),
            Some(format!(
                "event #0: expected {:?}, got {:?}",
                report.expected.events.first(),
                report.actual.events.first()
            ))
        );
    }

    #[test]
    fn tick_spanning_sysex_roundtrip_uses_export_projection_and_counts_loss() {
        let track = [0, 0xF0, 1, 1, 10, 0xF7, 2, 2, 0xF7, 0, 0xFF, 0x2F, 0];
        let bytes = crate::smf::assemble_format0([1, 0xE0], &track);
        let report = roundtrip_smf(&bytes, None).unwrap();

        assert_eq!(report.first_divergence(), None);
        assert_eq!(report.dropped.sysex_timing, 1);
        assert!(matches!(
            report.actual.events.as_slice(),
            [TimedEvent {
                tick: 0,
                event: Event::SysEx(SysEx::Complete(data)),
            }] if data.data() == [1, 2]
        ));
        assert_eq!(report.expected, report.actual);
    }

    #[test]
    fn u64_max_timeline_tick_is_rejected_before_delta_clockstamp_expansion() {
        let timeline = Timeline {
            ticks_per_quarter: 480,
            tempos: Vec::new(),
            events: vec![TimedMessage {
                tick: u64::MAX,
                message: Message::Midi2Cv(Midi2Cv::note_on(0, 0, 60, 0x8000, 0, 0)),
            }],
        };

        assert!(matches!(
            to_ump_file(&timeline),
            Err(SmfIoError::TimelineExpansion {
                absolute_tick: u64::MAX,
                timing_events,
            }) if timing_events > MAX_TIMING_EXPANSION_EVENTS
        ));
    }

    #[test]
    fn same_tick_realtime_inside_sysex_reports_atomic_reordering_loss() {
        let track = [
            0, 0xF0, 1, 1, // Start.
            0, 0xF7, 1, 0xF8, // Escaped Timing Clock inside the open run.
            0, 0xF7, 2, 2, 0xF7, // End.
            0, 0xFF, 0x2F, 0,
        ];
        let bytes = crate::smf::assemble_format0([1, 0xE0], &track);
        let report = roundtrip_smf(&bytes, None).unwrap();

        assert_eq!(report.first_divergence(), None);
        assert_eq!(report.dropped.sysex_timing, 1);
        assert!(matches!(
            report.actual.events.as_slice(),
            [
                TimedEvent {
                    tick: 0,
                    event: Event::SysEx(SysEx::Complete(data)),
                },
                TimedEvent {
                    tick: 0,
                    event: Event::System(System::TimingClock(_)),
                },
            ] if data.data() == [1, 2]
        ));
        assert_eq!(report.expected, report.actual);
    }
}
