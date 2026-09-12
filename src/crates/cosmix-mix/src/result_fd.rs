//! `--result-fd N` — the structured-value channel for an isolated task.
//!
//! An isolated task's whole purpose is exact capture, and the one thing pipes
//! cannot separate is "the program's text" from "the evaluation's value". Text
//! goes to stdout and stderr as it always has; the value travels its own
//! inherited descriptor, so a task that prints and a task that returns are not
//! competing for the same stream.
//!
//! The frame is a big-endian u32 length followed by exactly that many bytes,
//! matching the sealed-memfd idiom the S3 bootstrap already uses. A reader can
//! therefore tell three things apart that an unframed stream cannot:
//!
//! * nothing written at all — the process died before producing a value
//!   (`result_missing`),
//! * a length that the payload does not satisfy — the writer was killed
//!   mid-frame (`result_torn`),
//! * a complete frame carrying a `truncated` marker — the value was larger
//!   than the cap and says so.
//!
//! Without the prefix the first two are indistinguishable from each other and
//! from an empty result, which is precisely the ambiguity a task surface must
//! not have.

use cosmix_mix::value::Value;
use std::io::Write;
use std::os::fd::{FromRawFd, RawFd};

/// Matches the supervisor's per-stream capture cap. A value larger than this is
/// reported as truncated rather than split: chunked delivery is a named
/// deferral, and silently sending half a value would be worse than either.
pub const MAX_RESULT: usize = 64 * 1024;
/// How much of an over-long error message survives in the reference frame.
/// Small enough that the reference itself cannot approach the cap after
/// escaping, large enough to carry the sentence that names the failure.
const ERROR_HEAD: usize = 2048;

/// Validated at startup, before any user code runs. Holding the number rather
/// than the `File` keeps the descriptor un-owned until the moment of writing,
/// so an early exit path cannot close a descriptor the parent still expects to
/// see EOF on at process end.
#[derive(Clone, Copy, Debug)]
pub struct ResultFd(RawFd);

impl ResultFd {
    /// `N` must be a real, open descriptor above stderr.
    ///
    /// Above stderr because 0, 1 and 2 are the streams whose separation is the
    /// point; writing a frame into stdout would corrupt the very capture this
    /// exists to keep clean. Open because a task that was promised a structured
    /// result and silently produced none is the failure mode with no symptom —
    /// better to refuse at startup, loudly, than to run and report nothing.
    pub fn validate(raw: RawFd) -> Result<Self, String> {
        if raw <= 2 {
            return Err(format!(
                "--result-fd {raw}: must be above stderr (0, 1 and 2 are the \
                 text streams this channel exists to stay out of)"
            ));
        }
        // fstat rather than a write probe: a probe would put bytes on a
        // descriptor whose framing has not started yet.
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: fstat only reads; an invalid fd is reported as -1/EBADF.
        if unsafe { libc::fstat(raw, stat.as_mut_ptr()) } != 0 {
            return Err(format!(
                "--result-fd {raw}: not an open descriptor ({})",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self(raw))
    }

    /// Frame and write one result. Called exactly once, after evaluation.
    pub fn write(self, payload: &Payload) -> std::io::Result<()> {
        let encoded = payload.encode();
        let bytes = encoded.as_bytes();
        let mut frame = Vec::with_capacity(4 + bytes.len());
        frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        frame.extend_from_slice(bytes);
        // SAFETY: validated open above, and this is the sole owner from here.
        // Into a File so a partial write is retried rather than lost.
        let mut file = unsafe { std::fs::File::from_raw_fd(self.0) };
        let written = file.write_all(&frame);
        // Close deliberately: EOF on the read end is what tells the supervisor
        // the frame is complete, and it must not wait for process exit for it.
        drop(file);
        written
    }
}

/// What the task evaluated to, or why it did not.
pub enum Payload {
    Value(Value),
    Error(String),
}

impl Payload {
    /// Strict-data, through the interpreter's own serializer — the same
    /// encoding `data_parse` reads back, so a caller is not handed a bespoke
    /// format it has to learn.
    fn encode(&self) -> String {
        let frame = self.encode_body();
        if frame.len() <= MAX_RESULT {
            return frame;
        }
        // The inner cap bounds the VALUE's encoding; this bounds the FRAME.
        // Putting that encoding inside the outer map escapes it a second time,
        // so a value that passed the inner cap can still produce a frame larger
        // than the supervisor is willing to read — and a frame cut off by the
        // supervisor's cap decodes as "the writer was killed mid-write", which
        // is a lie about a process that exited cleanly.
        self.oversized(frame.len())
    }

    /// The truncated-reference frame, which is small by construction.
    ///
    /// An ERROR keeps as much of its message as still fits. A value's encoding
    /// is all-or-nothing — half a strict-data document is not a document — but
    /// an error message is prose, and the first sentence of "why it failed" is
    /// usually the whole answer. Dropping it entirely because it was slightly
    /// too long is the one case where truncating tells the caller MORE.
    fn oversized(&self, bytes: usize) -> String {
        let mut map = cosmix_mix::IndexMap::new();
        map.insert("ok".into(), Value::Bool(matches!(self, Self::Value(_))));
        map.insert("truncated".into(), Value::Bool(true));
        map.insert("bytes".into(), Value::String(bytes.to_string()));
        if let Self::Error(message) = self {
            let mut head = message.clone();
            let mut end = ERROR_HEAD.min(head.len());
            while !head.is_char_boundary(end) {
                end -= 1;
            }
            head.truncate(end);
            map.insert("error".into(), Value::String(head));
        }
        Value::Map(std::rc::Rc::new(map))
            .to_mix_data_string()
            .unwrap_or_default()
    }

    fn encode_body(&self) -> String {
        let mut map = cosmix_mix::IndexMap::new();
        match self {
            Self::Value(value) => {
                map.insert("ok".into(), Value::Bool(true));
                map.insert("type".into(), Value::String(value.type_name().into()));
                match value.to_mix_data_string() {
                    Ok(encoded) if encoded.len() <= MAX_RESULT => {
                        map.insert("value".into(), Value::String(encoded));
                    }
                    // Over the cap, or a value the serializer cannot represent
                    // (a function, a handle). Both are reported as a reference
                    // rather than silently dropped — the caller learns that a
                    // value existed and why it is not here.
                    Ok(encoded) => {
                        map.insert("truncated".into(), Value::Bool(true));
                        map.insert(
                            "bytes".into(),
                            Value::String(encoded.len().to_string()),
                        );
                    }
                    Err(error) => {
                        map.insert("unrepresentable".into(), Value::String(error.to_string()));
                    }
                }
            }
            Self::Error(message) => {
                map.insert("ok".into(), Value::Bool(false));
                let mut message = message.clone();
                if message.len() > MAX_RESULT {
                    let mut end = MAX_RESULT;
                    while !message.is_char_boundary(end) {
                        end -= 1;
                    }
                    message.truncate(end);
                }
                map.insert("error".into(), Value::String(message));
            }
        }
        Value::Map(std::rc::Rc::new(map))
            .to_mix_data_string()
            // The outer map is strings and booleans by construction, so this
            // cannot fail; if it ever did, an empty frame would be read as a
            // torn result, which is the safe direction.
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_descriptors_above_the_text_streams_are_accepted() {
        for raw in [0, 1, 2] {
            let error = ResultFd::validate(raw).unwrap_err();
            assert!(error.contains("above stderr"), "{error}");
        }
        // A descriptor that is not open is refused rather than silently
        // producing no result.
        let error = ResultFd::validate(9_999).unwrap_err();
        assert!(error.contains("not an open descriptor"), "{error}");
    }

    #[test]
    fn a_real_descriptor_validates_and_frames_length_first() {
        let mut fds = [0 as libc::c_int; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let result = ResultFd::validate(fds[1]).expect("an open pipe write end");
        result
            .write(&Payload::Value(Value::String("hello".into())))
            .unwrap();
        let mut buffer = Vec::new();
        // SAFETY: the read end, owned here for the length of the test.
        let mut read = unsafe { std::fs::File::from_raw_fd(fds[0]) };
        std::io::Read::read_to_end(&mut read, &mut buffer).unwrap();
        assert!(buffer.len() > 4);
        let declared = u32::from_be_bytes(buffer[..4].try_into().unwrap()) as usize;
        assert_eq!(
            declared,
            buffer.len() - 4,
            "the length prefix must describe exactly the payload that follows"
        );
        let payload = String::from_utf8(buffer[4..].to_vec()).unwrap();
        assert!(payload.contains("ok"), "{payload}");
        assert!(payload.contains("hello"), "{payload}");
    }

    #[test]
    fn an_oversized_value_is_reported_as_truncated_not_dropped() {
        let huge = Value::String("x".repeat(MAX_RESULT * 2));
        let encoded = Payload::Value(huge).encode();
        assert!(encoded.contains("truncated"), "{encoded}");
        assert!(encoded.contains("bytes"), "{encoded}");
        assert!(encoded.len() < MAX_RESULT, "the report itself must stay small");
    }

    #[test]
    fn a_failure_carries_its_message_bounded() {
        let encoded = Payload::Error("boom".repeat(MAX_RESULT)).encode();
        assert!(encoded.contains("ok"));
        assert!(encoded.len() <= MAX_RESULT + 256, "{}", encoded.len());
    }
}
