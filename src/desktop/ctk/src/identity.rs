//! Stable application identity for CosMix Desktop components.

pub use cosmix_app_identity::AppIdentity;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reexports_app_identity() {
        let identity = AppIdentity {
            slug: "studio",
            display_name: "CosMix Studio",
        };
        assert!(identity.validate().is_ok());
        assert_eq!(identity.app_id(), "dev.cosmix.studio");
        assert_eq!(identity.display_name, "CosMix Studio");
    }
}
