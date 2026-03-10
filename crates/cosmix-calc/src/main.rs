use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use cosmic::app::Settings;
use cosmic::iced::stream;
use cosmic::iced::{self, Length, Size, Subscription};
use cosmic::widget::menu::{self, key_bind::Modifier, ItemHeight, ItemWidth, KeyBind};
use cosmic::widget::{button, column, container, icon, row, text, text_input};
use cosmic::iced::futures::SinkExt;
use cosmic::{executor, Core, Element};

type PortRx = Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<cosmix_port::PortEvent>>>;
static PORT_RX: OnceLock<PortRx> = OnceLock::new();

static MENU_ID: LazyLock<iced::id::Id> = LazyLock::new(|| iced::id::Id::new("calc_menu"));

// ── Menu Actions ──

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MenuAction {
    CopyResult,
    CopyExpression,
    Paste,
    ClearHistory,
    Quit,
    About,
    RunScript(usize),
    RescanScripts,
}

impl menu::Action for MenuAction {
    type Message = Msg;

    fn message(&self) -> Msg {
        match self {
            MenuAction::CopyResult => Msg::CopyResult,
            MenuAction::CopyExpression => Msg::CopyExpression,
            MenuAction::Paste => Msg::Paste,
            MenuAction::ClearHistory => Msg::ClearHistory,
            MenuAction::Quit => Msg::Quit,
            MenuAction::About => Msg::About,
            MenuAction::RunScript(i) => Msg::RunScript(*i),
            MenuAction::RescanScripts => Msg::RescanScripts,
        }
    }
}

fn key_binds() -> HashMap<KeyBind, MenuAction> {
    use iced::keyboard::{key::Named, Key};

    let mut kb = HashMap::new();
    macro_rules! bind {
        ([$($m:ident),+], $key:expr, $action:ident) => {
            kb.insert(KeyBind { modifiers: vec![$(Modifier::$m),+], key: $key }, MenuAction::$action);
        };
    }

    bind!([Ctrl], Key::Character("c".into()), CopyResult);
    bind!([Ctrl, Shift], Key::Character("c".into()), CopyExpression);
    bind!([Ctrl], Key::Character("v".into()), Paste);
    bind!([Ctrl], Key::Character("q".into()), Quit);
    bind!([Ctrl], Key::Named(Named::Delete), ClearHistory);

    kb
}

// ── Calculator Engine ──

#[derive(Clone)]
struct CalcEngine {
    expression: String,
    display: String,
    history: Vec<(String, String)>,
    memory: f64,
}

impl CalcEngine {
    fn new() -> Self {
        Self {
            expression: String::new(),
            display: "0".into(),
            history: Vec::new(),
            memory: 0.0,
        }
    }

    fn push(&mut self, ch: char) {
        if self.display == "0" && ch != '.' {
            self.expression.clear();
            self.display.clear();
        }
        self.expression.push(ch);
        self.display = self.expression.clone();
    }

    fn push_str(&mut self, s: &str) {
        if self.display == "0" {
            self.expression.clear();
            self.display.clear();
        }
        self.expression.push_str(s);
        self.display = self.expression.clone();
    }

    fn evaluate(&mut self, expr: Option<&str>) -> String {
        let to_eval = expr.unwrap_or(&self.expression);
        if to_eval.is_empty() {
            return self.display.clone();
        }

        match meval::eval_str(to_eval) {
            Ok(result) => {
                let result_str = format_number(result);
                self.history.push((to_eval.to_string(), result_str.clone()));
                if expr.is_none() {
                    self.expression = result_str.clone();
                    self.display = result_str.clone();
                }
                result_str
            }
            Err(e) => {
                let err = format!("Error: {e}");
                if expr.is_none() {
                    self.display = err.clone();
                    self.expression.clear();
                }
                err
            }
        }
    }

    fn clear(&mut self) {
        self.expression.clear();
        self.display = "0".into();
    }

    fn clear_entry(&mut self) {
        if let Some(pos) = self.expression.rfind(|c: char| "+-*/^%(".contains(c)) {
            if pos == self.expression.len() - 1 {
                self.expression.pop();
            } else {
                self.expression.truncate(pos + 1);
            }
        } else {
            self.expression.clear();
        }
        self.display = if self.expression.is_empty() {
            "0".into()
        } else {
            self.expression.clone()
        };
    }

    fn backspace(&mut self) {
        self.expression.pop();
        self.display = if self.expression.is_empty() {
            "0".into()
        } else {
            self.expression.clone()
        };
    }

    fn negate(&mut self) {
        if self.expression.starts_with('-') {
            self.expression.remove(0);
        } else if !self.expression.is_empty() {
            self.expression.insert(0, '-');
        }
        self.display = self.expression.clone();
    }

    fn mem_add(&mut self) {
        if let Ok(v) = meval::eval_str(&self.expression) {
            self.memory += v;
        }
    }

    fn mem_sub(&mut self) {
        if let Ok(v) = meval::eval_str(&self.expression) {
            self.memory -= v;
        }
    }

    fn mem_recall(&mut self) {
        self.expression = format_number(self.memory);
        self.display = self.expression.clone();
    }

    fn mem_clear(&mut self) {
        self.memory = 0.0;
    }
}

fn format_number(n: f64) -> String {
    if n == n.floor() && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        let s = format!("{:.10}", n);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

// ── COSMIC App ──

#[derive(Debug, Clone)]
enum Msg {
    Digit(char),
    Op(char),
    Func(&'static str),
    Dot,
    Equals,
    Clear,
    ClearEntry,
    Backspace,
    Negate,
    Percent,
    Paren(char),
    MemClear,
    MemRecall,
    MemAdd,
    MemSub,
    ExprInput(String),
    // Menu
    CopyResult,
    CopyExpression,
    Paste,
    Pasted(Option<String>),
    ClearHistory,
    Quit,
    About,
    RunScript(usize),
    RescanScripts,
    Surface(cosmic::surface::Action),
    SyncFromPort,
    PortActivate,
    ScriptsUpdated(Vec<cosmix_port::ScriptInfo>),
}

struct CalcApp {
    core: Core,
    engine: Arc<Mutex<CalcEngine>>,
    keybinds: HashMap<KeyBind, MenuAction>,
    display_cache: String,
    memory_cache: String,
    history_count: usize,
    port_scripts: Vec<cosmix_port::ScriptInfo>,
    _port_handle: Option<cosmix_port::PortHandle>,
}

impl cosmic::Application for CalcApp {
    type Executor = executor::Default;
    type Flags = ();
    type Message = Msg;

    const APP_ID: &'static str = "org.cosmix.Calc";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(mut core: Core, _flags: Self::Flags) -> (Self, cosmic::app::Task<Self::Message>) {
        core.window.header_title = "Cosmix Calc".into();
        core.window.content_container = true;

        let engine = Arc::new(Mutex::new(CalcEngine::new()));
        let (port_tx, port_rx) = tokio::sync::mpsc::unbounded_channel();
        PORT_RX.set(Arc::new(tokio::sync::Mutex::new(port_rx))).ok();
        let port_handle = start_port(engine.clone(), port_tx);

        (
            Self {
                core,
                engine,
                keybinds: key_binds(),
                display_cache: "0".into(),
                memory_cache: String::new(),
                history_count: 0,
                port_scripts: Vec::new(),
                _port_handle: port_handle,
            },
            cosmic::app::Task::none(),
        )
    }

    fn header_start(&self) -> Vec<Element<'_, Self::Message>> {
        let mut menus = vec![(
            "File".into(),
            vec![
                menu::Item::Button(
                    "Copy Result",
                    Some(icon::from_name("edit-copy-symbolic").into()),
                    MenuAction::CopyResult,
                ),
                menu::Item::Button(
                    "Copy Expression",
                    None,
                    MenuAction::CopyExpression,
                ),
                menu::Item::Button(
                    "Paste",
                    Some(icon::from_name("edit-paste-symbolic").into()),
                    MenuAction::Paste,
                ),
                menu::Item::Divider,
                menu::Item::Button(
                    "Clear History",
                    Some(icon::from_name("edit-clear-all-symbolic").into()),
                    MenuAction::ClearHistory,
                ),
                menu::Item::Divider,
                menu::Item::Button(
                    "About",
                    Some(icon::from_name("help-about-symbolic").into()),
                    MenuAction::About,
                ),
                menu::Item::Button(
                    "Quit",
                    Some(icon::from_name("window-close-symbolic").into()),
                    MenuAction::Quit,
                ),
            ],
        )];

        // Scripts menu (populated by daemon via __scripts__ port command)
        if !self.port_scripts.is_empty() {
            let mut script_items: Vec<menu::Item<MenuAction, &str>> = self.port_scripts
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    // Leak the string to get 'static lifetime (menu items need &str)
                    let name: &str = Box::leak(s.display_name.clone().into_boxed_str());
                    menu::Item::Button(
                        name,
                        Some(icon::from_name("text-x-script-symbolic").into()),
                        MenuAction::RunScript(i),
                    )
                })
                .collect();
            script_items.push(menu::Item::Divider);
            script_items.push(menu::Item::Button(
                "Rescan Scripts",
                Some(icon::from_name("view-refresh-symbolic").into()),
                MenuAction::RescanScripts,
            ));
            menus.push(("Scripts".into(), script_items));
        }

        vec![cosmic::widget::responsive_menu_bar()
            .item_height(ItemHeight::Dynamic(40))
            .item_width(ItemWidth::Uniform(240))
            .spacing(4.0)
            .into_element(
                self.core(),
                &self.keybinds,
                MENU_ID.clone(),
                Msg::Surface,
                menus,
            )]
    }

    fn update(&mut self, message: Self::Message) -> cosmic::app::Task<Self::Message> {
        match message {
            // Calculator buttons
            Msg::Digit(d) => self.engine.lock().unwrap().push(d),
            Msg::Op(op) => self.engine.lock().unwrap().push(op),
            Msg::Dot => self.engine.lock().unwrap().push('.'),
            Msg::Paren(p) => self.engine.lock().unwrap().push(p),
            Msg::Func(f) => self.engine.lock().unwrap().push_str(&format!("{f}(")),
            Msg::Equals => {
                self.engine.lock().unwrap().evaluate(None);
            }
            Msg::Clear => self.engine.lock().unwrap().clear(),
            Msg::ClearEntry => self.engine.lock().unwrap().clear_entry(),
            Msg::Backspace => self.engine.lock().unwrap().backspace(),
            Msg::Negate => self.engine.lock().unwrap().negate(),
            Msg::Percent => self.engine.lock().unwrap().push_str("/100"),
            Msg::MemClear => self.engine.lock().unwrap().mem_clear(),
            Msg::MemRecall => self.engine.lock().unwrap().mem_recall(),
            Msg::MemAdd => self.engine.lock().unwrap().mem_add(),
            Msg::MemSub => self.engine.lock().unwrap().mem_sub(),
            Msg::ExprInput(s) => {
                let mut eng = self.engine.lock().unwrap();
                eng.expression = s;
                eng.display = eng.expression.clone();
            }
            // Menu actions
            Msg::CopyResult => {
                let eng = self.engine.lock().unwrap();
                let result = eng.display.clone();
                drop(eng);
                return cosmic::app::Task::perform(
                    async move {
                        let mut child = tokio::process::Command::new("wl-copy")
                            .stdin(std::process::Stdio::piped())
                            .spawn()
                            .ok()?;
                        if let Some(mut stdin) = child.stdin.take() {
                            use tokio::io::AsyncWriteExt;
                            stdin.write_all(result.as_bytes()).await.ok()?;
                        }
                        child.wait().await.ok()?;
                        Some(())
                    },
                    |_| cosmic::Action::App(Msg::Pasted(None)),
                );
            }
            Msg::CopyExpression => {
                let eng = self.engine.lock().unwrap();
                let expr = eng.expression.clone();
                drop(eng);
                return cosmic::app::Task::perform(
                    async move {
                        let mut child = tokio::process::Command::new("wl-copy")
                            .stdin(std::process::Stdio::piped())
                            .spawn()
                            .ok()?;
                        if let Some(mut stdin) = child.stdin.take() {
                            use tokio::io::AsyncWriteExt;
                            stdin.write_all(expr.as_bytes()).await.ok()?;
                        }
                        child.wait().await.ok()?;
                        Some(())
                    },
                    |_| cosmic::Action::App(Msg::About),
                );
            }
            Msg::Paste => {
                return cosmic::app::Task::perform(
                    async {
                        let output = tokio::process::Command::new("wl-paste")
                            .arg("--no-newline")
                            .output()
                            .await
                            .ok()?;
                        String::from_utf8(output.stdout).ok()
                    },
                    |result| cosmic::Action::App(Msg::Pasted(result)),
                );
            }
            Msg::Pasted(Some(text)) => {
                let cleaned: String = text
                    .chars()
                    .filter(|c| c.is_ascii_digit() || "+-*/^%.()".contains(*c))
                    .collect();
                if !cleaned.is_empty() {
                    let mut eng = self.engine.lock().unwrap();
                    eng.push_str(&cleaned);
                }
            }
            Msg::Pasted(None) => {}
            Msg::ClearHistory => {
                self.engine.lock().unwrap().history.clear();
            }
            Msg::Quit => return cosmic::iced::exit(),
            Msg::About => {
                return cosmic::app::Task::perform(
                    async {
                        let cosmic_ver = tokio::process::Command::new("cosmic-comp")
                            .arg("--version")
                            .output()
                            .await
                            .ok()
                            .and_then(|o| String::from_utf8(o.stdout).ok())
                            .unwrap_or_else(|| "unknown".into());
                        let body = format!(
                            "Cosmix Calc v{}\nScientific calculator for COSMIC\n\nCOSMIC: {}\nPort: cosmix-calc\nLicense: MIT",
                            env!("CARGO_PKG_VERSION"),
                            cosmic_ver.trim(),
                        );
                        let _ = tokio::process::Command::new("notify-send")
                            .arg("--app-name=Cosmix Calc")
                            .arg("--icon=accessories-calculator")
                            .arg("--expire-time=0")
                            .arg("Cosmix Calc")
                            .arg(&body)
                            .status()
                            .await;
                    },
                    |_| cosmic::Action::App(Msg::Pasted(None)), // no-op
                );
            }
            Msg::Surface(a) => {
                return cosmic::task::message(cosmic::Action::Cosmic(
                    cosmic::app::Action::Surface(a),
                ));
            }
            Msg::RunScript(i) => {
                if let Some(script) = self.port_scripts.get(i) {
                    let path = script.path.clone();
                    return cosmic::app::Task::perform(
                        async move {
                            let _ = tokio::process::Command::new("cosmix")
                                .args(["run-for", &path, "COSMIX-CALC.1"])
                                .output()
                                .await;
                        },
                        |_| cosmic::Action::App(Msg::Pasted(None)),
                    );
                }
            }
            Msg::RescanScripts => {
                return cosmic::app::Task::perform(
                    async {
                        let _ = tokio::process::Command::new("cosmix")
                            .args(["rescan-scripts", "COSMIX-CALC.1"])
                            .output()
                            .await;
                    },
                    |_| cosmic::Action::App(Msg::Pasted(None)),
                );
            }
            Msg::SyncFromPort => {
                // Port event received — fall through to update caches
            }
            Msg::PortActivate => {
                // Bring window to front — fall through to update caches
            }
            Msg::ScriptsUpdated(scripts) => {
                self.port_scripts = scripts;
            }
        }

        // Update caches
        let eng = self.engine.lock().unwrap();
        self.display_cache = eng.display.clone();
        self.memory_cache = if eng.memory != 0.0 {
            format!("M: {}", format_number(eng.memory))
        } else {
            String::new()
        };
        self.history_count = eng.history.len();
        cosmic::app::Task::none()
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        Subscription::run(|| {
            stream::channel(16, |mut output: cosmic::iced::futures::channel::mpsc::Sender<_>| async move {
                let rx = PORT_RX.get().expect("port receiver not initialized");
                loop {
                    match rx.lock().await.recv().await {
                        Some(cosmix_port::PortEvent::Activate) => {
                            let _ = output.send(Msg::PortActivate).await;
                        }
                        Some(cosmix_port::PortEvent::ScriptsUpdated(scripts)) => {
                            let _ = output.send(Msg::ScriptsUpdated(scripts)).await;
                        }
                        Some(cosmix_port::PortEvent::Command { .. }) => {
                            let _ = output.send(Msg::SyncFromPort).await;
                        }
                        None => {
                            std::future::pending::<()>().await;
                        }
                    }
                }
            })
        })
    }

    fn view(&self) -> Element<'_, Self::Message> {
        let display = container(
            column::with_capacity(2)
                .push(
                    text_input("0", &self.display_cache)
                        .on_input(Msg::ExprInput)
                        .on_submit(|_| Msg::Equals)
                        .size(28)
                        .padding(8),
                )
                .push(text::body(&self.memory_cache).size(11))
                .spacing(2),
        )
        .padding([8, 8])
        .width(Length::Fill);

        let mem_row = btn_row(vec![
            ("MC", BtnStyle::Mem, Msg::MemClear),
            ("MR", BtnStyle::Mem, Msg::MemRecall),
            ("M+", BtnStyle::Mem, Msg::MemAdd),
            ("M\u{2212}", BtnStyle::Mem, Msg::MemSub),
            ("\u{232b}", BtnStyle::Op, Msg::Backspace),
            ("C", BtnStyle::Op, Msg::Clear),
        ]);

        let func_row = btn_row(vec![
            ("sin", BtnStyle::Func, Msg::Func("sin")),
            ("cos", BtnStyle::Func, Msg::Func("cos")),
            ("tan", BtnStyle::Func, Msg::Func("tan")),
            ("\u{221a}", BtnStyle::Func, Msg::Func("sqrt")),
            ("ln", BtnStyle::Func, Msg::Func("ln")),
            ("log", BtnStyle::Func, Msg::Func("log10")),
        ]);

        let row1 = btn_row(vec![
            ("7", BtnStyle::Digit, Msg::Digit('7')),
            ("8", BtnStyle::Digit, Msg::Digit('8')),
            ("9", BtnStyle::Digit, Msg::Digit('9')),
            ("\u{00f7}", BtnStyle::Op, Msg::Op('/')),
            ("(", BtnStyle::Op, Msg::Paren('(')),
            (")", BtnStyle::Op, Msg::Paren(')')),
        ]);

        let row2 = btn_row(vec![
            ("4", BtnStyle::Digit, Msg::Digit('4')),
            ("5", BtnStyle::Digit, Msg::Digit('5')),
            ("6", BtnStyle::Digit, Msg::Digit('6')),
            ("\u{00d7}", BtnStyle::Op, Msg::Op('*')),
            ("^", BtnStyle::Op, Msg::Op('^')),
            ("%", BtnStyle::Op, Msg::Percent),
        ]);

        let row3 = btn_row(vec![
            ("1", BtnStyle::Digit, Msg::Digit('1')),
            ("2", BtnStyle::Digit, Msg::Digit('2')),
            ("3", BtnStyle::Digit, Msg::Digit('3')),
            ("\u{2212}", BtnStyle::Op, Msg::Op('-')),
            ("CE", BtnStyle::Op, Msg::ClearEntry),
            ("\u{00b1}", BtnStyle::Op, Msg::Negate),
        ]);

        let row4 = btn_row(vec![
            ("0", BtnStyle::Digit, Msg::Digit('0')),
            (".", BtnStyle::Digit, Msg::Dot),
            ("\u{03c0}", BtnStyle::Func, Msg::Func("pi*")),
            ("+", BtnStyle::Op, Msg::Op('+')),
            ("=", BtnStyle::Equals, Msg::Equals),
            ("=", BtnStyle::Equals, Msg::Equals),
        ]);

        // Status bar with history count
        let status = if self.history_count > 0 {
            format!("{} calculations", self.history_count)
        } else {
            String::new()
        };

        column::with_capacity(9)
            .push(display)
            .push(mem_row)
            .push(func_row)
            .push(row1)
            .push(row2)
            .push(row3)
            .push(row4)
            .push(text::caption(status))
            .spacing(8)
            .padding(12)
            .into()
    }
}

#[derive(Debug, Clone, Copy)]
enum BtnStyle {
    Digit,
    Op,
    Func,
    Mem,
    Equals,
}

fn btn_row(buttons: Vec<(&str, BtnStyle, Msg)>) -> Element<'static, Msg> {
    let mut r = row::with_capacity(buttons.len()).spacing(8);
    for (label, style, msg) in buttons {
        r = r.push(calc_button(label, style, msg));
    }
    r.width(Length::Fill).into()
}

fn calc_button(label: &str, style: BtnStyle, msg: Msg) -> Element<'static, Msg> {
    let label = label.to_string();
    let btn = match style {
        BtnStyle::Equals => button::suggested(label),
        BtnStyle::Op => button::standard(label),
        BtnStyle::Func => button::text(label),
        BtnStyle::Mem => button::text(label),
        BtnStyle::Digit => button::standard(label),
    }
    .on_press(msg)
    .width(Length::Fill);

    container(btn)
        .width(Length::FillPortion(1))
        .center_x(Length::Fill)
        .into()
}

// ── Cosmix Port Integration ──

fn start_port(
    engine: Arc<Mutex<CalcEngine>>,
    notifier: tokio::sync::mpsc::UnboundedSender<cosmix_port::PortEvent>,
) -> Option<cosmix_port::PortHandle> {
    let e1 = engine.clone();
    let e2 = engine.clone();
    let e3 = engine.clone();
    let e4 = engine.clone();
    let e5 = engine.clone();
    let e6 = engine.clone();

    let port = cosmix_port::Port::new("cosmix-calc")
        .events(notifier)
        .command("calc", "Evaluate a mathematical expression", move |args| {
            let expr = args
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("expected string expression"))?;
            let mut eng = e1.lock().unwrap();
            eng.expression = expr.to_string();
            eng.display = expr.to_string();
            let result = eng.evaluate(None);
            Ok(serde_json::json!(result))
        })
        .command("result", "Get the current display value", move |_| {
            let eng = e2.lock().unwrap();
            Ok(serde_json::json!(eng.display))
        })
        .command("clear", "Clear the calculator", move |_| {
            let mut eng = e3.lock().unwrap();
            eng.clear();
            Ok(serde_json::json!("cleared"))
        })
        .command("history", "Get calculation history", move |_| {
            let eng = e4.lock().unwrap();
            let h: Vec<serde_json::Value> = eng
                .history
                .iter()
                .map(|(expr, result)| serde_json::json!({"expr": expr, "result": result}))
                .collect();
            Ok(serde_json::Value::Array(h))
        })
        .command("memory", "Memory operations: add/sub/recall/clear", move |args| {
            let op = args.as_str().unwrap_or("recall");
            let mut eng = e5.lock().unwrap();
            match op {
                "add" | "+" => eng.mem_add(),
                "sub" | "-" => eng.mem_sub(),
                "recall" | "mr" => eng.mem_recall(),
                "clear" | "mc" => eng.mem_clear(),
                _ => return Err(anyhow::anyhow!("unknown memory op: {op}")),
            }
            Ok(serde_json::json!(format_number(eng.memory)))
        })
        .command("press", "Simulate a button press", move |args| {
            let key = args
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("expected key string"))?;
            let mut eng = e6.lock().unwrap();
            match key {
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" => {
                    eng.push(key.chars().next().unwrap());
                }
                "+" | "-" | "*" | "/" | "^" => {
                    eng.push(key.chars().next().unwrap());
                }
                "." => eng.push('.'),
                "=" | "enter" => {
                    eng.evaluate(None);
                }
                "c" | "clear" => eng.clear(),
                "ce" => eng.clear_entry(),
                "backspace" => eng.backspace(),
                _ => return Err(anyhow::anyhow!("unknown key: {key}")),
            }
            Ok(serde_json::json!(eng.display))
        })
        .standard_help()
        .standard_info("Cosmix Calc", env!("CARGO_PKG_VERSION"))
        .standard_activate();

    match port.start() {
        Ok(handle) => {
            tracing::info!("Cosmix port started at {}", handle.socket_path.display());
            Some(handle)
        }
        Err(e) => {
            tracing::warn!("Failed to start cosmix port: {e}");
            None
        }
    }
}

// ── Main ──

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("cosmix_calc=info")
        .init();

    let settings = Settings::default().size(Size::new(380.0, 460.0));

    cosmic::app::run::<CalcApp>(settings, ())?;

    Ok(())
}
