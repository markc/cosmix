//! Raw Universal MIDI Packet layer: packets as 1–4 `u32` words.
//!
//! A UMP's Message Type (MT, the top nibble of word 0) fully determines its
//! word count — that is what keeps a word stream self-synchronizing even
//! through unknown message types. This layer knows only MT, the raw routing
//! nibble, and word count; typed decoding lives in `msg`.

/// Words per packet for each of the 16 Message Types, indexed by MT.
///
/// MT 0x0 Utility, 0x1 System, 0x2 MIDI 1.0 CV → 1 word; 0x3 Data64/SysEx7,
/// 0x4 MIDI 2.0 CV → 2; 0x5 Data128 → 4; reserved MTs per spec: 0x6/0x7 → 1,
/// 0x8–0xA → 2, 0xB/0xC → 3, 0xE → 4; 0xD Flex Data, 0xF UMP Stream → 4.
/// Verified against ni-midi2 `universal_packet.h` (`size_lookup`) and the
/// bl-midi2-rs packet model.
pub const MT_WORDS: [usize; 16] = [1, 1, 1, 2, 2, 4, 1, 1, 2, 2, 2, 3, 3, 4, 4, 4];

/// Words in a packet whose word 0 carries Message Type `mt` (low nibble used).
pub const fn mt_words(mt: u8) -> usize {
    MT_WORDS[(mt & 0xF) as usize]
}

/// One Universal MIDI Packet: 1–4 words, length fixed by the MT nibble.
///
/// Plain data (`Copy`, `Eq`); construction guarantees `len == mt_words(mt)`,
/// so every `Ump` is self-consistent by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ump {
    words: [u32; 4],
    len: u8,
}

impl Ump {
    /// Build a packet from exactly the words its MT calls for; `None` if
    /// `words.len()` disagrees with the MT nibble of `words[0]` (or is empty).
    pub fn from_words(words: &[u32]) -> Option<Ump> {
        let (&w0, _) = words.split_first()?;
        let need = mt_words((w0 >> 28) as u8);
        if words.len() != need {
            return None;
        }
        let mut w = [0u32; 4];
        w[..need].copy_from_slice(words);
        // need is 1..=4 from the table.
        Some(Ump {
            words: w,
            len: need as u8,
        })
    }

    /// The packet's words (1–4 of them).
    pub fn words(&self) -> &[u32] {
        &self.words[..self.len as usize]
    }

    /// Word 0 (always present).
    pub const fn word0(&self) -> u32 {
        self.words[0]
    }

    /// Message Type — the top nibble of word 0.
    pub const fn mt(&self) -> u8 {
        (self.words[0] >> 28) as u8
    }

    /// Raw routing nibble — bits 27..24 of word 0.
    ///
    /// This is deliberately not called `group`: Utility and UMP Stream
    /// packets are groupless. Typed messages expose a semantic
    /// `Option<u8>` group where the family actually defines one.
    pub const fn routing_nibble(&self) -> u8 {
        ((self.words[0] >> 24) & 0xF) as u8
    }

    /// Set the raw routing nibble (the low nibble of `routing` is used).
    pub const fn set_routing_nibble(&mut self, routing: u8) {
        self.words[0] = (self.words[0] & 0xF0FF_FFFF) | (((routing & 0xF) as u32) << 24);
    }
}

/// A word stream ended mid-packet.
///
/// Word 0 announced `expected` words but only `actual` remained. Word 0 is
/// retained as a diagnostic without changing the stable expected/actual
/// contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NeedMoreWords {
    pub word0: u32,
    pub expected: usize,
    pub actual: usize,
}

/// Iterate the packets of a `&[u32]` word stream. Total: every word is
/// consumed; an unknown MT still advances by its table length, and the only
/// possible error is a truncated final packet.
pub fn packets(words: &[u32]) -> Packets<'_> {
    Packets { words }
}

/// Iterator returned by [`packets`].
#[derive(Debug, Clone)]
pub struct Packets<'a> {
    words: &'a [u32],
}

impl<'a> Iterator for Packets<'a> {
    type Item = Result<Ump, NeedMoreWords>;

    fn next(&mut self) -> Option<Self::Item> {
        let (&w0, _) = self.words.split_first()?;
        let need = mt_words((w0 >> 28) as u8);
        if self.words.len() < need {
            let have = self.words.len();
            self.words = &[];
            return Some(Err(NeedMoreWords {
                word0: w0,
                expected: need,
                actual: have,
            }));
        }
        let (head, rest) = self.words.split_at(need);
        self.words = rest;
        // Length matches MT by construction here.
        Some(Ok(Ump::from_words(head).expect("length == mt_words")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The MT→length table, pinned value by value against ni-midi2's
    /// `size_lookup` (universal_packet.h) — exhaustive over all 16 MTs.
    #[test]
    fn mt_length_table() {
        let want = [
            (0x0, 1), // Utility
            (0x1, 1), // System Common / Real Time
            (0x2, 1), // MIDI 1.0 Channel Voice
            (0x3, 2), // Data 64 / SysEx7
            (0x4, 2), // MIDI 2.0 Channel Voice
            (0x5, 4), // Data 128 / SysEx8 / Mixed Data Set
            (0x6, 1), // reserved
            (0x7, 1), // reserved
            (0x8, 2), // reserved
            (0x9, 2), // reserved
            (0xA, 2), // reserved
            (0xB, 3), // reserved
            (0xC, 3), // reserved
            (0xD, 4), // Flex Data
            (0xE, 4), // reserved
            (0xF, 4), // UMP Stream
        ];
        for (mt, n) in want {
            assert_eq!(mt_words(mt), n, "MT {mt:#x}");
        }
    }

    #[test]
    fn from_words_enforces_length() {
        // MT 0x4 (2 words): right length works, wrong lengths don't.
        assert!(Ump::from_words(&[0x4090_0000, 0]).is_some());
        assert!(Ump::from_words(&[0x4090_0000]).is_none());
        assert!(Ump::from_words(&[0x4090_0000, 0, 0]).is_none());
        assert!(Ump::from_words(&[]).is_none());
    }

    #[test]
    fn routing_nibble_get_set_across_all_mts() {
        for mt in 0u8..16 {
            let w0 = (mt as u32) << 28;
            let n = mt_words(mt);
            let mut words = [0u32; 4];
            words[0] = w0;
            let mut p = Ump::from_words(&words[..n]).unwrap();
            assert_eq!(p.routing_nibble(), 0, "MT {mt:#x}");
            p.set_routing_nibble(0xA);
            assert_eq!(p.routing_nibble(), 0xA, "MT {mt:#x}");
            assert_eq!(p.mt(), mt, "set_routing_nibble must not disturb MT");
            p.set_routing_nibble(0x5);
            assert_eq!(p.routing_nibble(), 0x5);
            // Only the routing nibble of word 0 changed.
            assert_eq!(p.word0() & 0xF0FF_FFFF, w0);
        }
    }

    #[test]
    fn stream_iteration_and_resync() {
        // MT2 (1 word), MT4 (2 words), unknown MT9 (2 words), MT0 (1 word).
        let stream = [
            0x2090_3C40u32,
            0x4090_3C00,
            0xABCD_0001,
            0x9123_4567,
            0xDEAD_BEEF,
            0x0000_0000,
        ];
        let got: Vec<_> = packets(&stream).collect();
        assert_eq!(got.len(), 4);
        let lens: Vec<_> = got
            .iter()
            .map(|r| r.as_ref().unwrap().words().len())
            .collect();
        assert_eq!(lens, [1, 2, 2, 1]);
        // Unknown MT 0x9 consumed exactly its table length, keeping the
        // stream in sync for the MT0 packet after it.
        assert_eq!(got[3].as_ref().unwrap().word0(), 0);
    }

    #[test]
    fn truncated_tail_reported() {
        let stream = [0x2090_3C40u32, 0x4090_3C00]; // MT4 wants 2 words, has 1
        let got: Vec<_> = packets(&stream).collect();
        assert_eq!(got.len(), 2);
        assert!(got[0].is_ok());
        assert_eq!(
            got[1],
            Err(NeedMoreWords {
                word0: 0x4090_3C00,
                expected: 2,
                actual: 1,
            })
        );
    }
}
