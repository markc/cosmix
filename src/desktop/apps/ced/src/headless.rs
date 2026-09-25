//! `ced --headless` (ced E1 plan §7.3): the Controller and the Bus thread with
//! no window — every `ced.*` verb except `ced.layout` (UNAVAILABLE), so the
//! Mix e2e can drive the real model. Stage E1d implements it.

use crate::config::Config;

/// Run until `app.quit` or SIGTERM. `service` is the Bus name.
pub fn run(service: &str, config: Config) -> anyhow::Result<()> {
    let _ = (service, config);
    todo!("ced E1d")
}
