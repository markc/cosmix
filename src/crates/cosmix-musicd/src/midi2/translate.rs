//! Allocation-free translator output and pending-state summaries.
//!
//! A single input can expand to at most four outputs: the maximum is a
//! down-translated Registered/Assignable Controller, which becomes selector
//! MSB, selector LSB, Data Entry MSB and Data Entry LSB. Both translation
//! directions use the same concrete return shape.

/// Output from one translator call.
///
/// The backing array avoids a `smallvec` dependency and makes the four-item
/// expansion bound explicit. Consuming the value yields only populated slots
/// in insertion order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Translation<T> {
    items: [Option<T>; 4],
    len: u8,
}

impl<T> Translation<T> {
    /// Empty output.
    pub fn new() -> Self {
        Self {
            items: std::array::from_fn(|_| None),
            len: 0,
        }
    }

    /// One output item.
    pub fn one(item: T) -> Self {
        let mut output = Self::new();
        output.push(item);
        output
    }

    /// Number of populated output slots.
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether no output was produced.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Borrow one output by position.
    pub fn get(&self, index: usize) -> Option<&T> {
        if index < self.len() {
            self.items[index].as_ref()
        } else {
            None
        }
    }

    pub(crate) fn push(&mut self, item: T) {
        let index = self.len();
        assert!(
            index < self.items.len(),
            "translation output exceeds four items"
        );
        self.items[index] = Some(item);
        self.len += 1;
    }
}

impl<T> Default for Translation<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> IntoIterator for Translation<T> {
    type Item = T;
    type IntoIter = std::iter::Flatten<std::array::IntoIter<Option<T>, 4>>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.into_iter().flatten()
    }
}

/// Counts of MIDI 1.0 state retained by the up-translator.
///
/// Completed parameter selections are included because they affect future
/// Data Entry messages even though they do not themselves require output.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Pending {
    parameter_selections: u16,
    data_entries: u16,
    bank_selects: u16,
}

impl Pending {
    pub(crate) const fn new(
        parameter_selections: u16,
        data_entries: u16,
        bank_selects: u16,
    ) -> Self {
        Self {
            parameter_selections,
            data_entries,
            bank_selects,
        }
    }

    /// Channels holding a complete or partial RPN/NRPN selection.
    pub const fn parameter_selections(self) -> u16 {
        self.parameter_selections
    }

    /// Channels holding a Data Entry MSB during one-message lookahead.
    pub const fn data_entries(self) -> u16 {
        self.data_entries
    }

    /// Channels holding one or both Bank Select halves.
    pub const fn bank_selects(self) -> u16 {
        self.bank_selects
    }

    /// Whether no state affects future translation.
    pub const fn is_empty(self) -> bool {
        self.parameter_selections == 0 && self.data_entries == 0 && self.bank_selects == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translation_iterates_only_populated_slots_in_order() {
        let mut output = Translation::new();
        output.push(2);
        output.push(4);
        output.push(6);
        output.push(8);
        assert_eq!(output.len(), 4);
        assert_eq!(output.get(2), Some(&6));
        assert_eq!(output.get(4), None);
        assert_eq!(output.into_iter().collect::<Vec<_>>(), [2, 4, 6, 8]);
    }

    #[test]
    fn pending_summary_accessors() {
        let pending = Pending::new(2, 1, 3);
        assert_eq!(pending.parameter_selections(), 2);
        assert_eq!(pending.data_entries(), 1);
        assert_eq!(pending.bank_selects(), 3);
        assert!(!pending.is_empty());
        assert!(Pending::default().is_empty());
    }
}
