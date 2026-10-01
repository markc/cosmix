use cosmix_actions::filemgr;
use cosmix_dopus::verbs::{ActionReq, OpenReq, PaneTarget, apply_action_in};
use cosmix_dopus_core::{DOpusConfig, DopusCore, PaneId, SortColumn};

#[test]
fn pane_request_defaults_and_validation() {
    let action: ActionReq = serde_json::from_str(r#"{"id":"nav.parent"}"#).unwrap();
    assert_eq!(action.pane, PaneTarget::Active);
    let open: OpenReq = serde_json::from_str(r#"{"paths":[]}"#).unwrap();
    assert_eq!(open.pane, None);
    for (target, expected) in [
        (r#""left""#, PaneTarget::Left),
        ("0", PaneTarget::Left),
        (r#""right""#, PaneTarget::Right),
        ("1", PaneTarget::Right),
        (r#""active""#, PaneTarget::Active),
    ] {
        let body = format!(r#"{{"id":"nav.parent","pane":{target}}}"#);
        assert_eq!(
            serde_json::from_str::<ActionReq>(&body).unwrap().pane,
            expected
        );
        let body = format!(r#"{{"paths":["/tmp"],"pane":{target}}}"#);
        assert_eq!(
            serde_json::from_str::<OpenReq>(&body).unwrap().pane,
            Some(expected)
        );
    }
    for target in [r#""other""#, "2", "-1", "1.5", "true", r#""1""#] {
        let body = format!(r#"{{"id":"nav.parent","pane":{target}}}"#);
        assert!(serde_json::from_str::<ActionReq>(&body).is_err());
        let body = format!(r#"{{"paths":[],"pane":{target}}}"#);
        assert!(serde_json::from_str::<OpenReq>(&body).is_err());
    }
    assert_eq!(
        serde_json::to_string(&PaneTarget::Right).unwrap(),
        r#""right""#
    );
}

#[test]
fn targeted_navigation_and_view_actions_preserve_active_pane() {
    let temp = tempfile::tempdir().unwrap();
    let child = temp.path().join("child");
    std::fs::create_dir(&child).unwrap();
    let mut config = DOpusConfig::default();
    config.left.path = temp.path().to_owned();
    config.right.path = child.clone();
    let (mut core, _rx) = DopusCore::new(config, None);
    let left_generation = core.pane(PaneId::Left).generation;
    for action in [
        filemgr::NAV_PARENT,
        filemgr::NAV_BACK,
        filemgr::NAV_FORWARD,
        filemgr::NAV_HOME,
        filemgr::VIEW_REFRESH,
        filemgr::VIEW_TOGGLE_HIDDEN,
        filemgr::VIEW_SORT_SIZE,
        filemgr::VIEW_SORT_MODIFIED,
        filemgr::VIEW_SORT_NAME,
        filemgr::SELECT_NEXT,
        filemgr::SELECT_PREVIOUS,
        filemgr::SELECT_FIRST,
        filemgr::SELECT_LAST,
    ] {
        assert!(apply_action_in(action, &mut core, PaneId::Right).is_ok());
        assert_eq!(core.active(), PaneId::Left);
        let left = core.pane(PaneId::Left);
        assert_eq!(left.path, temp.path());
        assert_eq!(left.generation, left_generation);
        assert_eq!(left.sort, SortColumn::Name);
        assert!(!left.show_hidden);
        assert!(left.selected.is_none());
    }
    assert!(core.pane(PaneId::Right).show_hidden);
}
