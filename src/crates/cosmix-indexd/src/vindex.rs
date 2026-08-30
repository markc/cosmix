//! In-memory exact vector index (2026-07-25 search-latency arc).
//!
//! One contiguous vector holding every corpus embedding, mirrored from
//! the sqlite-vec table (which stays the F32 source of truth). The mirror
//! is F32 by default, with an opt-in F16 representation to halve its RSS.
//! Exact
//! squared-L2 top-k over SQL-preselected candidate ids replaces the
//! sqlite-vec brute KNN that cost ~450ms per query on 56k×768 under
//! the global mutex — and, because eligibility filters run BEFORE
//! distance selection, it also fixes the filtered-search under-fill
//! defect (vec0 picked k nearest first, then JOIN predicates could
//! drop results below the requested limit).
//!
//! Consistency contract (enforced at the call sites in `VectorDb`):
//! writers acquire the `RwLock` write guard, commit the SQLite
//! mutation, patch this index, then release; search holds the read
//! guard across its own read-only SQLite transaction, so it sees
//! either fully-before or fully-after state, never a mix. The lock is
//! synchronous and must never be held across an `.await`.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VectorDtype {
    F32,
    F16,
}

impl VectorDtype {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "f32" => Ok(Self::F32),
            "f16" => Ok(Self::F16),
            other => Err(format!(
                "invalid vindex_dtype {other:?}; expected f32 or f16"
            )),
        }
    }
}

enum Embeddings {
    F32(Vec<f32>),
    F16(Vec<u16>),
}

pub struct VectorIndex {
    dim: usize,
    ids: Vec<i64>,
    /// `ids.len() * dim`, row-major, contiguous — one allocation, no
    /// per-vector boxing, cache-friendly scan order.
    embeddings: Embeddings,
    slot_by_id: HashMap<i64, usize>,
}

impl VectorIndex {
    pub fn new(dim: usize, dtype: VectorDtype) -> Self {
        Self::with_capacity(dim, 0, dtype)
    }

    /// Allocate the startup mirror once from the authoritative row count,
    /// avoiding Vec growth copies while the database scan fills it.
    pub fn with_capacity(dim: usize, rows: usize, dtype: VectorDtype) -> Self {
        Self {
            dim,
            ids: Vec::with_capacity(rows),
            embeddings: match dtype {
                VectorDtype::F32 => Embeddings::F32(Vec::with_capacity(rows.saturating_mul(dim))),
                VectorDtype::F16 => Embeddings::F16(Vec::with_capacity(rows.saturating_mul(dim))),
            },
            slot_by_id: HashMap::with_capacity(rows),
        }
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Insert or overwrite the embedding for `id`. Errors on a
    /// dimension mismatch — a wrong-width vector would corrupt the
    /// flat layout for every later slot.
    pub fn upsert(&mut self, id: i64, emb: &[f32]) -> Result<(), String> {
        if emb.len() != self.dim {
            return Err(format!(
                "embedding dim {} != index dim {} for id {id}",
                emb.len(),
                self.dim
            ));
        }
        match self.slot_by_id.get(&id) {
            Some(&slot) => {
                let base = slot * self.dim;
                match &mut self.embeddings {
                    Embeddings::F32(values) => {
                        values[base..base + self.dim].copy_from_slice(emb);
                    }
                    Embeddings::F16(values) => {
                        for (dst, src) in values[base..base + self.dim].iter_mut().zip(emb) {
                            *dst = f32_to_f16_bits(*src);
                        }
                    }
                }
            }
            None => {
                let slot = self.ids.len();
                self.ids.push(id);
                match &mut self.embeddings {
                    Embeddings::F32(values) => values.extend_from_slice(emb),
                    Embeddings::F16(values) => {
                        values.extend(emb.iter().map(|value| f32_to_f16_bits(*value)));
                    }
                }
                self.slot_by_id.insert(id, slot);
            }
        }
        Ok(())
    }

    /// Swap-remove `id`; the last slot's vector moves into the hole.
    /// Returns false if the id was absent.
    pub fn remove(&mut self, id: i64) -> bool {
        let Some(slot) = self.slot_by_id.remove(&id) else {
            return false;
        };
        let last = self.ids.len() - 1;
        if slot != last {
            let moved_id = self.ids[last];
            self.ids.swap(slot, last);
            match &mut self.embeddings {
                Embeddings::F32(values) => move_last_vector(values, self.dim, slot, last),
                Embeddings::F16(values) => move_last_vector(values, self.dim, slot, last),
            }
            self.slot_by_id.insert(moved_id, slot);
        }
        self.ids.pop();
        match &mut self.embeddings {
            Embeddings::F32(values) => values.truncate(last * self.dim),
            Embeddings::F16(values) => values.truncate(last * self.dim),
        }
        self.maybe_shrink();
        true
    }

    /// Mass deletes are uncommon, so retain strict swap-remove incrementality
    /// until live rows fall below half the allocated slots. Then shrink all
    /// three containers together; the 64-row floor avoids allocator churn for
    /// tiny indexes.
    fn maybe_shrink(&mut self) {
        const MIN_SHRINK_CAPACITY: usize = 64;
        let live = self.ids.len();
        let embedding_capacity = match &self.embeddings {
            Embeddings::F32(values) => values.capacity(),
            Embeddings::F16(values) => values.capacity(),
        };
        let row_capacity = self.ids.capacity().max(embedding_capacity / self.dim);
        if row_capacity >= MIN_SHRINK_CAPACITY && live.saturating_mul(2) < row_capacity {
            self.ids.shrink_to_fit();
            match &mut self.embeddings {
                Embeddings::F32(values) => values.shrink_to_fit(),
                Embeddings::F16(values) => values.shrink_to_fit(),
            }
            self.slot_by_id.shrink_to_fit();
        }
    }

    #[cfg(test)]
    fn capacities(&self) -> (usize, usize, usize) {
        let embeddings = match &self.embeddings {
            Embeddings::F32(values) => values.capacity(),
            Embeddings::F16(values) => values.capacity(),
        };
        (
            self.ids.capacity(),
            embeddings / self.dim,
            self.slot_by_id.capacity(),
        )
    }

    /// Exact top-`k` nearest (Euclidean) among `candidates`, returned
    /// as `(id, distance)` sorted ascending. Distances are sqrt'd so
    /// they match what sqlite-vec's `float[N]` L2 KNN reported —
    /// callers' ranking semantics are unchanged.
    ///
    /// A candidate id absent from the index is an INTEGRITY ERROR, not
    /// a skip: startup validates the exact chunk↔vector id sets and
    /// every mutation patches the mirror under the write gate while
    /// callers hold the read gate, so a hole here means the mirror and
    /// the database have diverged — silently skipping would break the
    /// `min(limit, eligible)` result guarantee while looking healthy.
    pub fn top_k(
        &self,
        query: &[f32],
        candidates: &[i64],
        k: usize,
    ) -> Result<Vec<(i64, f32)>, String> {
        if k == 0 || query.len() != self.dim {
            return Ok(Vec::new());
        }
        let mut scored: Vec<(f32, i64)> = Vec::with_capacity(candidates.len());
        for &id in candidates {
            let Some(&slot) = self.slot_by_id.get(&id) else {
                return Err(format!(
                    "candidate chunk {id} has no vector in the search index — \
                     mirror/database divergence"
                ));
            };
            let base = slot * self.dim;
            // Plain zip-fold over contiguous f32 — auto-vectorises in
            // release; measured well inside the latency budget without
            // hand-rolled SIMD or a thread pool.
            let d2: f32 = match &self.embeddings {
                Embeddings::F32(values) => values[base..base + self.dim]
                    .iter()
                    .zip(query)
                    .map(|(a, b)| {
                        let d = a - b;
                        d * d
                    })
                    .sum(),
                Embeddings::F16(values) => values[base..base + self.dim]
                    .iter()
                    .zip(query)
                    .map(|(a, b)| {
                        let d = f16_bits_to_f32(*a) - b;
                        d * d
                    })
                    .sum(),
            };
            scored.push((d2, id));
        }
        let k = k.min(scored.len());
        if k == 0 {
            return Ok(Vec::new());
        }
        scored.select_nth_unstable_by(k - 1, |a, b| {
            a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(k);
        scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        Ok(scored.into_iter().map(|(d2, id)| (id, d2.sqrt())).collect())
    }
}

fn move_last_vector<T: Copy>(values: &mut [T], dim: usize, slot: usize, last: usize) {
    let (head, tail) = values.split_at_mut(last * dim);
    head[slot * dim..(slot + 1) * dim].copy_from_slice(&tail[..dim]);
}

/// IEEE-754 round-to-nearest-even conversion. Embeddings are finite, but the
/// full conversion keeps the mirror well-defined if corrupt/non-finite input
/// reaches it before the SQLite layer rejects it.
fn f32_to_f16_bits(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x7f_ffff;

    if exponent == 0xff {
        return sign
            | if mantissa == 0 {
                0x7c00
            } else {
                0x7e00 | ((mantissa >> 13) as u16)
            };
    }

    let half_exp = exponent - 127 + 15;
    if half_exp >= 0x1f {
        return sign | 0x7c00;
    }
    if half_exp <= 0 {
        if half_exp < -10 {
            return sign;
        }
        let significand = mantissa | 0x80_0000;
        let shift = (14 - half_exp) as u32;
        let mut rounded = significand >> shift;
        let remainder = significand & ((1u32 << shift) - 1);
        let halfway = 1u32 << (shift - 1);
        if remainder > halfway || (remainder == halfway && rounded & 1 != 0) {
            rounded += 1;
        }
        return sign | rounded as u16;
    }

    let mut rounded_mantissa = mantissa + 0x0fff + ((mantissa >> 13) & 1);
    let mut rounded_exp = half_exp as u16;
    if rounded_mantissa & 0x80_0000 != 0 {
        rounded_mantissa = 0;
        rounded_exp += 1;
        if rounded_exp >= 0x1f {
            return sign | 0x7c00;
        }
    }
    sign | (rounded_exp << 10) | ((rounded_mantissa >> 13) as u16)
}

fn f16_bits_to_f32(bits: u16) -> f32 {
    let sign = (u32::from(bits & 0x8000)) << 16;
    let exponent = u32::from((bits >> 10) & 0x1f);
    let mantissa = u32::from(bits & 0x03ff);
    let out = match exponent {
        0 if mantissa == 0 => sign,
        0 => {
            let leading = mantissa.leading_zeros() - 22;
            let normalized = (mantissa << (leading + 1)) & 0x03ff;
            let exp = 127 - 15 - leading;
            sign | (exp << 23) | (normalized << 13)
        }
        0x1f => sign | 0x7f80_0000 | (mantissa << 13),
        _ => sign | ((exponent + 127 - 15) << 23) | (mantissa << 13),
    };
    f32::from_bits(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(dim: usize, seed: f32) -> Vec<f32> {
        (0..dim).map(|i| seed + i as f32 * 0.01).collect()
    }

    #[test]
    fn upsert_overwrite_and_len() {
        let mut idx = VectorIndex::new(4, VectorDtype::F32);
        idx.upsert(10, &[1.0, 0.0, 0.0, 0.0]).unwrap();
        idx.upsert(20, &[0.0, 1.0, 0.0, 0.0]).unwrap();
        assert_eq!(idx.len(), 2);
        // Overwrite keeps len and changes the vector.
        idx.upsert(10, &[0.0, 0.0, 1.0, 0.0]).unwrap();
        assert_eq!(idx.len(), 2);
        let top = idx.top_k(&[0.0, 0.0, 1.0, 0.0], &[10, 20], 1).unwrap();
        assert_eq!(top[0].0, 10);
        assert!(top[0].1 < 1e-6);
    }

    #[test]
    fn dim_mismatch_rejected() {
        let mut idx = VectorIndex::new(4, VectorDtype::F32);
        assert!(idx.upsert(1, &[1.0, 2.0]).is_err());
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn swap_remove_preserves_other_vectors() {
        let mut idx = VectorIndex::new(4, VectorDtype::F32);
        for (i, id) in [100i64, 200, 300, 400].iter().enumerate() {
            idx.upsert(*id, &v(4, i as f32)).unwrap();
        }
        assert!(idx.remove(200));
        assert!(!idx.remove(200));
        assert_eq!(idx.len(), 3);
        // Every survivor still ranks itself at distance 0.
        for (i, id) in [(0usize, 100i64), (2, 300), (3, 400)] {
            let q = v(4, i as f32);
            let top = idx.top_k(&q, &[100, 300, 400], 1).unwrap();
            assert_eq!(top[0].0, id, "seed {i}");
            assert!(top[0].1 < 1e-6);
        }
    }

    #[test]
    fn top_k_matches_naive_and_respects_candidates() {
        let dim = 16;
        let mut idx = VectorIndex::new(dim, VectorDtype::F32);
        let n = 200i64;
        for id in 0..n {
            idx.upsert(id, &v(dim, (id as f32) * 0.37)).unwrap();
        }
        let query = v(dim, 31.4);
        let candidates: Vec<i64> = (0..n).filter(|id| id % 3 != 0).collect();
        let got = idx.top_k(&query, &candidates, 5).unwrap();
        // Naive reference over the same candidate set.
        let mut naive: Vec<(f32, i64)> = candidates
            .iter()
            .map(|&id| {
                let e = v(dim, (id as f32) * 0.37);
                let d2: f32 = e
                    .iter()
                    .zip(query.iter())
                    .map(|(a, b)| (a - b) * (a - b))
                    .sum();
                (d2.sqrt(), id)
            })
            .collect();
        naive.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        assert_eq!(got.len(), 5);
        for (i, (id, dist)) in got.iter().enumerate() {
            assert_eq!(*id, naive[i].1, "rank {i}");
            assert!((dist - naive[i].0).abs() < 1e-4);
            assert!(id % 3 != 0, "excluded candidate leaked in");
        }
    }

    #[test]
    fn missing_candidate_is_integrity_error_and_k_clamped() {
        let mut idx = VectorIndex::new(4, VectorDtype::F32);
        idx.upsert(1, &[1.0, 0.0, 0.0, 0.0]).unwrap();
        // A candidate with no vector = mirror/database divergence.
        assert!(idx.top_k(&[1.0, 0.0, 0.0, 0.0], &[1, 999], 10).is_err());
        // k larger than the candidate set clamps cleanly.
        let got = idx.top_k(&[1.0, 0.0, 0.0, 0.0], &[1], 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 1);
    }

    #[test]
    fn exact_startup_reserve_and_mass_delete_shrink() {
        let mut idx = VectorIndex::with_capacity(4, 128, VectorDtype::F32);
        let initial = idx.capacities();
        assert!(initial.0 >= 128);
        assert!(initial.1 >= 128);
        assert!(initial.2 >= 128);
        for id in 0..128 {
            idx.upsert(id, &v(4, id as f32)).unwrap();
        }
        for id in 0..80 {
            assert!(idx.remove(id));
        }
        let shrunk = idx.capacities();
        assert!(
            shrunk.0 < initial.0,
            "ids did not shrink: {initial:?} -> {shrunk:?}"
        );
        assert!(
            shrunk.1 < initial.1,
            "embeddings did not shrink: {initial:?} -> {shrunk:?}"
        );
        assert!(
            shrunk.2 < initial.2,
            "map did not shrink: {initial:?} -> {shrunk:?}"
        );
        assert_eq!(idx.len(), 48);
    }

    #[test]
    fn f16_mirror_top_k_matches_f32_result_set() {
        let dim = 32;
        let mut f32_index = VectorIndex::new(dim, VectorDtype::F32);
        let mut f16_index = VectorIndex::new(dim, VectorDtype::F16);
        for id in 0..300i64 {
            let emb: Vec<f32> = (0..dim)
                .map(|i| ((id * 37 + i as i64 * 11) as f32 * 0.001).sin())
                .collect();
            f32_index.upsert(id, &emb).unwrap();
            f16_index.upsert(id, &emb).unwrap();
        }
        let query: Vec<f32> = (0..dim).map(|i| (i as f32 * 0.071).cos()).collect();
        let candidates: Vec<i64> = (0..300).filter(|id| id % 7 != 0).collect();
        let mut f32_ids: Vec<i64> = f32_index
            .top_k(&query, &candidates, 12)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let mut f16_ids: Vec<i64> = f16_index
            .top_k(&query, &candidates, 12)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        // F16 quantisation may perturb ordering inside the selected set, but
        // this separated synthetic corpus must select the same neighbours.
        f32_ids.sort_unstable();
        f16_ids.sort_unstable();
        assert_eq!(f16_ids, f32_ids);
    }
}
