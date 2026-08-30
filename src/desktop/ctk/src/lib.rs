//! # CTK — the Cosmix Tool Kit
//!
//! Reusable, **Bus-citizen** widgets for dense / interactive *native* desktop
//! surfaces (a `musicd` mixer first), built on Bevy's `bevy_ui` + `bevy_feathers`.
//!
//! CTK apps decline the Bus Display Protocol renderer and draw themselves, but
//! keep the same Bus command port: local behaviour at 60fps, Bus at the semantic
//! boundary. See `../../../CLAUDE.md` and the cosmix ADR
//! `~/.cmctl/_decisions/2026-07-15-bevy-ctk-native-surface-candidate.md`.

pub mod app_dirs;
#[cfg(feature = "body-view")]
pub mod body_view;
pub mod button;
pub mod chrome;
pub mod dcs;
pub mod dcs_app_shell;
pub mod dialog_shell;
pub mod dnd;
pub mod file_requester;
pub mod fs;
pub mod identity;
pub mod interaction;
pub mod latency;
#[cfg(feature = "menus")]
pub mod menu;
pub mod modal_capture;
#[cfg(feature = "os-dnd")]
pub mod os_dnd;
pub mod text_area;
pub mod text_elide;
pub mod text_field;
pub mod theme;
pub mod topology;
pub mod tree_view;
#[cfg(feature = "virtual-list")]
pub mod virtual_list;
pub mod wave;
pub mod widgets;

mod style;

#[cfg(feature = "bus")]
pub mod bus;

#[cfg(all(feature = "theme", feature = "bus"))]
mod theme_sync;

#[cfg(feature = "bus")]
pub mod app_control;

#[cfg(all(feature = "bus", feature = "actions"))]
pub mod action_control;

#[cfg(feature = "actions")]
pub mod key_input;

#[cfg(feature = "icons")]
pub mod icons;

#[cfg(feature = "mixer")]
pub mod mixer;

#[cfg(feature = "mixer")]
pub mod piano_roll;

#[cfg(feature = "mixer")]
pub mod transport;

/// Common CTK and upstream Feathers types for applications.
pub mod prelude {
    pub use bevy::feathers;
    pub use bevy::feathers::FeathersPlugins;

    pub use crate::app_dirs::AppDirs;
    #[cfg(feature = "body-view")]
    pub use crate::body_view::{
        project_body, sanitize_html, set_body_render_arm, spawn_body_view, BodyProjection,
        BodySource, CtkBodyView, CtkBodyViewEntities, CtkBodyViewPlugin, CtkBodyViewProps,
        LinkActivated, ProjectedBlock, ProjectedBlockKind, ProjectedSpan, RemoteRefs, RenderArm,
        SanitizedBody, SanitizedHtml, BODY_VIEW_MAX_BLOCKS, BODY_VIEW_MAX_INPUT_BYTES,
        BODY_VIEW_MAX_SPANS, BODY_VIEW_MAX_TEXT_RUN_BYTES,
    };
    pub use crate::button::{
        spawn_button, ButtonDef, ButtonLabel, ButtonSize, ButtonVariant, CtkButton,
    };
    #[cfg(feature = "actions")]
    pub use crate::chrome::ToolbarActionButton;
    pub use crate::chrome::{
        spawn_status_bar, spawn_status_text, spawn_toolbar_row, spawn_toolbar_row_aligned,
        ChromePlugin, StatusBarEntities, StatusText, ToolbarAlignment, ToolbarButtonDef,
        ToolbarItem, ToolbarRowEntities,
    };
    #[cfg(feature = "icons")]
    pub use crate::chrome::{spawn_toolbar_row_with_icons, spawn_toolbar_row_with_icons_aligned};
    pub use crate::fs::write_atomic;
    pub use crate::identity::AppIdentity;

    #[cfg(feature = "icons")]
    pub use crate::icons::{
        file_icon, prepare_data_root, spawn_icon, Icon, IconRasterError, IconSet, SvgColor,
        SvgFile, SvgPlugin, ThemeSvgColor, UiSvg,
    };

    pub use crate::dcs::{
        spawn_dcs_shell, spawn_dcs_split, DcsPanel, DcsShell, DcsShellEntities, DcsShellPlugin,
        DcsShellProps, DcsShellState, DcsSide, DcsSidebarControlVisuals, DcsSidebarMode,
        DcsSidebarState, DcsSplitEntities, DcsSplitProps, DcsSplitState,
    };
    #[cfg(feature = "icons")]
    pub use crate::dcs_app_shell::spawn_dcs_app_shell_with_icons;
    pub use crate::dcs_app_shell::{
        spawn_dcs_app_shell, DcsAppShell, DcsAppShellEntities, DcsAppShellPlugin, DcsAppShellProps,
    };

    pub use crate::dialog_shell::{
        spawn_dialog_shell, DialogShell, DialogShellEntities, DialogShellPanel, DialogShellRoot,
    };
    pub use crate::dnd::{
        dnd_click_is_blocked, AcceptanceProposal, ActionMask, AppResolve, DeliveryId,
        DndCancelReason, DndCancelled, DndClickSuppressed, DndCommit, DndDeliveryCancelled,
        DndDrop, DndHighlightChanged, DndOrigin, DndPlugin, DndPropose, DragPayload, DragPhase,
        DragSession, DragSource, DropAcceptance, DropAction, DropComplete, DropDecisionRequirement,
        DropOutcome, DropTarget, ExportIconRaster, ExportIconRasterError, GhostBuilder, Modifiers,
        PayloadSummary, ProposalId, ProposalRevision, TransferId, DND_GHOST_Z, DRAG_THRESHOLD_PX,
    };
    #[cfg(feature = "os-dnd")]
    pub use crate::os_dnd::{
        outgoing_icon_from_raster, DropDecision, DropDecisionKind, DropDecisionResult,
        DropDecisionStatus, OsDndPlugin, PositionlessFileDrop,
    };

    pub use crate::file_requester::{
        FileFilter, FileRequest, FileRequestId, FileRequestMode, FileRequestOutcome,
        FileRequestResult, FileRequesterPlugin, FileRequesterState, FileRequesterSystems,
        WithdrawFileRequest,
    };

    pub use crate::interaction::{
        ActionRole, ChoiceItem, ChoiceSpec, ConfirmSpec, DialogCommon, InteractionAction,
        InteractionId, InteractionKind, InteractionOutcome, InteractionPlugin, InteractionRequest,
        InteractionResult, InteractionSeverity, InteractionState, InteractionSystems,
        InteractionValue, MessageSpec, ModalCoordinator, MultiChoiceSpec, ProgressComplete,
        ProgressCompletion, ProgressSpec, ProgressState, ProgressUpdate, ProgressValue, PromptSpec,
        SecretPromptSpec, SliderSpec, TextViewSpec, WithdrawInteraction,
    };
    pub use crate::text_area::{
        redo_text_area, spawn_text_area, undo_text_area, CtkTextArea, CtkTextAreaBlurred,
        CtkTextAreaChanged, CtkTextAreaEntities, CtkTextAreaPlugin, CtkTextAreaProps,
        CtkTextAreaRedo, CtkTextAreaSubmitted, CtkTextAreaUndo,
    };
    pub use crate::text_elide::{elide_filename_middle_with_measure, MiddleElideText};
    pub use crate::text_field::{
        set_text_field_error, spawn_secret_field, spawn_text_field, validate_filename,
        CtkSecretField, CtkSecretFieldProps, CtkTextField, CtkTextFieldEntities,
        CtkTextFieldPlugin, CtkTextFieldProps, CtkTextInputFocusBorder, SecretValue, TextValidator,
    };

    #[cfg(feature = "actions")]
    pub use crate::key_input::EventKeyState;
    #[cfg(feature = "menus")]
    pub use crate::menu::{
        spawn_context_menu, spawn_menu_bar, ContextMenu, MenuActivated, MenuBar, MenuBarPlugin,
        MenuDef, MenuItemDef, MenuItemLabel, MenuItemMarker, MenuItemPresentation,
        MenuPresentation, CONTEXT_MENU_Z,
    };
    #[cfg(all(feature = "menus", feature = "icons"))]
    pub use crate::menu::{spawn_context_menu_with_icons, spawn_menu_bar_with_icons};
    #[cfg(all(feature = "menus", feature = "actions"))]
    pub use crate::menu::{
        validate_menu_against_registry, ActionBridgeBar, ActionBridgePlugin,
        ActionRegistryResource, ActionRequest, MenuActionRegistry, MenuActivationOrigin,
        MenuKeymap, MenuValidationIssue, Source,
    };
    pub use crate::modal_capture::{
        ModalCapture, ModalCaptureLayer, ModalCaptureOwner, ModalCapturePlugin,
        ModalCaptureSystems, ModalCaptureToken,
    };
    pub use crate::theme::{
        apply_theme, ApplyTheme, CtkThemeMetrics, CtkThemePlugin, CtkTypography,
        CtkTypographyOptOut, Mode, Oklch, RadiusScale, Scheme, ThemeColors, ThemeSpec, ThemeState,
        TypographyFallback, TypographyProvenance, TypographySpec,
    };
    #[cfg(feature = "theme")]
    pub use crate::theme::{
        load_theme_file, resolve_app_theme, resolve_app_theme_with_selection, resolve_theme,
        resolve_theme_with_selection, shared_theme_path, ThemeFile, ThemeWriteCompleted,
        ThemeWriteRequest, TypographyFile, THEME_FILE,
    };
    #[cfg(all(feature = "theme", feature = "bus"))]
    pub use crate::theme_sync::{ThemeChanged, THEME_CHANGED_TOPIC};
    pub use crate::topology::{
        spawn_topology_canvas, spawn_topology_edge, spawn_topology_node, TopologyCanvas,
        TopologyCanvasEntities, TopologyCanvasPlugin, TopologyCanvasProps, TopologyCanvasState,
        TopologyEdge, TopologyEdgeProps, TopologyNode, TopologyNodeEntities, TopologyNodeProps,
    };
    pub use crate::tree_view::{
        spawn_tree_disclosure, spawn_tree_disclosure_with_icons, spawn_tree_view, sync_tree_view,
        TreeDisclosure, TreeItem, TreeView, TreeViewChanged, TreeViewPlugin,
    };
    #[cfg(feature = "virtual-list")]
    pub use crate::virtual_list::{
        changed as virtual_list_changed, scroll_to as virtual_list_scroll_to, spawn_virtual_list,
        Align, ChangeHint, Overscan, RowId, SelectionMode, VirtualList, VirtualListEntities,
        VirtualListModel, VirtualListModelChanged, VirtualListPlugin, VirtualListProps,
        VirtualListRow, VirtualListRowActivated, VirtualListSelectionChanged,
    };
    pub use crate::wave::{
        format_ruler_secs, paint_region_lane, paint_ruler, paint_wave_lane, ruler_minor_step_secs,
        ruler_ticks, RegionLanePaintParams, RulerTicks, WavePyramid, WaveRegion,
    };
    pub use crate::widgets::{
        action_button, fader, fader_sized, hfader_sized, knob, knob_sized, level_meter,
        level_meter_sized, toggle_button, toggle_button_sized, ActionButton, BusWidget,
        ControlChange, ControlGestureCancel, ControlMeta, ControlRange, ControlValue,
        CtkWidgetsPlugin, Fader, FaderHorizontal, KeyboardControlQueue, KeyboardControlSystems,
        KeyboardInputOrder, Knob, LevelMeter, MappingError, MeterLane, MeterValue,
        NumericControlProps, SetControlValue, SetToggleValue, ToggleButton, ValueMapping,
    };

    #[cfg(feature = "bus")]
    pub use crate::bus::{
        provenance_from_build, resolve_noded_url, BusBridge, BusBridgeConfig, BusBridgeEvent,
        BusBridgePlugin, BusConnectionState, BusMessage, BusReply, InboundRequest,
    };

    #[cfg(feature = "bus")]
    pub use crate::app_control::{
        AppControlInfo, AppControlPlugin, AppPortAppExt, AppPortPlugin, AppPortReply,
        AppPortRequest, AppPortSystems, ControlRegistry, WidgetControlPlugin, APP_CONTROL_CONTRACT,
        APP_ENGINE,
    };

    #[cfg(all(feature = "bus", feature = "actions"))]
    pub use crate::action_control::{
        prepare_action_invocation, validate_action_direct_verbs, ActionPortError, ActionPortPlugin,
        ACTIONS_DESCRIBE_VERB, ACTIONS_LIST_VERB, ACTION_ERROR_DISABLED, ACTION_ERROR_INTERACTIVE,
        ACTION_ERROR_INVALID_ARGS, ACTION_ERROR_LOCAL_ONLY_INTERACTIVE, ACTION_ERROR_MODAL_ACTIVE,
        ACTION_ERROR_REMOTE_IDENTITY_UNAVAILABLE, ACTION_ERROR_SOURCE_NOT_ALLOWED,
        ACTION_ERROR_UNKNOWN, ACTION_ERROR_UNREGISTERED_CALLER, ACTION_INVOKE_VERB,
    };

    #[cfg(feature = "mixer")]
    pub use crate::mixer::{
        db_to_meter_position, default_fader_mapping, extrapolate_position_seconds, smoke,
        spawn_channel_strip, spawn_channel_strip_styled, spawn_master_strip, spawn_song_footer,
        spawn_transport_footer, transport_is_playing, transport_length_secs, ChannelStripEntities,
        DesiredTransport, MixerBinding, MixerBindingKind, MixerMeterBinding,
        MixerTransportIngressSystems, MusicdMixerPlugin, MusicdMixerState, StripStyle,
        TransportFooter, TransportPosition, TransportScrubber, TransportSeekRequest,
        TransportState, TransportTimeReadout,
    };

    #[cfg(feature = "mixer")]
    pub use crate::piano_roll::{
        channel_color, spawn_piano_roll, PianoRollClick, PianoRollEntities, PianoRollGrid,
        PianoRollModel, PianoRollNote, PianoRollPlugin,
    };

    #[cfg(feature = "mixer")]
    pub use crate::transport::{
        ChangedEvent, MixerConnectionState, MixerTransport, MixerTransportRes, TransportEvent,
        TransportMessage, TransportPoll, TransportReply,
    };

    #[cfg(all(feature = "bus", feature = "mixer"))]
    pub use crate::bus::BusTransport;
}

/// Compile-time CTK version.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
