//! 启动配置（CLI → 运行时配置）。

use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "agentdesk", version, about = "面向 AI Agent 的无头 Wayland 合成器")]
pub struct Cli {
    /// 实例名（决定默认 socket 路径与 Wayland socket 名）
    #[arg(long, default_value = "main")]
    pub instance: String,

    /// 主虚拟输出尺寸，如 1920x1080
    #[arg(long, default_value = "1920x1080")]
    pub size: String,

    /// 控制 socket 路径（默认 $XDG_RUNTIME_DIR/agentdesk/<instance>.sock）
    #[arg(long)]
    pub socket: Option<PathBuf>,

    /// 渲染器：auto | glow | pixman（M3 起生效，当前恒为 none）
    #[arg(long, default_value = "auto")]
    pub renderer: String,

    /// 可选访问令牌（要求客户端先发 auth）
    #[arg(long)]
    pub token: Option<String>,

    /// 约定 xwayland-satellite 使用的 X display 号（供 x11.status 与 windows.launch --x11）
    #[arg(long)]
    pub x11_display: Option<u32>,

    /// 以守护进程模式运行：脱离终端、日志/PID 落盘（见 --daemon-log）
    #[arg(short, long)]
    pub daemon: bool,

    /// 守护进程日志路径（默认 <socket 路径去掉扩展名>.log）
    #[arg(long)]
    pub daemon_log: Option<PathBuf>,

    /// 启动后向 stdout 打印一行 JSON（socket 路径等信息），便于脚本解析
    #[arg(long)]
    pub print_info: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RendererKind {
    Auto,
    Glow,
    Pixman,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub instance: String,
    pub width: i32,
    pub height: i32,
    pub socket_path: PathBuf,
    pub wayland_socket_name: String,
    pub renderer: RendererKind,
    pub token: Option<String>,
    pub print_info: bool,
    pub daemon: bool,
    pub daemon_log: Option<PathBuf>,
    pub x11_display: Option<u32>,
    /// 父会话类型（启动时探测）：wayland | x11 | None
    pub parent_session: Option<&'static str>,
}

impl Config {
    pub fn from_cli(cli: Cli) -> anyhow::Result<Self> {
        let (width, height) = parse_size(&cli.size)?;
        let renderer = match cli.renderer.as_str() {
            "auto" => RendererKind::Auto,
            "glow" => RendererKind::Glow,
            "pixman" => RendererKind::Pixman,
            other => anyhow::bail!("未知渲染器 {other}（可选 auto|glow|pixman）"),
        };
        let socket_path = cli
            .socket
            .clone()
            .or_else(|| ad_ipc::default_socket_path(&cli.instance))
            .ok_or_else(|| anyhow::anyhow!("无法推导 socket 路径：请设置 XDG_RUNTIME_DIR 或使用 --socket"))?;
        let parent_session = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            Some("wayland")
        } else if std::env::var_os("DISPLAY").is_some() {
            Some("x11")
        } else {
            None
        };
        let instance = cli.instance;
        let wayland_socket_name = format!("agentdesk-{instance}");
        Ok(Config {
            instance,
            width,
            height,
            socket_path,
            wayland_socket_name,
            renderer,
            token: cli.token,
            print_info: cli.print_info,
            daemon: cli.daemon,
            daemon_log: cli.daemon_log,
            x11_display: cli.x11_display,
            parent_session,
        })
    }

    /// 守护进程日志路径（默认与 socket 同名、扩展名 .log）。
    pub fn log_path(&self) -> PathBuf {
        self.daemon_log
            .clone()
            .unwrap_or_else(|| self.socket_path.with_extension("log"))
    }

    /// 守护进程 PID 文件路径（默认与 socket 同名、扩展名 .pid）。
    pub fn pid_path(&self) -> PathBuf {
        self.socket_path.with_extension("pid")
    }
}

pub fn parse_size(s: &str) -> anyhow::Result<(i32, i32)> {
    let (w, h) = s
        .split_once('x')
        .ok_or_else(|| anyhow::anyhow!("尺寸格式应为 WxH，得到 {s}"))?;
    let w: i32 = w.parse()?;
    let h: i32 = h.parse()?;
    if !(64..=16384).contains(&w) || !(64..=16384).contains(&h) {
        anyhow::bail!("尺寸超出 64..=16384：{w}x{h}");
    }
    Ok((w, h))
}
