use ctk::identity::AppIdentity;

pub(crate) const IDENTITY: AppIdentity = AppIdentity {
    slug: "tower",
    display_name: "CosMix Tower",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_matches_package_workspace_and_native_app_id() {
        assert!(IDENTITY.validate().is_ok());
        assert_eq!(env!("CARGO_PKG_NAME"), format!("cosmix-{}", IDENTITY.slug));
        assert!(env!("CARGO_MANIFEST_DIR").ends_with(&format!("/apps/{}", IDENTITY.slug)));
        assert_eq!(IDENTITY.app_id(), "dev.cosmix.tower");
    }
}
