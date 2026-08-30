//! FileMgr: a native, standalone CTK twin-pane file manager.

mod action;
mod app_port;
mod browser;
mod config;
mod file_ops;

use ctk::identity::AppIdentity;

pub(crate) const IDENTITY: AppIdentity = AppIdentity {
    slug: "filemgr",
    display_name: "CosMix FileMgr",
};

fn main() {
    browser::run();
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
}
