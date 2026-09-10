//! Broker-owned observation classification. Never deserialised from a header.
use cosmix_bus::bus::{self, BusMessage};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TrafficClass {
    #[default]
    Legacy,
    NativeSession,
}

#[cfg(test)]
mod tests {
    use super::*;
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
            && bus::parse(&message.body).is_ok_and(|inner| {
                inner
                    .command_name()
                    .is_some_and(|c| c.starts_with("noded.session."))
            })
        {
            return Self::NativeSession;
        }
        if message.command_name() == Some("topic.publish")
            && message
                .get("name")
                .is_some_and(|name| crate::props_reservation::reserved_owner(name).is_some())
        {
            return Self::NativeSession;
        }
        Self::Legacy
    }
}
