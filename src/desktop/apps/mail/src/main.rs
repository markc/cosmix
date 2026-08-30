//! CosMix Mail — the frontend mail reader and composer.
//!
//! `maild` is the backend mail server and keeps that name; this is the client.
//! The pair is APPS.md's one standing collision exception (2026-07-31).
//!
//! ## What this binary currently is
//!
//! The vertical slice that puts ctk's `virtual_list`, `body_view` and
//! `text_area` under one real consumer, over a fixture corpus. There is no
//! JMAP client, no Bus dependency and no persistence: the point was to find
//! out what the three widgets are missing before more is built on them, and
//! composing them against a fixture store finds that just as well as a
//! transport would, sooner.
//!
//! What it found is recorded where it belongs — `list.rs` on uniform row
//! height, `reader.rs` on the absent body-swap API and the hard limit on
//! remote images, `probe.rs` on the absent programmatic selection API,
//! `compose.rs` on the clipboard feature left off.

mod app;
mod compose;
mod fixtures;
mod list;
mod probe;
mod reader;
mod store;

use ctk::identity::AppIdentity;

pub(crate) const IDENTITY: AppIdentity = AppIdentity {
    slug: "mail",
    display_name: "CosMix Mail",
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let plan = match probe::ProbePlan::parse(&args) {
        Ok(plan) => plan,
        Err(error) => {
            eprintln!("cosmix-mail: {error}");
            std::process::exit(2);
        }
    };
    app::run(plan);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_matches_package_and_workspace_path() {
        assert!(IDENTITY.validate().is_ok());
        assert_eq!(env!("CARGO_PKG_NAME"), format!("cosmix-{}", IDENTITY.slug));
        assert!(env!("CARGO_MANIFEST_DIR").ends_with(&format!("/apps/{}", IDENTITY.slug)));
    }

    #[test]
    fn the_slug_is_the_one_registered_in_apps_md() {
        // Guards the 2026-07-31 exception: if someone re-slugs this app, the
        // APPS.md row and the ADR that released `mail` both go stale.
        assert_eq!(IDENTITY.slug, "mail");
    }
}
