//! Bounded Freedesktop application discovery and Exec expansion.

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

const APPLICATIONS_DIR: &str = "/usr/local/share/applications";
const DESKTOP_PREFIX: &str = "dev.cosmix.";
const DESKTOP_SUFFIX: &str = ".desktop";
const TRAY_DESKTOP_FILE: &str = "dev.cosmix.tray.desktop";
const MAX_DESKTOP_FILES: usize = 64;
/// Upper bound on directory entries EXAMINED per scan (matching or not).
const MAX_DESKTOP_SCAN: usize = 512;
const MAX_DESKTOP_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DesktopApp {
    pub(crate) slug: String,
    pub(crate) label: String,
    pub(crate) icon: Option<String>,
    pub(crate) argv: Option<Vec<String>>,
}

impl DesktopApp {
    fn unavailable(slug: String, label: String, error: impl AsRef<str>) -> Self {
        Self {
            slug,
            label: format!("{label} (unavailable: {})", concise(error.as_ref())),
            icon: None,
            argv: None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ParsedDesktopEntry {
    pub(crate) name: String,
    pub(crate) icon: Option<String>,
    pub(crate) argv: Vec<String>,
}

pub(crate) fn discover() -> Result<Vec<DesktopApp>, String> {
    discover_in(Path::new(APPLICATIONS_DIR))
}

fn discover_in(directory: &Path) -> Result<Vec<DesktopApp>, String> {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?;
    let mut apps = Vec::new();

    // The cap bounds directory ITERATION, not just accepted records — a huge
    // unrelated directory must not be enumerated to the end.
    for entry in entries.take(MAX_DESKTOP_SCAN) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                if apps.len() < MAX_DESKTOP_FILES {
                    apps.push(DesktopApp::unavailable(
                        "unknown".into(),
                        "Unreadable desktop entry".into(),
                        error.to_string(),
                    ));
                }
                continue;
            }
        };
        let filename = entry.file_name().to_string_lossy().into_owned();
        if filename == TRAY_DESKTOP_FILE
            || !filename.starts_with(DESKTOP_PREFIX)
            || !filename.ends_with(DESKTOP_SUFFIX)
        {
            continue;
        }
        if apps.len() == MAX_DESKTOP_FILES {
            break;
        }

        let slug = match desktop_slug(&filename) {
            Some(slug) => slug,
            None => {
                apps.push(DesktopApp::unavailable(
                    "invalid".into(),
                    filename,
                    "invalid CosMix application slug",
                ));
                continue;
            }
        };
        match entry.file_type() {
            Ok(file_type) if file_type.is_file() => {}
            Ok(_) => continue,
            Err(error) => {
                apps.push(DesktopApp::unavailable(
                    slug,
                    filename,
                    format!("cannot inspect file type: {error}"),
                ));
                continue;
            }
        }

        let path = entry.path();
        let input = match read_bounded(&path) {
            Ok(input) => input,
            Err(error) => {
                apps.push(DesktopApp::unavailable(slug, filename, error));
                continue;
            }
        };
        match parse_desktop_entry(&input, &path) {
            Ok(parsed) => apps.push(DesktopApp {
                slug,
                label: parsed.name,
                icon: parsed.icon,
                argv: Some(parsed.argv),
            }),
            Err(error) => apps.push(DesktopApp::unavailable(slug, filename, error)),
        }
    }

    apps.sort_by_key(|app| app.label.to_lowercase());
    Ok(apps)
}

fn desktop_slug(filename: &str) -> Option<String> {
    let slug = filename
        .strip_prefix(DESKTOP_PREFIX)?
        .strip_suffix(DESKTOP_SUFFIX)?;
    let bytes = slug.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 16
        || !bytes[0].is_ascii_lowercase()
        || !bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return None;
    }
    Some(slug.to_owned())
}

fn read_bounded(path: &Path) -> Result<String, String> {
    let file =
        File::open(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut bytes = Vec::with_capacity(MAX_DESKTOP_BYTES.min(4096));
    file.take(MAX_DESKTOP_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if bytes.len() > MAX_DESKTOP_BYTES {
        return Err(format!(
            "{} exceeds the {MAX_DESKTOP_BYTES}-byte desktop-file limit",
            path.display()
        ));
    }
    String::from_utf8(bytes)
        .map_err(|error| format!("{} is not valid UTF-8: {error}", path.display()))
}

pub(crate) fn parse_desktop_entry(
    input: &str,
    desktop_path: &Path,
) -> Result<ParsedDesktopEntry, String> {
    let mut in_desktop_entry = false;
    let mut name = None;
    let mut icon = None;
    let mut exec = None;

    for raw_line in input.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_desktop_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_desktop_entry || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(value) = line.strip_prefix("Name=") {
            if name.is_none() && !value.is_empty() {
                name = Some(unescape_string(value)?);
            }
        } else if let Some(value) = line.strip_prefix("Icon=") {
            if icon.is_none() && !value.is_empty() {
                icon = Some(unescape_string(value)?);
            }
        } else if let Some(value) = line.strip_prefix("Exec=") {
            if exec.is_none() && !value.is_empty() {
                exec = Some(value);
            }
        }
    }

    let name = name.ok_or("missing Name= in [Desktop Entry]")?;
    let exec = exec.ok_or("missing Exec= in [Desktop Entry]")?;
    let command_line = unescape_string(exec)?;
    let tokens = tokenise_exec(&command_line)?;
    let argv = expand_field_codes(tokens, &name, icon.as_deref(), desktop_path)?;
    if argv.is_empty() {
        return Err("Exec= has no executable".into());
    }
    if argv[0].contains('=') {
        return Err("Exec= executable may not contain '='".into());
    }
    Ok(ParsedDesktopEntry { name, icon, argv })
}

/// Apply the general desktop-entry string escapes before Exec quoting.
fn unescape_string(input: &str) -> Result<String, String> {
    let mut characters = input.chars();
    let mut output = String::with_capacity(input.len());
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        let escaped = characters
            .next()
            .ok_or_else(|| "value ends with an incomplete escape".to_owned())?;
        output.push(match escaped {
            's' => ' ',
            'n' => '\n',
            't' => '\t',
            'r' => '\r',
            '\\' => '\\',
            other => return Err(format!("unsupported desktop-entry escape \\{other}")),
        });
    }
    Ok(output)
}

#[derive(Debug, PartialEq, Eq)]
struct ExecToken {
    value: String,
    quoted: bool,
}

fn tokenise_exec(input: &str) -> Result<Vec<ExecToken>, String> {
    let mut quoted = false;
    let mut token_quoted = false;
    let mut token_started = false;
    let mut token = String::new();
    let mut tokens = Vec::new();
    let mut characters = input.chars();

    while let Some(character) = characters.next() {
        if quoted {
            match character {
                '"' => quoted = false,
                '\\' => {
                    let escaped = characters
                        .next()
                        .ok_or_else(|| "Exec= ends with an incomplete quoted escape".to_owned())?;
                    if matches!(escaped, '"' | '`' | '$' | '\\') {
                        token.push(escaped);
                    } else {
                        return Err(format!(
                            "Exec= cannot escape {escaped:?} inside a quoted argument"
                        ));
                    }
                }
                '$' | '`' => {
                    return Err(format!(
                        "Exec= requires {character:?} to be backslash-escaped inside a quoted argument"
                    ));
                }
                other => token.push(other),
            }
            token_started = true;
            continue;
        }

        match character {
            '"' => {
                quoted = true;
                token_quoted = true;
                token_started = true;
            }
            other if other.is_whitespace() => {
                if token_started {
                    tokens.push(ExecToken {
                        value: std::mem::take(&mut token),
                        quoted: token_quoted,
                    });
                    token_started = false;
                    token_quoted = false;
                }
            }
            other if is_reserved_exec_character(other) => {
                return Err(format!(
                    "Exec= reserved character {other:?} must be inside double quotes"
                ));
            }
            other => {
                token.push(other);
                token_started = true;
            }
        }
    }

    if quoted {
        return Err("Exec= contains an unterminated double quote".into());
    }
    if token_started {
        tokens.push(ExecToken {
            value: token,
            quoted: token_quoted,
        });
    }
    Ok(tokens)
}

fn is_reserved_exec_character(character: char) -> bool {
    matches!(
        character,
        '\'' | '\\' | '>' | '<' | '~' | '|' | '&' | ';' | '$' | '*' | '?' | '#' | '(' | ')' | '`'
    )
}

fn expand_field_codes(
    tokens: Vec<ExecToken>,
    name: &str,
    icon: Option<&str>,
    desktop_path: &Path,
) -> Result<Vec<String>, String> {
    let desktop_path = desktop_path.to_string_lossy();
    let mut argv = Vec::new();
    let mut file_code_count = 0;

    for token in tokens {
        if token.quoted && contains_field_code(&token.value) {
            return Err("Exec= field codes may not appear inside quoted arguments".into());
        }
        if token.value == "%i" {
            if let Some(icon) = icon.filter(|icon| !icon.is_empty()) {
                argv.push("--icon".into());
                argv.push(icon.into());
            }
            continue;
        }
        if matches!(token.value.as_str(), "%F" | "%U") {
            file_code_count += 1;
            continue;
        }

        let mut characters = token.value.chars();
        let mut expanded = String::new();
        while let Some(character) = characters.next() {
            if character != '%' {
                expanded.push(character);
                continue;
            }
            let code = characters
                .next()
                .ok_or_else(|| "Exec= ends with an incomplete field code".to_owned())?;
            match code {
                '%' => expanded.push('%'),
                'f' | 'u' => file_code_count += 1,
                'F' | 'U' | 'i' => {
                    return Err(format!("Exec= %{code} must be an argument on its own"));
                }
                'd' | 'D' | 'n' | 'N' | 'v' | 'm' => {}
                'c' => expanded.push_str(name),
                'k' => expanded.push_str(&desktop_path),
                other if other.is_ascii_alphabetic() => {
                    return Err(format!("Exec= contains unsupported field code %{other}"));
                }
                other => {
                    return Err(format!("Exec= contains invalid field code %{other}"));
                }
            }
        }
        if !expanded.is_empty() {
            argv.push(expanded);
        }
    }

    if file_code_count > 1 {
        return Err("Exec= contains more than one file/URL field code".into());
    }
    Ok(argv)
}

fn contains_field_code(input: &str) -> bool {
    let mut characters = input.chars();
    while let Some(character) = characters.next() {
        if character != '%' {
            continue;
        }
        match characters.next() {
            Some('%') => {}
            Some(code) if code.is_ascii_alphabetic() => return true,
            Some(_) | None => {}
        }
    }
    false
}

fn concise(message: &str) -> String {
    let single_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    const LIMIT: usize = 120;
    if single_line.chars().count() <= LIMIT {
        return single_line;
    }
    let mut shortened = single_line.chars().take(LIMIT).collect::<String>();
    shortened.push('…');
    shortened
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(name: &str) -> Self {
            let nonce = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("cosmix-tray-{name}-{}-{nonce}", std::process::id()));
            fs::create_dir(&path).expect("create fixture directory");
            Self(path)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture_path() -> PathBuf {
        PathBuf::from("/usr/local/share/applications/dev.cosmix.example.desktop")
    }

    #[test]
    fn extracts_name_and_exec_argv_from_desktop_fixture() {
        let fixture = r#"
            [Desktop Entry]
            Type=Application
            Name=CosMix Example
            Name[de]=CosMix Beispiel
            Exec=/opt/cosmix/bin/cosmix-example --title "Two words" %U

            [Desktop Action Other]
            Name=Wrong section
            Exec=/bin/false
        "#;
        assert_eq!(
            parse_desktop_entry(fixture, &fixture_path()),
            Ok(ParsedDesktopEntry {
                name: "CosMix Example".into(),
                icon: None,
                argv: vec![
                    "/opt/cosmix/bin/cosmix-example".into(),
                    "--title".into(),
                    "Two words".into(),
                ],
            })
        );
    }

    #[test]
    fn exec_parser_applies_double_quote_escape_rules() {
        let fixture = r##"
            [Desktop Entry]
            Name=Escapes
            Exec="/opt/My App/bin/run" "price \\$5" "slash \\\\" "quote \\"ok\\"" "it's quoted"
        "##;
        let parsed = parse_desktop_entry(fixture, &fixture_path()).expect("valid fixture");
        assert_eq!(
            parsed.argv,
            vec![
                "/opt/My App/bin/run",
                "price $5",
                "slash \\",
                "quote \"ok\"",
                "it's quoted",
            ]
        );
    }

    #[test]
    fn expands_and_strips_freedesktop_field_codes() {
        let fixture = r#"
            [Desktop Entry]
            Name=CosMix\sExample
            Icon=network-workgroup
            Exec=/opt/cosmix/bin/example %i %c %k %f %d %D %n %N %v %m 50%%
        "#;
        let path = fixture_path();
        let parsed = parse_desktop_entry(fixture, &path).expect("valid fixture");
        assert_eq!(parsed.icon.as_deref(), Some("network-workgroup"));
        assert_eq!(
            parsed.argv,
            vec![
                "/opt/cosmix/bin/example",
                "--icon",
                "network-workgroup",
                "CosMix Example",
                path.to_str().expect("UTF-8 fixture path"),
                "50%",
            ]
        );
    }

    #[test]
    fn strips_each_file_url_and_deprecated_field_code_without_input() {
        let path = fixture_path();
        for code in ["%f", "%F", "%u", "%U", "%d", "%D", "%n", "%N", "%v", "%m"] {
            let fixture =
                format!("[Desktop Entry]\nName=Codes\nExec=/bin/echo before {code} after\n");
            let parsed = parse_desktop_entry(&fixture, &path).expect("valid field-code fixture");
            assert_eq!(parsed.argv, vec!["/bin/echo", "before", "after"]);
        }
    }

    #[test]
    fn rejects_invalid_exec_quoting_and_field_codes() {
        let path = fixture_path();
        assert!(
            parse_desktop_entry("[Desktop Entry]\nName=Single\nExec='/bin/echo'\n", &path).is_err()
        );
        assert!(parse_desktop_entry(
            "[Desktop Entry]\nName=Quoted code\nExec=/bin/echo \"%c\"\n",
            &path
        )
        .is_err());
        assert!(
            parse_desktop_entry("[Desktop Entry]\nName=Unknown\nExec=/bin/echo %x\n", &path)
                .is_err()
        );
        assert!(parse_desktop_entry(
            "[Desktop Entry]\nName=Too many\nExec=/bin/echo %f %U\n",
            &path
        )
        .is_err());
    }

    #[test]
    fn malformed_desktop_entries_are_errors() {
        let path = fixture_path();
        assert!(parse_desktop_entry("[Desktop Entry]\nName=No command\n", &path).is_err());
        assert!(parse_desktop_entry("[Desktop Entry]\nExec=/bin/true\n", &path).is_err());
        assert!(parse_desktop_entry(
            "[Desktop Entry]\nName=Bad quote\nExec=/bin/echo \"unterminated\n",
            &path
        )
        .is_err());
    }

    #[test]
    fn discovery_reads_only_regular_files_and_caps_file_count() {
        let root = TestRoot::new("bounded-discovery");
        fs::create_dir(root.0.join("dev.cosmix.directory.desktop"))
            .expect("create non-file fixture");
        for index in 0..MAX_DESKTOP_FILES + 1 {
            let path = root.0.join(format!("dev.cosmix.app{index:02}.desktop"));
            fs::write(
                path,
                format!(
                    "[Desktop Entry]\nName=App {index:02}\nIcon=dev.cosmix.app{index:02}\nExec=/opt/cosmix/bin/app{index:02}\n"
                ),
            )
            .expect("write desktop fixture");
        }

        let apps = discover_in(&root.0).expect("discover fixtures");
        assert_eq!(apps.len(), MAX_DESKTOP_FILES);
        assert!(apps.iter().all(|app| app.argv.is_some()));
        assert!(apps
            .iter()
            .all(|app| app.icon.as_deref() == Some(format!("dev.cosmix.{}", app.slug).as_str())));
    }

    #[test]
    fn desktop_file_reads_are_byte_bounded() {
        let root = TestRoot::new("bounded-read");
        let path = root.0.join("dev.cosmix.large.desktop");
        fs::write(&path, vec![b'x'; MAX_DESKTOP_BYTES + 1]).expect("write oversized fixture");
        assert!(read_bounded(&path).is_err());
    }
}
