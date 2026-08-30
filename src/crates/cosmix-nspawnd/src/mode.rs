use clap::ValueEnum;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Mode {
    Executor,
    Controller,
}

impl Mode {
    pub fn resolve(cli: Option<Self>, env: Option<&str>) -> Result<Self, String> {
        if let Some(mode) = cli {
            return Ok(mode);
        }
        match env.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("executor") => Ok(Self::Executor),
            Some("controller") => Ok(Self::Controller),
            Some(value) => Err(format!(
                "COSMIX_NSPAWND_MODE must be executor or controller, got {value:?}"
            )),
        }
    }

    pub fn needs_executor_stack(self) -> bool {
        matches!(self, Self::Executor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_overrides_environment_and_executor_is_default() {
        assert_eq!(Mode::resolve(None, None).unwrap(), Mode::Executor);
        assert_eq!(
            Mode::resolve(Some(Mode::Executor), Some("controller")).unwrap(),
            Mode::Executor
        );
        assert_eq!(
            Mode::resolve(None, Some("controller")).unwrap(),
            Mode::Controller
        );
        assert!(!Mode::Controller.needs_executor_stack());
        assert!(Mode::resolve(None, Some("other")).is_err());
    }
}
