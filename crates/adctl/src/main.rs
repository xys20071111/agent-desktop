//! adctl：agent-desktop 的命令行控制客户端。

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use serde_json::{json, Value};

use ad_ipc::client::{Client, ClientError};
use ad_ipc::protocol::Event;
use ad_ipc::{default_socket_path, DEFAULT_INSTANCE};

#[derive(Debug, Parser)]
#[command(name = "agdctl", version, about = "agent-desktop 控制客户端")]
struct Cli {
    /// 控制 socket 路径（默认 $AGENTDESK_SOCKET 或 $XDG_RUNTIME_DIR/agentdesk/main.sock）
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    /// 输出原始 JSON 而非人类可读格式
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Parser)]
enum Cmd {
    /// 服务器信息
    Info,
    Ping,
    Shutdown,
    /// 用令牌鉴权（服务器以 --token 启动时必须先调用）
    Auth { token: String },

    Windows {
        #[command(subcommand)]
        cmd: WindowsCmd,
    },
    /// 启动应用（连入 agentdesk；--x11 时改接 X11 display）
    Launch {
        #[arg(long)]
        x11: bool,
        #[arg(long = "wait", value_name = "MS")]
        wait: Option<u64>,
        #[arg(long = "env", value_name = "K=V")]
        envs: Vec<String>,
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
    /// 截图：<id> 为窗口截图，"screen" 为整屏
    Screenshot {
        target: String,
        #[arg(short, long)]
        out: Option<PathBuf>,
        #[arg(long)]
        format: Option<String>,
        #[arg(long)]
        quality: Option<u8>,
        #[arg(long)]
        cursor: bool,
    },

    Click {
        x: f64,
        y: f64,
        #[arg(long, default_value = "left")]
        button: String,
        #[arg(long, default_value = "1")]
        count: u32,
    },
    Move { x: f64, y: f64 },
    #[command(name = "move-rel")]
    MoveRel { dx: f64, dy: f64 },
    Scroll {
        dy: f64,
        #[arg(long)]
        dx: Option<f64>,
        #[arg(long)]
        x: Option<f64>,
        #[arg(long)]
        y: Option<f64>,
    },
    Key { key: String },
    Combo { keys: Vec<String> },
    Type { text: String },

    /// 显示嵌套查看器窗口
    Show {
        #[arg(long, value_name = "WxH")]
        size: Option<String>,
        #[arg(long)]
        output: Option<String>,
    },
    /// 隐藏嵌套查看器窗口
    Hide,
    /// 查看器状态
    Status,

    /// 网格排列窗口
    Tile {
        #[arg(long, default_value = "grid")]
        direction: String,
        #[arg(long)]
        output: Option<String>,
    },
    /// 级联排列窗口
    Cascade {
        #[arg(long)]
        output: Option<String>,
    },

    Clipboard {
        #[command(subcommand)]
        cmd: ClipboardCmd,
    },
    /// 订阅并打印事件流（默认全部）
    Watch { patterns: Vec<String> },
    /// X11（xwayland-satellite）状态
    #[command(name = "x11")]
    X11,
}

#[derive(Debug, Parser)]
enum WindowsCmd {
    List {
        #[arg(long)]
        app_id: Option<String>,
        #[arg(long)]
        title_contains: Option<String>,
        #[arg(long)]
        origin: Option<String>,
        #[arg(long)]
        output: Option<String>,
    },
    Get { id: u64 },
    Close { id: u64 },
    Focus { id: u64 },
    Raise { id: u64 },
    Geometry {
        id: u64,
        #[arg(long)]
        x: Option<i32>,
        #[arg(long)]
        y: Option<i32>,
        #[arg(long)]
        width: Option<i32>,
        #[arg(long)]
        height: Option<i32>,
    },
}

#[derive(Debug, Parser)]
enum ClipboardCmd {
    Get {
        #[arg(long, default_value = "regular")]
        which: String,
    },
    Set { text: String },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("agdctl: {e}");
            ExitCode::from(1)
        }
    }
}

fn socket_path(cli: &Cli) -> anyhow::Result<PathBuf> {
    if let Some(p) = &cli.socket {
        return Ok(p.clone());
    }
    if let Some(p) = std::env::var_os("AGENTDESK_SOCKET") {
        return Ok(PathBuf::from(p));
    }
    default_socket_path(DEFAULT_INSTANCE)
        .ok_or_else(|| anyhow::anyhow!("无法推导 socket 路径：请设置 XDG_RUNTIME_DIR 或 --socket"))
}

fn run(cli: Cli) -> Result<ExitCode, anyhow::Error> {
    let path = socket_path(&cli)?;
    let mut client = Client::connect(&path).map_err(|e| {
        anyhow::anyhow!("连接 {path:?} 失败：{e}（agentdesk 是否在运行？）")
    })?;

    let result = dispatch(&mut client, cli.cmd, cli.json);
    match result {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(ClientError::Rpc(e)) => {
            eprintln!("{}", serde_json::json!({"error": {"code": e.code, "message": e.message, "data": e.data}}));
            Ok(ExitCode::from(2))
        }
        Err(ClientError::Protocol(m)) => {
            eprintln!("agdctl: 协议错误: {m}");
            Ok(ExitCode::from(3))
        }
        Err(ClientError::Io(e)) => Err(anyhow::Error::new(e).context("传输错误")),
    }
}

fn dispatch(client: &mut Client, cmd: Cmd, json: bool) -> Result<(), ClientError> {
    let (method, params): (&str, Option<Value>) = match cmd {
        Cmd::Info => ("server.info", None),
        Cmd::Ping => ("server.ping", None),
        Cmd::Shutdown => ("server.shutdown", None),
        Cmd::Auth { token } => ("auth", Some(json!({ "token": token }))),

        Cmd::Windows { cmd } => match cmd {
            WindowsCmd::List { app_id, title_contains, origin, output } => {
                let mut filter = serde_json::Map::new();
                if let Some(v) = app_id {
                    filter.insert("app_id".into(), json!(v));
                }
                if let Some(v) = title_contains {
                    filter.insert("title_contains".into(), json!(v));
                }
                if let Some(v) = origin {
                    filter.insert("origin".into(), json!(v));
                }
                if let Some(v) = output {
                    filter.insert("output".into(), json!(v));
                }
                let params = if filter.is_empty() {
                    Value::Null
                } else {
                    json!({ "filter": Value::Object(filter) })
                };
                ("windows.list", Some(params))
            }
            WindowsCmd::Get { id } => ("windows.get", Some(json!({ "id": id }))),
            WindowsCmd::Close { id } => ("windows.close", Some(json!({ "id": id }))),
            WindowsCmd::Focus { id } => ("windows.focus", Some(json!({ "id": id }))),
            WindowsCmd::Raise { id } => ("windows.raise", Some(json!({ "id": id }))),
            WindowsCmd::Geometry { id, x, y, width, height } => {
                let mut p = serde_json::Map::new();
                p.insert("id".into(), json!(id));
                for (k, v) in [("x", x), ("y", y), ("width", width), ("height", height)] {
                    if let Some(v) = v {
                        p.insert(k.into(), json!(v));
                    }
                }
                ("windows.set_geometry", Some(Value::Object(p)))
            }
        },

        Cmd::Launch { x11, wait, envs, argv } => {
            let mut env_map = serde_json::Map::new();
            for e in envs {
                if let Some((k, v)) = e.split_once('=') {
                    env_map.insert(k.to_owned(), json!(v));
                }
            }
            let mut p = serde_json::Map::new();
            p.insert("argv".into(), json!(argv));
            if x11 {
                p.insert("x11".into(), json!(true));
            }
            if let Some(w) = wait {
                p.insert("wait_for_window_ms".into(), json!(w));
            }
            if !env_map.is_empty() {
                p.insert("env".into(), Value::Object(env_map));
            }
            ("windows.launch", Some(Value::Object(p)))
        }

        Cmd::Screenshot { target, out, format, quality, cursor } => {
            let mut p = serde_json::Map::new();
            if target.eq_ignore_ascii_case("screen") {
                if let Some(o) = out {
                    p.insert("to_path".into(), json!(o.display().to_string()));
                }
                if let Some(f) = format {
                    p.insert("format".into(), json!(f));
                }
                if let Some(q) = quality {
                    p.insert("quality".into(), json!(q));
                }
                if cursor {
                    p.insert("include_cursor".into(), json!(true));
                }
                ("screen.capture", Some(Value::Object(p)))
            } else {
                let id: u64 = target
                    .parse()
                    .map_err(|_| ClientError::Protocol(format!("截图目标应为窗口 id 或 screen，得到 {target}")))?;
                p.insert("id".into(), json!(id));
                if let Some(o) = out {
                    p.insert("to_path".into(), json!(o.display().to_string()));
                }
                if let Some(f) = format {
                    p.insert("format".into(), json!(f));
                }
                if cursor {
                    p.insert("include_cursor".into(), json!(true));
                }
                ("windows.screenshot", Some(Value::Object(p)))
            }
        }

        Cmd::Click { x, y, button, count } => (
            "input.click",
            Some(json!({ "x": x, "y": y, "button": button, "count": count })),
        ),
        Cmd::Move { x, y } => ("input.move_cursor", Some(json!({ "x": x, "y": y }))),
        Cmd::MoveRel { dx, dy } => ("input.move_relative", Some(json!({ "dx": dx, "dy": dy }))),
        Cmd::Scroll { dy, dx, x, y } => {
            let mut p = serde_json::Map::new();
            p.insert("dy".into(), json!(dy));
            if let Some(v) = dx {
                p.insert("dx".into(), json!(v));
            }
            if let Some(v) = x {
                p.insert("x".into(), json!(v));
            }
            if let Some(v) = y {
                p.insert("y".into(), json!(v));
            }
            ("input.scroll", Some(Value::Object(p)))
        }
        Cmd::Key { key } => ("input.key", Some(json!({ "key": key, "action": "tap" }))),
        Cmd::Combo { keys } => ("input.combo", Some(json!({ "keys": keys }))),
        Cmd::Type { text } => ("input.type_text", Some(json!({ "text": text }))),

        Cmd::Show { size, output } => {
            let mut p = serde_json::Map::new();
            if let Some(s) = size {
                let (w, h) = s
                    .split_once('x')
                    .ok_or_else(|| ClientError::Protocol(format!("尺寸应为 WxH，得到 {s}")))?;
                p.insert("width".into(), json!(w.parse::<u64>().map_err(|_| ClientError::Protocol("宽度解析失败".into()))?));
                p.insert("height".into(), json!(h.parse::<u64>().map_err(|_| ClientError::Protocol("高度解析失败".into()))?));
            }
            if let Some(o) = output {
                p.insert("output".into(), json!(o));
            }
            ("display.show", Some(Value::Object(p)))
        }
        Cmd::Hide => ("display.hide", None),
        Cmd::Status => ("display.status", None),

        Cmd::Tile { direction, output } => {
            let mut p = serde_json::Map::new();
            p.insert("direction".into(), json!(direction));
            if let Some(o) = output {
                p.insert("output".into(), json!(o));
            }
            ("layout.tile", Some(Value::Object(p)))
        }
        Cmd::Cascade { output } => {
            let mut p = serde_json::Map::new();
            if let Some(o) = output {
                p.insert("output".into(), json!(o));
            }
            ("layout.cascade", Some(Value::Object(p)))
        }

        Cmd::Clipboard { cmd } => match cmd {
            ClipboardCmd::Get { which } => ("clipboard.get", Some(json!({ "which": which }))),
            ClipboardCmd::Set { text } => ("clipboard.set", Some(json!({ "text": text }))),
        },

        Cmd::Watch { patterns } => {
            let types = if patterns.is_empty() { vec!["*".to_string()] } else { patterns };
            client.call("events.subscribe", json!({ "types": types }))?;
            loop {
                match client.try_next_event(Duration::from_secs(3600))? {
                    Some(ev) => println!("{}", event_line(&ev)),
                    None => continue,
                }
            }
        }
        Cmd::X11 => ("x11.status", None),
    };

    let result = match params {
        Some(p) => client.call(method, p)?,
        None => client.call0(method)?,
    };
    print_result(method, &result, json);
    Ok(())
}

fn event_line(ev: &Event) -> String {
    serde_json::to_string(ev).unwrap_or_default()
}

fn print_result(method: &str, result: &Value, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(result).unwrap_or_default());
        return;
    }
    match method {
        "windows.list" => {
            if let Some(wins) = result.get("windows").and_then(Value::as_array) {
                if wins.is_empty() {
                    println!("（无窗口）");
                }
                for w in wins {
                    println!("{}", window_line(w));
                }
            }
        }
        "windows.get" | "window.created" => println!("{}", window_line(result)),
        "screen.capture" | "windows.screenshot" => {
            let fmt = result.get("format").and_then(Value::as_str).unwrap_or("?");
            let w = result.get("width").and_then(Value::as_u64).unwrap_or(0);
            let h = result.get("height").and_then(Value::as_u64).unwrap_or(0);
            match result.get("path").and_then(Value::as_str) {
                Some(p) => println!("已写入 {p}（{fmt} {w}x{h}）"),
                None => println!("{} {w}x{h}（{} 字节 base64）", fmt, result.get("data").and_then(Value::as_str).map(str::len).unwrap_or(0)),
            }
        }
        "server.info" => {
            println!("name           = {}", result.get("name").and_then(Value::as_str).unwrap_or("?"));
            println!("version        = {}", result.get("version").and_then(Value::as_str).unwrap_or("?"));
            println!("api_version    = {}", result.get("api_version").and_then(Value::as_u64).unwrap_or(0));
            println!("pid            = {}", result.get("pid").and_then(Value::as_u64).unwrap_or(0));
            println!("socket         = {}", result.get("socket").and_then(Value::as_str).unwrap_or("?"));
            println!("wayland_display= {}", result.get("wayland_display").and_then(Value::as_str).unwrap_or("?"));
            println!("parent_session = {:?}", result.get("parent_session").and_then(Value::as_str));
            println!("renderer       = {}", result.get("renderer").and_then(Value::as_str).unwrap_or("?"));
            println!("viewer visible = {}", result.pointer("/viewer/visible").and_then(Value::as_bool).unwrap_or(false));
            println!("x11            = {}", serde_json::to_string(result.get("x11").unwrap_or(&Value::Null)).unwrap_or_default());
            if let Some(outputs) = result.get("outputs").and_then(Value::as_array) {
                for o in outputs {
                    println!(
                        "output         = {} {}x{} @{}",
                        o.get("id").and_then(Value::as_str).unwrap_or("?"),
                        o.get("width").and_then(Value::as_i64).unwrap_or(0),
                        o.get("height").and_then(Value::as_i64).unwrap_or(0),
                        serde_json::to_string(o.get("position").unwrap_or(&Value::Null)).unwrap_or_default()
                    );
                }
            }
        }
        _ => println!("{}", serde_json::to_string_pretty(result).unwrap_or_default()),
    }
}

fn window_line(w: &Value) -> String {
    let g = |k: &str| w.pointer(&format!("/geometry/{k}")).and_then(Value::as_i64).unwrap_or(0);
    format!(
        "#{id:<4} {app:<24} {origin:<8} {gw}x{gh}+{gx}+{gy} {title}",
        id = w.get("id").and_then(Value::as_u64).unwrap_or(0),
        app = w.get("app_id").and_then(Value::as_str).unwrap_or("-"),
        origin = w.get("origin").and_then(Value::as_str).unwrap_or("?"),
        gw = g("width"),
        gh = g("height"),
        gx = g("x"),
        gy = g("y"),
        title = w.get("title").and_then(Value::as_str).unwrap_or("-"),
    )
}
