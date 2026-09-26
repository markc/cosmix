//! `cosmix-dopus` — the CosMix twin-pane file manager (iced), the windowed
//! frontend of the headless `cosmix-dopus-core`. P1: one live pane.
//! `cosmix-dopus [PATH…]` starts the window registered on the Bus as
//! `dopus`; a second launch forwards its paths to the running instance and
//! exits (the instance accepts and ignores paths until P2).

use cosmix_dopus::dirs::{AppDirs, COMPONENT};

const HELP: &str = "cosmix-dopus — the CosMix twin-pane file manager (iced; P1: one live pane)\n\
Usage: cosmix-dopus [PATH…]\n\
  --headless        no window: the core and the `dopus` Bus port only
                    (PATH… arguments are currently ignored)\n\
  --service NAME    register as NAME instead of `dopus` (tests)\n\
  --noded-url URL   Bus broker endpoint (default: node.conf.mix's noded_url)\n\
  --print-config    print the resolved configuration and exit\n\
  --version         print version and build hash, and nothing else\n\
Bus: serves `dopus.*` (schema dopus.v1); file-mutating actions stay\n\
keyboard-only in P1 and are refused on the Bus (FORBIDDEN).";

struct Args {
    headless: bool,
    print_config: bool,
    service: String,
    noded_url: Option<String>,
    paths: Vec<String>,
}

fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut out = Args {
        headless: false,
        print_config: false,
        service: cosmix_dopus::verbs::SERVICE.to_owned(),
        noded_url: None,
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
            "--noded-url" => {
                let url = args.next().ok_or("--noded-url needs a URL")?;
                if !(url.starts_with("ws://") || url.starts_with("wss://")) {
                    return Err(format!("--noded-url {url:?}: expected ws:// or wss://"));
                }
                out.noded_url = Some(url);
            }
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag} (see --help)")),
            _ => out.paths.push(a),
        }
    }
    Ok(out)
}

/// Paths are resolved against dopus's cwd here, because a forwarded
/// `dopus.open` is served by an instance with a different cwd.
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
            eprintln!("cosmix-dopus: {e}");
            std::process::exit(2);
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("DOPUS_LOG").unwrap_or_else(|_| "warn".into()))
        .with_writer(std::io::stderr)
        .init();

    let dirs = AppDirs::resolve(COMPONENT);
    let (config, config_file) = cosmix_dopus::config::load(dirs.as_ref().map(AppDirs::config_dir).as_deref());
    if args.print_config {
        if let Some(d) = &dirs {
            println!("-- {}", d.config_dir().join("config.conf.mix").display());
        }
        println!("{}", cosmix_dopus::config::to_json(&config));
        return;
    }
    let noded_url = args.noded_url.clone().unwrap_or_else(cosmix_config::client_helpers::resolve_noded_url);
    let paths = absolute(args.paths);
    let result = if args.headless {
        // Headless dopus IS the Bus port: no broker, no process.
        cosmix_dopus::headless::run(config, config_file, dirs, &args.service, &noded_url)
    } else {
        // Single instance: a running dopus takes the paths (which P1 accepts
        // and ignores; P2 gives them panes).
        if cosmix_dopus::bus::probe_running(&noded_url, &args.service) {
            if paths.is_empty() {
                eprintln!("cosmix-dopus: already running as {}", args.service);
                return;
            }
            match cosmix_dopus::bus::forward_open(&noded_url, &args.service, &paths) {
                Ok(()) => return,
                Err(e) => {
                    eprintln!("cosmix-dopus: a running instance answered but refused the paths: {e}");
                    std::process::exit(1);
                }
            }
        }
        cosmix_dopus::app::run(config, config_file, dirs, &args.service, &noded_url, &paths)
    };
    if let Err(e) = result {
        eprintln!("cosmix-dopus: {e:#}");
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
        let a = p(&["--service", "dopus-gate", "--headless", "--", "--weird"]).unwrap();
        assert_eq!(a.service, "dopus-gate");
        assert!(a.headless && !a.print_config);
        assert_eq!(a.paths, ["--weird"]);
        assert!(a.noded_url.is_none());
        assert!(p(&["--service"]).is_err());
        assert!(p(&["--service", "bad name"]).is_err());
        assert!(p(&["--frobnicate"]).is_err());
        assert!(p(&["--noded-url"]).is_err());
        assert!(p(&["--noded-url", "http://x"]).is_err());
        assert_eq!(p(&["--noded-url", "ws://h:1/ws"]).unwrap().noded_url.as_deref(), Some("ws://h:1/ws"));
    }
}
