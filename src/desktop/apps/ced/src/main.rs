//! `ced` — the CosMix Editor (ced E1). `ced [PATH[:LINE[:COL]]…]` opens the
//! paths in the running instance if there is one (plan §4.8: an anonymous
//! `ced.ping`, then `ced.open`), otherwise starts the window registered on
//! the Bus as `ced`.

use cosmix_ced::config::{self, Config};
use cosmix_ced::dirs::{AppDirs, COMPONENT};

const HELP: &str = "ced — the CosMix Editor (iced), a client of the `edit` Bus service\n\
Usage: ced [PATH[:LINE[:COL]]…]\n\
  --headless        no window: the controller and the `ced` Bus port only\n\
  --service NAME    register as NAME instead of `ced` (tests)\n\
  --print-config    print the resolved configuration and exit\n\
  --version         print version and build hash, and nothing else\n\
Bus: serves `ced.*` (schema ced.v1) and `app.describe` / `app.quit`.";

struct Args {
    headless: bool,
    print_config: bool,
    service: String,
    paths: Vec<String>,
}

fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut out = Args {
        headless: false,
        print_config: false,
        service: cosmix_ced::verbs::SERVICE.to_owned(),
        paths: Vec::new(),
    };
    let mut args = args.peekable();
    let mut only_paths = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            _ if only_paths => out.paths.push(a),
            "--" => only_paths = true,
            "--headless" => out.headless = true,
            "--print-config" => out.print_config = true,
            "--service" => {
                let name = args.next().ok_or("--service needs a name")?;
                if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
                    return Err(format!("--service {name:?}: use letters, digits, '-', '_' or '.'"));
                }
                out.service = name;
            }
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag} (see --help)")),
            _ => out.paths.push(a),
        }
    }
    Ok(out)
}

/// Paths are resolved against ced's cwd here, because a forwarded `ced.open`
/// is served by an instance with a different cwd.
fn absolute(paths: Vec<String>) -> Vec<String> {
    let cwd = std::env::current_dir().ok();
    paths
        .into_iter()
        .map(|p| match &cwd {
            Some(cwd) if !std::path::Path::new(&p).is_absolute() => cwd.join(&p).to_string_lossy().into_owned(),
            _ => p,
        })
        .collect()
}

fn main() {
    // FIRST, before any config read, Bus connect or Wayland check: `--version`
    // reports the version and the build hash and does nothing else.
    cosmix_buildinfo::exit_on_version!();
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!("{HELP}");
        return;
    }
    let args = match parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("ced: {e}");
            std::process::exit(2);
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("CED_LOG").unwrap_or_else(|_| "warn".into()))
        .with_writer(std::io::stderr)
        .init();

    let dirs = AppDirs::resolve(COMPONENT);
    let (config, note) = match &dirs {
        Some(d) => config::load(&d.config_file()),
        None => (Config::default(), Some("no HOME: AppDirs unresolved; using defaults".to_owned())),
    };
    if let Some(note) = &note {
        eprintln!("ced: {note}");
    }
    if args.print_config {
        if let Some(d) = &dirs {
            println!("-- {}", d.config_file().display());
        }
        println!("{}", config.to_json());
        return;
    }
    let paths = absolute(args.paths);
    let result = if args.headless {
        cosmix_ced::headless::run(&args.service, config)
    } else {
        // Single instance (plan §4.8): a running ced takes the paths.
        if cosmix_ced::bus::probe_running(&args.service) {
            match cosmix_ced::bus::forward_open(&args.service, &paths) {
                Ok(()) => return,
                Err(e) => {
                    eprintln!("ced: a running instance answered but refused the paths: {e}");
                    std::process::exit(1);
                }
            }
        }
        cosmix_ced::app::run(&args.service, config, paths)
    };
    if let Err(e) = result {
        eprintln!("ced: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(v: &[&str]) -> Result<Args, String> {
        parse(v.iter().map(|s| s.to_string()))
    }

    #[test]
    fn arguments() {
        let a = p(&["--service", "ced-gate", "a.mix", "--headless", "--", "--weird"]).unwrap();
        assert_eq!(a.service, "ced-gate");
        assert!(a.headless && !a.print_config);
        assert_eq!(a.paths, ["a.mix", "--weird"]);
        assert!(p(&["--service"]).is_err());
        assert!(p(&["--service", "bad name"]).is_err());
        assert!(p(&["--frobnicate"]).is_err());
    }
}
