//! `ced` — the CosMix Editor (ced E1). Stage S skeleton: `--version` and
//! `--help` answer; everything else refuses until E1d/E1f land.

const HELP: &str = "ced — the CosMix Editor (iced), a client of the `edit` Bus service\n\
Usage: ced [PATH[:LINE[:COL]]…]\n\
  --headless        no window: the controller and the `ced` Bus port only\n\
  --service NAME    register as NAME instead of `ced` (tests)\n\
  --print-config    print the resolved configuration and exit\n\
  --version         print version and build hash, and nothing else\n\
Bus: serves `ced.*` (schema ced.v1) and `app.describe` / `app.quit`.";

fn main() {
    // FIRST, before any config read, Bus connect or Wayland check: `--version`
    // reports the version and the build hash and does nothing else.
    cosmix_buildinfo::exit_on_version!();
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!("{HELP}");
        return;
    }
    eprintln!("ced: this is the ced E1 Stage S skeleton; the editor lands in stages E1d-E1f");
    std::process::exit(2);
}
