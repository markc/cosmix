use std::sync::{Arc, Mutex};

use cosmic::app::Settings;
use cosmic::iced::{Length, Size};
use cosmic::widget::{button, column, container, row, text, text_input};
use cosmic::{executor, Core, Element};

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
}

struct CalcApp {
    core: Core,
    engine: Arc<Mutex<CalcEngine>>,
    display_cache: String,
    memory_cache: String,
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
        let port_handle = start_port(engine.clone());

        (
            Self {
                core,
                engine,
                display_cache: "0".into(),
                memory_cache: String::new(),
                _port_handle: port_handle,
            },
            cosmic::app::Task::none(),
        )
    }

    fn update(&mut self, message: Self::Message) -> cosmic::app::Task<Self::Message> {
        let mut eng = self.engine.lock().unwrap();
        match message {
            Msg::Digit(d) => eng.push(d),
            Msg::Op(op) => eng.push(op),
            Msg::Dot => eng.push('.'),
            Msg::Paren(p) => eng.push(p),
            Msg::Func(f) => eng.push_str(&format!("{f}(")),
            Msg::Equals => { eng.evaluate(None); }
            Msg::Clear => eng.clear(),
            Msg::ClearEntry => eng.clear_entry(),
            Msg::Backspace => eng.backspace(),
            Msg::Negate => eng.negate(),
            Msg::Percent => eng.push_str("/100"),
            Msg::MemClear => eng.mem_clear(),
            Msg::MemRecall => eng.mem_recall(),
            Msg::MemAdd => eng.mem_add(),
            Msg::MemSub => eng.mem_sub(),
            Msg::ExprInput(s) => {
                eng.expression = s;
                eng.display = eng.expression.clone();
            }
        }
        self.display_cache = eng.display.clone();
        self.memory_cache = if eng.memory != 0.0 {
            format!("M: {}", format_number(eng.memory))
        } else {
            String::new()
        };
        cosmic::app::Task::none()
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

        column::with_capacity(8)
            .push(display)
            .push(mem_row)
            .push(func_row)
            .push(row1)
            .push(row2)
            .push(row3)
            .push(row4)
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

fn start_port(engine: Arc<Mutex<CalcEngine>>) -> Option<cosmix_port::PortHandle> {
    let e1 = engine.clone();
    let e2 = engine.clone();
    let e3 = engine.clone();
    let e4 = engine.clone();
    let e5 = engine.clone();
    let e6 = engine.clone();

    let port = cosmix_port::Port::new("cosmix-calc")
        .command("calc", move |args| {
            let expr = args
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("expected string expression"))?;
            let mut eng = e1.lock().unwrap();
            let result = eng.evaluate(Some(expr));
            Ok(serde_json::json!(result))
        })
        .command("result", move |_| {
            let eng = e2.lock().unwrap();
            Ok(serde_json::json!(eng.display))
        })
        .command("clear", move |_| {
            let mut eng = e3.lock().unwrap();
            eng.clear();
            Ok(serde_json::json!("cleared"))
        })
        .command("history", move |_| {
            let eng = e4.lock().unwrap();
            let h: Vec<serde_json::Value> = eng
                .history
                .iter()
                .map(|(expr, result)| serde_json::json!({"expr": expr, "result": result}))
                .collect();
            Ok(serde_json::Value::Array(h))
        })
        .command("memory", move |args| {
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
        .command("press", move |args| {
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
                "=" | "enter" => { eng.evaluate(None); }
                "c" | "clear" => eng.clear(),
                "ce" => eng.clear_entry(),
                "backspace" => eng.backspace(),
                _ => return Err(anyhow::anyhow!("unknown key: {key}")),
            }
            Ok(serde_json::json!(eng.display))
        });

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

    let settings = Settings::default()
        .size(Size::new(380.0, 420.0));

    cosmic::app::run::<CalcApp>(settings, ())?;

    Ok(())
}
