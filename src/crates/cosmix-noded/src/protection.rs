//! Broker-owned observation classification. Never deserialised from a header.
use cosmix_bus::bus::{self, BusMessage};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TrafficClass {
    #[default]
    Legacy,
    NativeSession,
}

impl TrafficClass {
    pub(crate) fn merge(self, other: Self) -> Self {
        if self == Self::NativeSession || other == Self::NativeSession {
            Self::NativeSession
        } else {
            Self::Legacy
        }
    }
    pub(crate) fn protected(self) -> bool {
        self == Self::NativeSession
    }

    pub(crate) fn command(message: &BusMessage) -> Self {
        if message
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("command") && v.starts_with("noded.session."))
        {
            return Self::NativeSession;
        }
        // Check the inner command before topic/property canonicalisation.
        if message.command_name() == Some("topic.publish")
            && message.body.starts_with("---\n")
            && bus::parse(&message.body).is_ok_and(|inner| {
                inner
                    .command_name()
                    .is_some_and(|c| c.starts_with("noded.session."))
            })
        {
            return Self::NativeSession;
        }
        Self::Legacy
    }
}

/// Correlation precision cache, not a confidentiality deadline. The responder's
/// connection-lifetime sticky bit is the unconditional backstop after expiry.
/// Overflow conservatively protects unknown responses during this cache horizon.
#[derive(Default)]
pub(crate) struct ResponseProtection {
    entries: std::collections::HashMap<String, std::time::Instant>,
    overflow_until: Option<std::time::Instant>,
}
impl ResponseProtection {
    #[cfg(test)]
    pub(crate) fn expire_for_test(&mut self) {
        let past = std::time::Instant::now() - Self::HORIZON;
        for deadline in self.entries.values_mut() {
            *deadline = past;
        }
        self.overflow_until = None;
    }
    const HORIZON: std::time::Duration = std::time::Duration::from_secs(15 * 60);
    const LIMIT: usize = 65_536;
    pub(crate) fn retain(&mut self, id: &str, class: TrafficClass) {
        if !class.protected() {
            return;
        }
        let now = std::time::Instant::now();
        if self.entries.len() >= Self::LIMIT {
            self.entries.retain(|_, deadline| *deadline > now);
        }
        let deadline = now + Self::HORIZON;
        if self.entries.len() < Self::LIMIT || self.entries.contains_key(id) {
            self.entries.insert(id.to_owned(), deadline);
        } else {
            self.overflow_until = Some(deadline);
        }
    }
    pub(crate) fn class(&self, id: &str) -> TrafficClass {
        let now = std::time::Instant::now();
        if self.entries.get(id).is_some_and(|until| *until > now)
            || self.overflow_until.is_some_and(|until| until > now)
        {
            TrafficClass::NativeSession
        } else {
            TrafficClass::Legacy
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tombstones_are_bounded_and_overflow_never_exposes_payloads() {
        let mut history = ResponseProtection::default();
        let future = std::time::Instant::now() + ResponseProtection::HORIZON;
        for id in 0..ResponseProtection::LIMIT {
            history.entries.insert(id.to_string(), future);
        }
        history.retain("overflow", TrafficClass::NativeSession);
        assert_eq!(history.entries.len(), ResponseProtection::LIMIT);
        assert!(history.class("overflow").protected());
        assert!(history.class("unknown").protected());
        history.overflow_until = Some(std::time::Instant::now());
        assert_eq!(history.class("unknown"), TrafficClass::Legacy);
        assert!(history.class("0").protected());
        history
            .entries
            .insert("0".into(), std::time::Instant::now());
        assert_eq!(history.class("0"), TrafficClass::Legacy);
    }
    #[test]
    fn classification_is_broker_owned_and_monotonic() {
        let forged = BusMessage::new()
            .with_header("native-session", "1")
            .with_header("broker_principal", "forged");
        assert_eq!(TrafficClass::command(&forged), TrafficClass::Legacy);
        let command = BusMessage::new().with_header("Command", "noded.session.prove");
        assert_eq!(TrafficClass::command(&command), TrafficClass::NativeSession);
        let outer = BusMessage::new()
            .with_header("command", "topic.publish")
            .with_body(
                &BusMessage::new()
                    .with_header("command", "noded.session.prove")
                    .to_wire(),
            );
        assert!(TrafficClass::command(&outer).protected());
        assert!(
            TrafficClass::NativeSession
                .merge(TrafficClass::Legacy)
                .protected()
        );
        assert!(
            TrafficClass::Legacy
                .merge(TrafficClass::NativeSession)
                .protected()
        );
    }
}
