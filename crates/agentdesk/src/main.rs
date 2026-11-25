//! agentdesk：面向 Agent 的无头 Wayland 合成器。
//!
//! 组装：Wayland 服务端（smithay）+ 虚拟输出 + 控制平面（ad-ipc）+ calloop 主循环。
//! 架构见 docs/DESIGN.md §4；本里程碑（M0/M1）不含渲染管线与查看器。

mod config;
mod control;
mod input;
mod render;
mod state;
mod viewer;

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use calloop::timer::{TimeoutAction, Timer};
use clap::Parser;
use smithay::reexports::wayland_server::Display;
use smithay::wayland::socket::ListeningSocketSource;

use ad_ipc::server::{IpcServer, IpcServerConfig};

use crate::config::Config;
use crate::state::{AgentDesk, ClientState};

/// 帧回调节拍：headless 无 vblank，以 30Hz tick 作为呈现时钟。
const TICK: Duration = Duration::from_millis(33);

fn main() -> ExitCode {
    let cli = config::Cli::parse();
    let config = match Config::from_cli(cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("agentdesk: {e}");
            return ExitCode::from(2);
        }
    };
    let is_daemon_child = std::env::var_os("AGENTDESK_DAEMON_CHILD").is_some();
    if config.daemon && !is_daemon_child {
        // 父进程：拉起守护子进程、等就绪、打印 JSON、退出
        return match spawn_daemon(&config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("agentdesk: {e:#}");
                ExitCode::FAILURE
            }
        };
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    match run(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("agentdesk: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// `--daemon`：以自身重新 exec 的方式脱离终端（独立进程组、stdio 重定向到日志）。
/// 父进程轮询"控制 socket 可连接"作为就绪信号（stale socket 文件不会误报），然后退出。
fn spawn_daemon(config: &Config) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let log_path = config.log_path();
    if let Some(dir) = log_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.args(std::env::args_os().skip(1))
        .env("AGENTDESK_DAEMON_CHILD", "1")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // 独立进程组：终端关闭的 SIGHUP 不会送达
    cmd.process_group(0);
    let mut child = cmd.spawn().context("daemon 进程拉起失败")?;
    let pid = child.id();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait()? {
            anyhow::bail!(
                "daemon 启动失败（exit {status}）。日志尾部:\n{}",
                log_tail(&log_path, 5)
            );
        }
        // connect 成功 = IPC 监听已建立（stale socket 文件在此处会连接失败，不会误报）
        if ad_ipc::client::Client::connect(&config.socket_path).is_ok() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "daemon 未在时限内就绪，已终止；日志: {}",
                log_path.display()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    println!(
        "{}",
        serde_json::json!({
            "daemon": true,
            "pid": pid,
            "socket": config.socket_path.display().to_string(),
            "wayland_display": config.wayland_socket_name,
            "log": log_path.display().to_string(),
        })
    );
    use std::io::Write;
    let _ = std::io::stdout().flush();
    Ok(())
}

fn log_tail(path: &std::path::Path, n: usize) -> String {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

fn run(config: Config) -> anyhow::Result<()> {
    let mut display: Display<AgentDesk> = Display::new()?;
    let dh = display.handle();

    let (ipc_server, hub) = IpcServer::bind(IpcServerConfig {
        path: config.socket_path.clone(),
        token: config.token.clone(),
        ..Default::default()
    })?;

    // 查看器线程 → 主循环 的事件通道
    let (viewer_events_tx, viewer_events_rx) = calloop::channel::channel::<viewer::ViewerEvent>();
    let (clipboard_results_tx, clipboard_results_rx) =
        calloop::channel::channel::<state::ClipboardResult>();

    let mut state = AgentDesk::new(&dh, &config, hub, viewer_events_tx, clipboard_results_tx)?;

    let mut event_loop: calloop::EventLoop<AgentDesk> = calloop::EventLoop::try_new()?;
    state.loop_signal = Some(event_loop.get_signal());
    let loop_handle = event_loop.handle();

    loop_handle
        .insert_source(
            viewer_events_rx,
            |event, _, state: &mut AgentDesk| match event {
                calloop::channel::Event::Msg(ev) => control::handle_viewer_event(state, ev),
                calloop::channel::Event::Closed => {}
            },
        )
        .map_err(|_e| anyhow::anyhow!("挂载查看器事件 source 失败"))?;

    loop_handle
        .insert_source(clipboard_results_rx, |event, _, state: &mut AgentDesk| {
            if let calloop::channel::Event::Msg(result) = event {
                let outcome = result
                    .result
                    .map(|text| serde_json::json!({"text":text}))
                    .map_err(|error| {
                        ad_ipc::error::RpcError::new(ad_ipc::error::codes::INTERNAL, error)
                    });
                state
                    .hub
                    .respond(result.conn_id, Some(result.request_id), outcome);
            }
        })
        .map_err(|_e| anyhow::anyhow!("挂载剪贴板读取结果 source 失败"))?;

    // 控制平面请求 → 主循环
    loop_handle
        .insert_source(
            ipc_server.channel,
            |event, _, state: &mut AgentDesk| match event {
                calloop::channel::Event::Msg(req) => control::handle_ipc(state, req),
                calloop::channel::Event::Closed => {}
            },
        )
        .map_err(|_e| anyhow::anyhow!("挂载 IPC channel source 失败"))?;

    // Wayland 客户端接入
    let socket_source = ListeningSocketSource::with_name(&config.wayland_socket_name)?;
    loop_handle
        .insert_source(socket_source, |stream, _, state: &mut AgentDesk| {
            let _ = state
                .dh
                .insert_client(stream, Arc::new(ClientState::default()));
        })
        .map_err(|_e| anyhow::anyhow!("挂载 Wayland socket source 失败"))?;

    // 呈现时钟（帧回调 + 退出处理）
    loop_handle
        .insert_source(Timer::from_duration(TICK), |_, _, state: &mut AgentDesk| {
            state.tick();
            TimeoutAction::ToDuration(TICK)
        })
        .map_err(|_e| anyhow::anyhow!("挂载定时器 source 失败"))?;

    if config.print_info {
        println!(
            "{}",
            serde_json::json!({
                "name": "agentdesk",
                "version": env!("CARGO_PKG_VERSION"),
                "api_version": ad_ipc::API_VERSION,
                "pid": std::process::id(),
                "socket": config.socket_path.display().to_string(),
                "wayland_display": config.wayland_socket_name,
            })
        );
        use std::io::Write;
        let _ = std::io::stdout().flush();
    }

    if config.daemon {
        std::fs::write(config.pid_path(), std::process::id().to_string())?;
    }

    tracing::info!(
        instance = %config.instance,
        wayland_display = %config.wayland_socket_name,
        socket = %config.socket_path.display(),
        size = %(format!("{}x{}", config.width, config.height)),
        "agentdesk 已启动"
    );

    let ipc_handle = ipc_server.handle;
    let result = event_loop.run(None, &mut state, move |state| {
        let _ = display.dispatch_clients(state);
        let _ = display.flush_clients();
    });
    ipc_handle.shutdown();
    if config.daemon {
        let _ = std::fs::remove_file(config.pid_path());
    }
    tracing::info!("agentdesk 已退出");
    result.map_err(|e| anyhow::anyhow!("事件循环错误: {e}"))
}
