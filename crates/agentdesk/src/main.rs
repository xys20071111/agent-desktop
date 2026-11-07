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

fn run(config: Config) -> anyhow::Result<()> {
    let mut display: Display<AgentDesk> = Display::new()?;
    let dh = display.handle();

    let (ipc_server, hub) = IpcServer::bind(IpcServerConfig {
        path: config.socket_path.clone(),
        token: config.token.clone(),
        ..Default::default()
    })?;

    // 查看器线程 → 主循环 的事件通道
    let (viewer_events_tx, viewer_events_rx) =
        calloop::channel::channel::<viewer::ViewerEvent>();

    let mut state = AgentDesk::new(&dh, &config, hub, viewer_events_tx);

    let mut event_loop: calloop::EventLoop<AgentDesk> = calloop::EventLoop::try_new()?;
    state.loop_signal = Some(event_loop.get_signal());
    let loop_handle = event_loop.handle();

    loop_handle
        .insert_source(viewer_events_rx, |event, _, state: &mut AgentDesk| {
            match event {
                calloop::channel::Event::Msg(ev) => control::handle_viewer_event(state, ev),
                calloop::channel::Event::Closed => {}
            }
        })
        .map_err(|_e| anyhow::anyhow!("挂载查看器事件 source 失败"))?;

    // 控制平面请求 → 主循环
    loop_handle
        .insert_source(ipc_server.channel, |event, _, state: &mut AgentDesk| {
            match event {
                calloop::channel::Event::Msg(req) => control::handle_ipc(state, req),
                calloop::channel::Event::Closed => {}
            }
        })
        .map_err(|_e| anyhow::anyhow!("挂载 IPC channel source 失败"))?;

    // Wayland 客户端接入
    let socket_source = ListeningSocketSource::with_name(&config.wayland_socket_name)?;
    loop_handle
        .insert_source(socket_source, |stream, _, state: &mut AgentDesk| {
            let _ = state.dh.insert_client(stream, Arc::new(ClientState::default()));
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
    tracing::info!("agentdesk 已退出");
    result.map_err(|e| anyhow::anyhow!("事件循环错误: {e}"))
}
