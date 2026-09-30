//! Headless owning-service acceptance fixture. Runs the real CTK Bus bridge,
//! widget admission and ordinary final ControlChange path; no display backend.

use bevy::{app::ScheduleRunnerPlugin, prelude::*};
use ctk::{
    app_control::{AppPortAppExt, AppPortReply, AppPortRequest},
    prelude::*,
};

#[derive(Resource, Default)]
struct AppliedChanges(usize);

fn record(change: On<ControlChange>, mut applied: ResMut<AppliedChanges>) {
    if change.is_final {
        applied.0 += 1;
    }
}

fn observed(_: In<AppPortRequest>, applied: Res<AppliedChanges>) -> AppPortReply {
    (
        0,
        serde_json::json!({"final_changes":applied.0}).to_string(),
    )
}

fn main() {
    cosmix_buildinfo::exit_on_version!();
    let url = std::env::var("COSMIX_MCP_TEST_URL").expect("explicit isolated broker URL required");
    let mut app = App::new();
    app.add_plugins(MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(
        std::time::Duration::from_millis(10),
    )))
    .add_plugins((
        CtkWidgetsPlugin,
        BusBridgePlugin::new(BusBridgeConfig::new("mcp-control-probe", url)),
        AppControlPlugin::new("MCP control probe", "probe"),
    ))
    .init_resource::<AppliedChanges>()
    .add_observer(record)
    .register_app_verb("app.probe", observed);
    app.world_mut().spawn(knob_sized(
        NumericControlProps::new(
            "trim",
            0.0,
            ControlRange {
                min: -18.0,
                max: 18.0,
                step: 0.1,
                detent: None,
            },
            ValueMapping::linear(-18.0, 18.0).unwrap(),
        ),
        26.0,
    ));
    app.run();
}
