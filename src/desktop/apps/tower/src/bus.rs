//! Tower's fixed local subscriptions and topic-to-snapshot mapping.

pub(crate) const WORLD_NODED: &str = "world.noded";
pub(crate) const WORLD_INDEXD: &str = "world.indexd";
pub(crate) const WORLD_MUSICD: &str = "world.musicd";
pub(crate) const INTERACT_CHANGED: &str = "interact.props.changed";
pub(crate) const THEME_CHANGED: &str = "theme.changed";

pub(crate) const SUBSCRIPTIONS: [&str; 5] = [
    WORLD_NODED,
    WORLD_INDEXD,
    WORLD_MUSICD,
    INTERACT_CHANGED,
    THEME_CHANGED,
];

pub(crate) fn retained_service(topic: &str) -> Option<&'static str> {
    match topic {
        WORLD_NODED => Some("noded"),
        WORLD_INDEXD => Some("indexd"),
        WORLD_MUSICD => Some("musicd"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_real_retained_topics_map_to_services() {
        assert_eq!(retained_service(WORLD_NODED), Some("noded"));
        assert_eq!(retained_service(WORLD_INDEXD), Some("indexd"));
        assert_eq!(retained_service(WORLD_MUSICD), Some("musicd"));
        assert_eq!(retained_service(INTERACT_CHANGED), None);
        assert_eq!(retained_service(THEME_CHANGED), None);
    }
}
