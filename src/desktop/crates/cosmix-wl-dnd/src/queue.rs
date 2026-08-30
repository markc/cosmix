use std::collections::{BTreeMap, VecDeque};

use crate::types::{BridgeEvent, DataTransferId};

/// Default bridge queue sizing.
///
/// One transfer is active today, so 32 lifecycle records cover substantial
/// compositor churn without making overflow unobservable. Eight action keys
/// and eight motion keys leave room for later multi-seat work. Up to eight
/// motion records are drained per frame after terminals, lifecycle, and action
/// updates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueConfig {
    pub lifecycle_capacity: usize,
    pub action_capacity: usize,
    pub motion_capacity: usize,
    /// Maximum coalesced motions drained per frame.
    ///
    /// Must be non-zero: leaving every motion queued can prevent the consumer's
    /// delivered revision from ever covering a physical-drop fence.
    pub motion_drain_budget: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueConfigError {
    ZeroLifecycleCapacity,
    ZeroActionCapacity,
    ZeroMotionCapacity,
    ZeroMotionDrainBudget,
}

impl QueueConfig {
    pub fn validate(self) -> Result<Self, QueueConfigError> {
        if self.lifecycle_capacity == 0 {
            return Err(QueueConfigError::ZeroLifecycleCapacity);
        }
        if self.action_capacity == 0 {
            return Err(QueueConfigError::ZeroActionCapacity);
        }
        if self.motion_capacity == 0 {
            return Err(QueueConfigError::ZeroMotionCapacity);
        }
        if self.motion_drain_budget == 0 {
            return Err(QueueConfigError::ZeroMotionDrainBudget);
        }
        Ok(self)
    }
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            lifecycle_capacity: 32,
            action_capacity: 8,
            motion_capacity: 8,
            motion_drain_budget: 8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventClass<K> {
    /// Out-of-band per-transfer latch. The first terminal wins.
    Terminal(DataTransferId),
    /// Ordered, non-replaceable work.
    Lifecycle,
    /// Keep-latest action update for this key.
    Action(K),
    /// Keep-latest motion update for this key.
    Motion(K),
}

pub trait QueueEvent {
    type CoalesceKey: Copy + Ord;

    fn class(&self) -> EventClass<Self::CoalesceKey>;
    fn transfer_id(&self) -> Option<DataTransferId>;
}

impl QueueEvent for BridgeEvent {
    type CoalesceKey = (DataTransferId, u8);

    fn class(&self) -> EventClass<Self::CoalesceKey> {
        match self {
            Self::Terminal(event) => EventClass::Terminal(event.transfer_id),
            Self::Motion { transfer_id, .. } => EventClass::Motion((*transfer_id, 0)),
            Self::ActionChanged { transfer_id, .. } => EventClass::Action((*transfer_id, 0)),
            Self::SourceActionsChanged { transfer_id, .. } => EventClass::Action((*transfer_id, 1)),
            Self::Entered { .. } | Self::HoverLeft { .. } | Self::Drop(_) => EventClass::Lifecycle,
        }
    }

    fn transfer_id(&self) -> Option<DataTransferId> {
        Some(match self {
            Self::Entered { transfer_id, .. }
            | Self::Motion { transfer_id, .. }
            | Self::ActionChanged { transfer_id, .. }
            | Self::SourceActionsChanged { transfer_id, .. }
            | Self::HoverLeft { transfer_id, .. } => *transfer_id,
            Self::Drop(event) => event.transfer_id,
            Self::Terminal(event) => event.transfer_id,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum EnqueueError<E> {
    LifecycleFull(E),
    ActionFull(E),
    MotionFull(E),
}

impl<E> EnqueueError<E> {
    pub fn into_event(self) -> E {
        match self {
            Self::LifecycleFull(event) | Self::ActionFull(event) | Self::MotionFull(event) => event,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueStats {
    pub terminals: usize,
    pub lifecycle: usize,
    pub actions: usize,
    pub motions: usize,
}

/// A bounded classified queue with an unshared terminal latch per transfer.
///
/// Terminal latches are outside the ordinary capacities and are always
/// drained first. Lifecycle records preserve insertion order and are returned
/// to the caller on overflow, so the caller can fail the transfer closed
/// without silently losing the record.
pub struct BoundedEventQueue<E>
where
    E: QueueEvent,
{
    config: QueueConfig,
    terminals: BTreeMap<DataTransferId, E>,
    lifecycle: VecDeque<E>,
    actions: BTreeMap<E::CoalesceKey, E>,
    motions: BTreeMap<E::CoalesceKey, E>,
}

impl<E> BoundedEventQueue<E>
where
    E: QueueEvent,
{
    pub fn new(config: QueueConfig) -> Result<Self, QueueConfigError> {
        let config = config.validate()?;
        Ok(Self {
            config,
            terminals: BTreeMap::new(),
            lifecycle: VecDeque::with_capacity(config.lifecycle_capacity),
            actions: BTreeMap::new(),
            motions: BTreeMap::new(),
        })
    }

    pub fn enqueue(&mut self, event: E) -> Result<(), EnqueueError<E>> {
        match event.class() {
            EventClass::Terminal(transfer_id) => {
                self.terminals.entry(transfer_id).or_insert(event);
                Ok(())
            }
            EventClass::Lifecycle => {
                if self.lifecycle.len() == self.config.lifecycle_capacity {
                    return Err(EnqueueError::LifecycleFull(event));
                }
                self.lifecycle.push_back(event);
                Ok(())
            }
            EventClass::Action(key) => {
                if !self.actions.contains_key(&key)
                    && self.actions.len() == self.config.action_capacity
                {
                    return Err(EnqueueError::ActionFull(event));
                }
                self.actions.insert(key, event);
                Ok(())
            }
            EventClass::Motion(key) => {
                if !self.motions.contains_key(&key)
                    && self.motions.len() == self.config.motion_capacity
                {
                    return Err(EnqueueError::MotionFull(event));
                }
                self.motions.insert(key, event);
                Ok(())
            }
        }
    }

    /// Drains terminal latches, all lifecycle work, all current action updates,
    /// then at most the configured number of coalesced motions.
    pub fn drain_frame(&mut self) -> Vec<E> {
        let motion_drain_budget = self.config.motion_drain_budget;
        let mut drained = Vec::with_capacity(
            self.terminals.len()
                + self.lifecycle.len()
                + self.actions.len()
                + motion_drain_budget.min(self.motions.len()),
        );
        drained.extend(std::mem::take(&mut self.terminals).into_values());
        drained.extend(self.lifecycle.drain(..));
        drained.extend(std::mem::take(&mut self.actions).into_values());

        let keys = self
            .motions
            .keys()
            .copied()
            .take(motion_drain_budget)
            .collect::<Vec<_>>();
        for key in keys {
            if let Some(event) = self.motions.remove(&key) {
                drained.push(event);
            }
        }
        drained
    }

    pub fn stats(&self) -> QueueStats {
        QueueStats {
            terminals: self.terminals.len(),
            lifecycle: self.lifecycle.len(),
            actions: self.actions.len(),
            motions: self.motions.len(),
        }
    }

    pub(crate) fn lifecycle_contains(&self, predicate: impl FnMut(&E) -> bool) -> bool {
        self.lifecycle.iter().any(predicate)
    }

    pub(crate) fn remove_lifecycle(&mut self, mut predicate: impl FnMut(&E) -> bool) -> Option<E> {
        let index = self.lifecycle.iter().position(&mut predicate)?;
        self.lifecycle.remove(index)
    }

    pub(crate) fn discard_motions_for(&mut self, transfer_id: DataTransferId) {
        self.motions
            .retain(|_, event| event.transfer_id() != Some(transfer_id));
    }

    /// Removes non-terminal work for a transfer after its terminal transition.
    pub fn discard_ordinary_for(&mut self, transfer_id: DataTransferId) {
        self.lifecycle
            .retain(|event| event.transfer_id() != Some(transfer_id));
        self.actions
            .retain(|_, event| event.transfer_id() != Some(transfer_id));
        self.motions
            .retain(|_, event| event.transfer_id() != Some(transfer_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Position, TerminalDisposition, TerminalEvent, TerminalReason};

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum TestEvent {
        Terminal(DataTransferId, u8),
        Lifecycle(u8),
        Action(DataTransferId, u8),
        Motion(DataTransferId, u8),
    }

    impl QueueEvent for TestEvent {
        type CoalesceKey = DataTransferId;

        fn class(&self) -> EventClass<Self::CoalesceKey> {
            match self {
                Self::Terminal(id, _) => EventClass::Terminal(*id),
                Self::Lifecycle(_) => EventClass::Lifecycle,
                Self::Action(id, _) => EventClass::Action(*id),
                Self::Motion(id, _) => EventClass::Motion(*id),
            }
        }

        fn transfer_id(&self) -> Option<DataTransferId> {
            match self {
                Self::Terminal(id, _) | Self::Action(id, _) | Self::Motion(id, _) => Some(*id),
                Self::Lifecycle(_) => None,
            }
        }
    }

    fn config() -> QueueConfig {
        QueueConfig {
            lifecycle_capacity: 2,
            action_capacity: 2,
            motion_capacity: 2,
            motion_drain_budget: 1,
        }
    }

    #[test]
    fn replaceable_events_coalesce_keep_latest() {
        let id = DataTransferId(1);
        let mut queue = BoundedEventQueue::new(config()).unwrap();
        queue.enqueue(TestEvent::Motion(id, 1)).unwrap();
        queue.enqueue(TestEvent::Motion(id, 2)).unwrap();
        queue.enqueue(TestEvent::Action(id, 3)).unwrap();
        queue.enqueue(TestEvent::Action(id, 4)).unwrap();

        assert_eq!(
            queue.drain_frame(),
            vec![TestEvent::Action(id, 4), TestEvent::Motion(id, 2)]
        );
    }

    #[test]
    fn lifecycle_overflow_returns_event_without_dropping_queued_work() {
        let mut queue = BoundedEventQueue::new(config()).unwrap();
        queue.enqueue(TestEvent::Lifecycle(1)).unwrap();
        queue.enqueue(TestEvent::Lifecycle(2)).unwrap();

        assert_eq!(
            queue.enqueue(TestEvent::Lifecycle(3)),
            Err(EnqueueError::LifecycleFull(TestEvent::Lifecycle(3)))
        );
        assert_eq!(
            queue.drain_frame(),
            vec![TestEvent::Lifecycle(1), TestEvent::Lifecycle(2)]
        );
    }

    #[test]
    fn reserved_terminal_survives_saturated_ordinary_queue_and_drains_first() {
        let id = DataTransferId(7);
        let mut queue = BoundedEventQueue::new(config()).unwrap();
        queue.enqueue(TestEvent::Lifecycle(1)).unwrap();
        queue.enqueue(TestEvent::Lifecycle(2)).unwrap();
        queue.enqueue(TestEvent::Motion(id, 3)).unwrap();
        queue.enqueue(TestEvent::Action(id, 4)).unwrap();
        queue.enqueue(TestEvent::Terminal(id, 5)).unwrap();

        assert_eq!(
            queue.drain_frame(),
            vec![
                TestEvent::Terminal(id, 5),
                TestEvent::Lifecycle(1),
                TestEvent::Lifecycle(2),
                TestEvent::Action(id, 4),
                TestEvent::Motion(id, 3),
            ]
        );
    }

    #[test]
    fn terminal_latch_is_exactly_once_per_transfer() {
        let id = DataTransferId(9);
        let mut queue = BoundedEventQueue::new(config()).unwrap();
        queue.enqueue(TestEvent::Terminal(id, 1)).unwrap();
        queue.enqueue(TestEvent::Terminal(id, 2)).unwrap();
        assert_eq!(queue.drain_frame(), vec![TestEvent::Terminal(id, 1)]);
    }

    #[test]
    fn bridge_event_classification_keeps_motion_replaceable() {
        let event = BridgeEvent::Motion {
            transfer_id: DataTransferId(1),
            position: Position { x: 1.0, y: 2.0 },
            transport_revision: crate::types::TransportRevision(1),
        };
        assert_eq!(event.class(), EventClass::Motion((DataTransferId(1), 0)));

        let terminal = BridgeEvent::Terminal(TerminalEvent {
            transfer_id: DataTransferId(1),
            disposition: TerminalDisposition::Rejected,
            reason: TerminalReason::QueueOverflow,
        });
        assert_eq!(terminal.class(), EventClass::Terminal(DataTransferId(1)));
    }

    #[test]
    fn zero_motion_budget_is_rejected_before_it_can_wedge_drop_fence_progress() {
        let mut zero = config();
        zero.motion_drain_budget = 0;
        assert!(matches!(
            BoundedEventQueue::<TestEvent>::new(zero),
            Err(QueueConfigError::ZeroMotionDrainBudget)
        ));
    }

    #[test]
    fn every_zero_capacity_is_rejected() {
        for (config, expected) in [
            (
                QueueConfig {
                    lifecycle_capacity: 0,
                    ..config()
                },
                QueueConfigError::ZeroLifecycleCapacity,
            ),
            (
                QueueConfig {
                    action_capacity: 0,
                    ..config()
                },
                QueueConfigError::ZeroActionCapacity,
            ),
            (
                QueueConfig {
                    motion_capacity: 0,
                    ..config()
                },
                QueueConfigError::ZeroMotionCapacity,
            ),
        ] {
            assert!(matches!(
                BoundedEventQueue::<TestEvent>::new(config),
                Err(error) if error == expected
            ));
        }
    }
}
