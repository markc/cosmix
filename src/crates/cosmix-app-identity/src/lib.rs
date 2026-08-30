//! The single identity authority for CosMix desktop apps.

/// The stable slug and branded display name of a desktop application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppIdentity {
    /// Stable component slug used for packages, runtime state and Bus identity.
    pub slug: &'static str,
    /// Human-facing branding used for window and app-control titles.
    pub display_name: &'static str,
}

impl AppIdentity {
    /// Return the freedesktop application identifier for this component.
    pub fn app_id(&self) -> String {
        format!("dev.cosmix.{}", self.slug)
    }

    /// Validate the registry slug grammar: `^[a-z][a-z0-9]{0,15}$`.
    pub fn validate(&self) -> Result<(), &'static str> {
        let bytes = self.slug.as_bytes();
        if bytes.is_empty() || bytes.len() > 16 || !bytes[0].is_ascii_lowercase() {
            return Err("slug must start with an ASCII lowercase letter and be at most 16 bytes");
        }
        if !bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        {
            return Err("slug may contain only ASCII lowercase letters and digits");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_registry_slug_grammar() {
        for slug in ["a", "studio", "filemgr", "a123456789012345"] {
            assert!(
                AppIdentity {
                    slug,
                    display_name: "Test",
                }
                .validate()
                .is_ok()
            );
        }
        for slug in ["", "Studio", "1studio", "studio-app", "abcdefghijklmnopq"] {
            assert!(
                AppIdentity {
                    slug,
                    display_name: "Test",
                }
                .validate()
                .is_err()
            );
        }
    }

    #[test]
    fn derives_freedesktop_app_id() {
        let identity = AppIdentity {
            slug: "studio",
            display_name: "CosMix Studio",
        };
        assert_eq!(identity.app_id(), "dev.cosmix.studio");
    }
}
