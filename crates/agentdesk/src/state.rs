//! 合成器核心状态、窗口记录与 Wayland 协议处理。
//!
//! 依据 smithay 0.7.0 实际 API（速查表已核实源码）：
//! - 无 headless backend：虚拟输出 = 普通 `Output` global + 自管理渲染时钟；
//! - `XdgShellHandler::new_toplevel` 里发送首个 configure，无 initial_commit 钩子；
//! - 帧回调经 `send_frames_surface_tree` 以 tick 节拍（30Hz）发送。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::desktop::utils::send_frames_surface_tree;
use smithay::desktop::{Space, Window};
use smithay::input::keyboard::KeyboardHandle;
use smithay::input::pointer::{CursorImageStatus, PointerHandle};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason, GlobalId};
use smithay::reexports::wayland_server::protocol::{wl_buffer::WlBuffer, wl_shm::Format};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DisplayHandle, Resource};
use smithay::utils::{Logical, Point, SERIAL_COUNTER, Size, Transform};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState, get_parent, with_states,
};
use smithay::wayland::shell::xdg::{
    Configure, PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XdgShellHandler,
    XdgShellState, XdgToplevelSurfaceData,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::wayland::viewporter::ViewporterState;

use ad_ipc::error::RpcError;
use ad_ipc::protocol::{
    Event, GeometryJson, SizeJson, WindowFilter, WindowJson, WindowKind, WindowOrigin, WindowState,
};

use crate::config::Config;

/// 每个客户端连接的协议状态。
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client: ClientId) {}
    fn disconnected(&self, _client: ClientId, _reason: DisconnectReason) {}
}

/// 虚拟输出记录。
pub struct OutputEntry {
    pub name: String,
    pub output: Output,
    #[allow(dead_code)]
    pub global: GlobalId,
    pub size: (i32, i32),
}

/// 窗口记录（`WindowId` 永不复用）。
pub struct WindowRecord {
    pub id: u64,
    pub element: Window,
    pub output: String,
    pub origin: WindowOrigin,
    pub pid: u32,
    pub created_at_ms: u64,
    pub minimized: bool,
    /// 是否已进入 Space（首次带 buffer 的 commit 后置位）
    pub mapped: bool,
    /// 上次上报的内容尺寸（用于变化检测，避免 30Hz 重绘刷屏）
    pub last_surface_size: (i32, i32),
}

/// 等待窗口出现的 launch 请求。
pub struct PendingLaunch {
    pub pid: u32,
    pub conn_id: u64,
    pub request_id: ad_ipc::rpc::Id,
    pub deadline: Instant,
}

/// 合成器全局状态（calloop 的 `Data`）。
pub struct AgentDesk {
    pub dh: DisplayHandle,
    pub started: Instant,

    // —— 配置快照 ——
    pub socket_path: String,
    pub wayland_socket_name: String,
    pub parent_session: Option<&'static str>,
    pub token: Option<String>,
    pub x11_display: Option<u32>,

    // —— Wayland 协议状态 ——
    pub compositor_state: CompositorState,
    pub shm_state: ShmState,
    pub xdg_shell_state: XdgShellState,
    pub viewporter_state: ViewporterState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub seat: Seat<Self>,
    #[allow(dead_code)]
    pub keyboard: Option<KeyboardHandle<Self>>,
    #[allow(dead_code)]
    pub pointer: Option<PointerHandle<Self>>,

    /// pixman 软件渲染器（截图/查看器共用；None = libpixman 初始化失败）
    pub renderer: Option<PixmanRenderer>,
    /// xwayland-satellite 子进程（x11 编排拉起后持有，防 zombie）
    pub satellite: Option<std::process::Child>,
    /// 等待窗口出现的 launch 请求（tick 中轮询，避免阻塞主循环）
    pub pending_launches: Vec<PendingLaunch>,

    // —— 窗口系统 ——
    pub space: Space<Window>,
    pub outputs: Vec<OutputEntry>,
    pub layout: ad_wm::layout::FloatingLayout,
    pub windows: HashMap<u64, WindowRecord>,
    pub window_order: Vec<u64>,
    pub next_window_id: u64,
    pub focused: Option<u64>,
    pub cursor_pos: Point<f64, Logical>,

    // —— 控制平面 ——
    pub hub: ad_ipc::server::IpcHub,
    /// 嵌套查看器（活跃时 Some）
    pub viewer: Option<crate::viewer::ViewerHandle>,
    pub viewer_visible: bool,
    pub viewer_size: Option<(i32, i32)>,
    pub viewer_events: calloop::channel::Sender<crate::viewer::ViewerEvent>,
    pub loop_signal: Option<calloop::LoopSignal>,
    pub shutting_down: bool,
}

impl AgentDesk {
    pub fn new(
        dh: &DisplayHandle,
        config: &Config,
        hub: ad_ipc::server::IpcHub,
        viewer_events: calloop::channel::Sender<crate::viewer::ViewerEvent>,
    ) -> Self {
        let compositor_state = CompositorState::new::<Self>(dh);
        let shm_state = ShmState::new::<Self>(
            dh,
            [Format::Argb8888, Format::Xrgb8888, Format::Abgr8888, Format::Xbgr8888],
        );
        let xdg_shell_state = XdgShellState::new::<Self>(dh);
        let viewporter_state = ViewporterState::new::<Self>(dh);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(dh);

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(dh, "agentdesk");
        // 强制 us 布局：注入层按 US 键码表工作（见 ad-wm::keymap），客户端按同 keymap 解析
        let keyboard = seat
            .add_keyboard(
                smithay::input::keyboard::XkbConfig {
                    rules: "",
                    model: "",
                    layout: "us",
                    variant: "",
                    options: None,
                },
                200,
                25,
            )
            .ok();
        let pointer = seat.add_pointer();

        // 虚拟输出（无 backend；渲染时钟由 tick 定时器承担）
        let name = "HEADLESS-1".to_string();
        let output = Output::new(
            name.clone(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "agentdesk".into(),
                model: "virtual".into(),
            },
        );
        let mode = Mode { size: (config.width, config.height).into(), refresh: 60_000 };
        output.add_mode(mode);
        output.set_preferred(mode);
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Integer(1)),
            Some(Point::from((0, 0))),
        );
        let global = output.create_global::<Self>(dh);
        let mut space = Space::default();
        space.map_output(&output, Point::from((0, 0)));

        let outputs = vec![OutputEntry { name, output, global, size: (config.width, config.height) }];

        Self {
            dh: dh.clone(),
            started: Instant::now(),
            socket_path: config.socket_path.display().to_string(),
            wayland_socket_name: config.wayland_socket_name.clone(),
            parent_session: config.parent_session,
            token: config.token.clone(),
            x11_display: config.x11_display,
            compositor_state,
            shm_state,
            xdg_shell_state,
            viewporter_state,
            output_manager_state,
            seat_state,
            seat,
            keyboard,
            pointer: Some(pointer),
            renderer: PixmanRenderer::new().ok(),
            satellite: None,
            pending_launches: Vec::new(),
            space,
            outputs,
            layout: ad_wm::layout::FloatingLayout::new(),
            windows: HashMap::new(),
            window_order: Vec::new(),
            next_window_id: 1,
            focused: None,
            cursor_pos: Point::from((0.0, 0.0)),
            hub,
            viewer: None,
            viewer_visible: false,
            viewer_size: None,
            viewer_events,
            loop_signal: None,
            shutting_down: false,
        }
    }

    pub fn primary_output(&self) -> Option<&OutputEntry> {
        self.outputs.first()
    }

    pub fn primary_output_size(&self) -> (i32, i32) {
        self.primary_output().map(|o| o.size).unwrap_or((1920, 1080))
    }

    /// 主循环 tick：保活 Space、发送帧回调、查看器推帧、处理退出。
    pub fn tick(&mut self) {
        self.space.refresh();
        let Some(output) = self.primary_output().map(|o| o.output.clone()) else { return };
        let surfaces: Vec<WlSurface> = self
            .windows
            .values()
            .filter(|r| r.mapped)
            .filter_map(|r| r.element.toplevel().map(|t| t.wl_surface().clone()))
            .collect();
        for surface in surfaces {
            send_frames_surface_tree(
                &surface,
                &output,
                self.started.elapsed(),
                None,
                |_, _| Some(output.clone()),
            );
        }
        // 查看器推帧（30Hz；隐藏时零开销）
        if self.viewer_visible {
            if let Some(viewer) = self.viewer.clone() {
                match self.screen_capture(None) {
                    Ok(raw) => {
                        viewer.send_frame(Arc::new(raw.data), raw.width, raw.height);
                    }
                    Err(e) => {
                        tracing::debug!("查看器帧渲染失败: {e}");
                    }
                }
            }
        }
        // launch 等待：窗口出现或超时后补发响应（不阻塞主循环）
        if !self.pending_launches.is_empty() {
            let now = Instant::now();
            let mut done = Vec::new();
            for p in &self.pending_launches {
                let found = self.windows.values().find(|r| r.pid == p.pid).map(|r| r.id);
                match found {
                    Some(id) if now <= p.deadline => {
                        self.hub.respond(
                            p.conn_id,
                            Some(p.request_id.clone()),
                            Ok(serde_json::json!({ "pid": p.pid, "window_id": id })),
                        );
                        done.push(p.pid);
                    }
                    _ if now > p.deadline => {
                        self.hub.respond(
                            p.conn_id,
                            Some(p.request_id.clone()),
                            Ok(serde_json::json!({ "pid": p.pid, "window_id": null })),
                        );
                        done.push(p.pid);
                    }
                    _ => {}
                }
            }
            self.pending_launches.retain(|p| !done.contains(&p.pid));
        }
        if self.shutting_down {
            if let Some(sig) = &self.loop_signal {
                sig.stop();
            }
        }
    }

    // ---------------- 窗口系统 ----------------

    /// 沿 parent 链向上定位 surface 所属窗口（覆盖子表面提交）。
    fn window_for_surface(&self, surface: &WlSurface) -> Option<u64> {
        let mut cur = surface.clone();
        loop {
            let hit = self.windows.iter().find(|(_, r)| {
                r.element.toplevel().map(|t| t.wl_surface() == &cur).unwrap_or(false)
            }).map(|(k, _)| *k);
            if hit.is_some() {
                return hit;
            }
            cur = get_parent(&cur)?;
        }
    }

    fn id_of_toplevel(&self, surface: &ToplevelSurface) -> Option<u64> {
        let wl = surface.wl_surface();
        self.windows
            .iter()
            .find(|(_, r)| r.element.toplevel().map(|t| t.wl_surface() == wl).unwrap_or(false))
            .map(|(k, _)| *k)
    }

    fn on_surface_commit(&mut self, surface: &WlSurface) {
        let Some(id) = self.window_for_surface(surface) else { return };
        let was_mapped = self.windows.get(&id).map(|r| r.mapped).unwrap_or(false);

        if !was_mapped {
            // 1) 已映射窗口占据集合（immutable 借阅先行）
            let output_size = self.primary_output_size();
            let mut occupied: Vec<(i32, i32, i32, i32)> = Vec::new();
            for (k, r) in &self.windows {
                if r.mapped && *k != id {
                    if let Some(loc) = self.space.element_location(&r.element) {
                        let b = r.element.bbox();
                        occupied.push((loc.x + b.loc.x, loc.y + b.loc.y, b.size.w, b.size.h));
                    }
                }
            }
            // 2) 标记并取出元素
            let (window, bbox) = {
                let r = self.windows.get_mut(&id).expect("id 来自同表");
                r.mapped = true;
                r.last_surface_size = (bbox_size_of(&r.element).0, bbox_size_of(&r.element).1);
                let b = r.element.bbox();
                (r.element.clone(), b)
            };
            // 3) 布局放置
            let pos = self.layout.place_new(
                output_size,
                (bbox.size.w.max(64), bbox.size.h.max(64)),
                &occupied,
            );
            self.space.map_element(window, (pos.0, pos.1), false);
            // 4) 焦点 + 事件
            let _ = self.focus_window(id);
            if let Some(wj) = self.window_json(id) {
                self.hub.broadcast(&Event::WindowCreated { window: wj });
            }
        } else {
            // 已映射窗口：内容尺寸变化才推送（on_commit 已刷新 bbox）
            let new_size = self
                .windows
                .get(&id)
                .map(|r| bbox_size_of(&r.element))
                .unwrap_or((0, 0));
            let changed = self.windows.get(&id).map(|r| r.last_surface_size != new_size).unwrap_or(false);
            if changed {
                if let Some(r) = self.windows.get_mut(&id) {
                    r.last_surface_size = new_size;
                }
                if let Some(wj) = self.window_json(id) {
                    self.hub.broadcast(&Event::WindowUpdated {
                        id,
                        changed: vec!["surface_size".into()],
                        window: wj,
                    });
                }
            }
        }
    }

    pub fn list_windows(&self, filter: Option<&WindowFilter>) -> Vec<WindowJson> {
        self.window_order
            .iter()
            .filter_map(|id| self.window_json(*id))
            .filter(|w| filter.map(|f| f.matches(w)).unwrap_or(true))
            .collect()
    }

    /// 组装窗口 JSON（见 docs/CONTROL_API.md §3.2）。
    pub fn window_json(&self, id: u64) -> Option<WindowJson> {
        let r = self.windows.get(&id)?;
        let toplevel = r.element.toplevel()?;
        let wl = toplevel.wl_surface();
        let (title, app_id) = with_states(wl, |s| {
            match s.data_map.get::<XdgToplevelSurfaceData>() {
                Some(d) => {
                    let d = d.lock().unwrap();
                    (d.title.clone(), d.app_id.clone())
                }
                None => (None, None),
            }
        });
        let loc = self.space.element_location(&r.element).unwrap_or_default();
        let b = r.element.bbox();
        let tstate = toplevel.current_state();
        let mut states: Vec<WindowState> = Vec::new();
        if tstate.states.contains(xdg_toplevel::State::Activated) {
            states.push(WindowState::Activated);
        }
        if tstate.states.contains(xdg_toplevel::State::Fullscreen) {
            states.push(WindowState::Fullscreen);
        }
        if tstate.states.contains(xdg_toplevel::State::Maximized) {
            states.push(WindowState::Maximized);
        }
        if r.minimized {
            states.push(WindowState::Minimized);
        }
        let cached = with_states(wl, |s| *s.cached_state.get::<SurfaceCachedState>().current());
        let min_size = (cached.min_size.w > 0 || cached.min_size.h > 0)
            .then(|| SizeJson { width: cached.min_size.w, height: cached.min_size.h });
        let max_size = (cached.max_size.w > 0 || cached.max_size.h > 0)
            .then(|| SizeJson { width: cached.max_size.w, height: cached.max_size.h });
        Some(WindowJson {
            id,
            kind: WindowKind::Toplevel,
            origin: r.origin,
            title,
            app_id,
            pid: r.pid,
            geometry: GeometryJson {
                x: loc.x + b.loc.x,
                y: loc.y + b.loc.y,
                width: b.size.w,
                height: b.size.h,
            },
            surface_size: SizeJson { width: b.size.w, height: b.size.h },
            states,
            output: r.output.clone(),
            focused: self.focused == Some(id),
            min_size,
            max_size,
            created_at_ms: r.created_at_ms,
        })
    }

    pub fn focus_window(&mut self, id: u64) -> Result<(), RpcError> {
        if !self.windows.contains_key(&id) {
            return Err(RpcError::window_not_found(id));
        }
        let previous = self.focused;
        if let Some(old) = previous.filter(|o| *o != id) {
            if let Some(r) = self.windows.get(&old) {
                r.element.set_activated(false);
            }
        }
        self.focused = Some(id);
        {
            let r = self.windows.get(&id).expect("刚检查过存在");
            r.element.set_activated(true);
            let loc = self.space.element_location(&r.element).unwrap_or_default();
            self.space.map_element(r.element.clone(), loc, true);
        }
        // 键盘焦点（handle 为独立 clone，避免借用冲突）
        let surface = self
            .windows
            .get(&id)
            .and_then(|r| r.element.toplevel())
            .map(|t| t.wl_surface().clone());
        let kb = self.seat.get_keyboard();
        let serial = SERIAL_COUNTER.next_serial();
        if let (Some(kb), Some(surface)) = (kb, surface) {
            kb.set_focus(self, Some(surface), serial);
        }
        self.hub.broadcast(&Event::FocusChanged { previous_id: previous, id: Some(id) });
        Ok(())
    }

    pub fn raise_window(&mut self, id: u64) -> Result<(), RpcError> {
        let loc = self
            .windows
            .get(&id)
            .ok_or_else(|| RpcError::window_not_found(id))
            .ok()
            .and_then(|r| self.space.element_location(&r.element));
        let Some(loc) = loc else { return Err(RpcError::window_not_found(id)) };
        let window = self.windows.get(&id).map(|r| r.element.clone()).unwrap();
        self.space.map_element(window, loc, false);
        Ok(())
    }

    pub fn close_window(&mut self, id: u64) -> Result<(), RpcError> {
        let toplevel = self
            .windows
            .get(&id)
            .and_then(|r| r.element.toplevel().cloned())
            .ok_or_else(|| RpcError::window_not_found(id))?;
        toplevel.send_close();
        Ok(())
    }

    pub fn set_geometry(
        &mut self,
        id: u64,
        x: Option<i32>,
        y: Option<i32>,
        width: Option<i32>,
        height: Option<i32>,
    ) -> Result<(), RpcError> {
        if !self.windows.contains_key(&id) {
            return Err(RpcError::window_not_found(id));
        }
        let mut changed: Vec<String> = Vec::new();
        let moved = x.is_some() || y.is_some();
        if moved {
            let cur = self
                .windows
                .get(&id)
                .and_then(|r| self.space.element_location(&r.element))
                .unwrap_or_default();
            let nx = x.unwrap_or(cur.x);
            let ny = y.unwrap_or(cur.y);
            let window = self.windows.get(&id).map(|r| r.element.clone()).unwrap();
            self.space.map_element(window, (nx, ny), false);
            changed.push("geometry".into());
        }
        if width.is_some() || height.is_some() {
            let toplevel = self.windows.get(&id).and_then(|r| r.element.toplevel().cloned());
            if let Some(t) = toplevel {
                t.with_pending_state(|s| {
                    let cur = s.size.map(|sz| (sz.w, sz.h)).unwrap_or((0, 0));
                    s.size = Some(Size::from((
                        width.unwrap_or(cur.0),
                        height.unwrap_or(cur.1),
                    )));
                });
                t.send_configure();
                changed.push("size".into());
            }
        }
        if let Some(wj) = self.window_json(id) {
            self.hub.broadcast(&Event::WindowUpdated { id, changed, window: wj });
        }
        Ok(())
    }

    // ---------------- 控制平面辅助 ----------------

    pub fn server_info(&self) -> ad_ipc::protocol::ServerInfoJson {
        let outputs = self
            .outputs
            .iter()
            .map(|o| {
                let loc = o.output.current_location();
                ad_ipc::protocol::OutputJson {
                    id: o.name.clone(),
                    width: o.size.0,
                    height: o.size.1,
                    scale: 1.0,
                    position: ad_ipc::protocol::PosJson { x: loc.x, y: loc.y },
                }
            })
            .collect();
        ad_ipc::protocol::ServerInfoJson {
            name: "agentdesk".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            api_version: ad_ipc::API_VERSION,
            pid: std::process::id(),
            uptime_ms: self.started.elapsed().as_millis() as u64,
            socket: self.socket_path.clone(),
            wayland_display: self.wayland_socket_name.clone(),
            parent_session: self.parent_session.map(str::to_owned),
            viewer: ad_ipc::protocol::ViewerInfoJson { visible: self.viewer_visible, size: self.viewer_size.map(|(w, h)| ad_ipc::protocol::SizeJson { width: w, height: h }) },
            renderer: if self.renderer.is_some() { "pixman".into() } else { "none".into() },
            outputs,
            x11: self.x11_status(),
        }
    }

    pub fn x11_status(&self) -> ad_ipc::protocol::X11StatusJson {
        let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
            return ad_ipc::protocol::X11StatusJson { available: false, display: self.x11_display, socket: None };
        };
        let candidates: Vec<u32> = match self.x11_display {
            Some(n) => vec![n],
            None => (0..=32).collect(),
        };
        for n in candidates {
            let p = std::path::Path::new(&runtime).join(".X11-unix").join(format!("X{n}"));
            if p.exists() {
                return ad_ipc::protocol::X11StatusJson {
                    available: true,
                    display: Some(n),
                    socket: Some(p.display().to_string()),
                };
            }
        }
        ad_ipc::protocol::X11StatusJson { available: false, display: self.x11_display, socket: None }
    }

    /// X11 编排：确保 xwayland-satellite 在运行，返回 X display 号。
    pub fn ensure_x11(&mut self) -> Result<u32, RpcError> {
        use ad_ipc::error::codes;
        let n = self.x11_display.unwrap_or(100);
        let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
            return Err(RpcError::internal("XDG_RUNTIME_DIR 未设置"));
        };
        let socket = std::path::Path::new(&runtime).join(".X11-unix").join(format!("X{n}"));
        if !socket.exists() {
            let mut cmd = std::process::Command::new("xwayland-satellite");
            cmd.arg(format!(":{n}"))
                .env("WAYLAND_DISPLAY", &self.wayland_socket_name);
            let child = cmd.spawn().map_err(|e| {
                RpcError::new(codes::LAUNCH_FAILED, format!("启动 xwayland-satellite 失败: {e}"))
            })?;
            self.satellite = Some(child);
            // 等 X socket 就绪（最多 3s）
            let deadline = Instant::now() + Duration::from_secs(3);
            while !socket.exists() {
                if Instant::now() >= deadline {
                    return Err(RpcError::new(
                        codes::LAUNCH_FAILED,
                        "xwayland-satellite 未在时限内创建 X socket",
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            tracing::info!(display = n, "xwayland-satellite 已启动");
        }
        Ok(n)
    }

    /// 启动应用：注入 WAYLAND_DISPLAY（或 X11 DISPLAY），返回 pid。
    pub fn launch_process(
        &mut self,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
        x11: bool,
    ) -> Result<u32, RpcError> {
        use ad_ipc::error::codes;
        let Some(program) = argv.first() else {
            return Err(RpcError::invalid_params("argv 为空"));
        };
        let mut cmd = std::process::Command::new(program);
        cmd.args(&argv[1..]);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        if x11 {
            let n = self.ensure_x11()?;
            cmd.env("DISPLAY", format!(":{n}"));
        } else {
            cmd.env("WAYLAND_DISPLAY", &self.wayland_socket_name);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().map_err(|e| {
            RpcError::new(codes::LAUNCH_FAILED, format!("启动 {program} 失败: {e}"))
        })?;
        let pid = child.id();
        tracing::info!(pid, program, x11, "已启动应用");
        Ok(pid)
    }

    /// 布局放置：位置（Space）+ 尺寸（configure），并推送事件。
    pub fn place_window(
        &mut self,
        id: u64,
        x: i32,
        y: i32,
        width: Option<i32>,
        height: Option<i32>,
    ) -> Result<(), RpcError> {
        self.set_geometry(id, Some(x), Some(y), width, height)
    }

    /// 虚拟输出枚举（outputs.list）。
    pub fn outputs_json(&self) -> Vec<ad_ipc::protocol::OutputJson> {
        self.outputs
            .iter()
            .map(|o| {
                let loc = o.output.current_location();
                ad_ipc::protocol::OutputJson {
                    id: o.name.clone(),
                    width: o.size.0,
                    height: o.size.1,
                    scale: 1.0,
                    position: ad_ipc::protocol::PosJson { x: loc.x, y: loc.y },
                }
            })
            .collect()
    }

    /// 新建虚拟输出（排在现有输出右侧）。
    pub fn output_create(
        &mut self,
        width: i32,
        height: i32,
        name: Option<String>,
    ) -> Result<String, RpcError> {
        let n = self.outputs.len() + 1;
        let name = name.unwrap_or_else(|| format!("HEADLESS-{n}"));
        if self.outputs.iter().any(|o| o.name == name) {
            return Err(RpcError::invalid_params(format!("输出名 {name} 已存在")));
        }
        let x_offset: i32 = self.outputs.iter().map(|o| o.size.0).sum();
        let output = Output::new(
            name.clone(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "agentdesk".into(),
                model: "virtual".into(),
            },
        );
        let mode = Mode { size: (width, height).into(), refresh: 60_000 };
        output.add_mode(mode);
        output.set_preferred(mode);
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Integer(1)),
            Some(Point::from((x_offset, 0))),
        );
        let global = output.create_global::<Self>(&self.dh);
        self.space.map_output(&output, Point::from((x_offset, 0)));
        self.outputs.push(OutputEntry {
            name: name.clone(),
            output,
            global,
            size: (width, height),
        });
        self.hub.broadcast(&ad_ipc::protocol::Event::OutputAdded {
            output: self.outputs_json().into_iter().find(|o| o.id == name).unwrap(),
        });
        tracing::info!(name, width, height, "虚拟输出已创建");
        Ok(name)
    }

    /// 销毁虚拟输出（其上窗口标记迁移到主输出；坐标不动）。
    pub fn output_destroy(&mut self, id: &str) -> Result<(), RpcError> {
        if self.outputs.len() <= 1 {
            return Err(RpcError::app("不能销毁最后一个输出"));
        }
        let Some(idx) = self.outputs.iter().position(|o| o.name == id) else {
            return Err(RpcError::new(
                ad_ipc::error::codes::OUTPUT_NOT_FOUND,
                format!("输出 {id} 不存在"),
            ));
        };
        let entry = self.outputs.remove(idx);
        self.space.unmap_output(&entry.output);
        self.dh.remove_global::<Self>(entry.global);
        let primary = self.outputs.first().map(|o| o.name.clone()).unwrap_or_default();
        for r in self.windows.values_mut() {
            if r.output == id {
                r.output = primary.clone();
            }
        }
        self.hub
            .broadcast(&ad_ipc::protocol::Event::OutputRemoved { id: id.to_owned() });
        tracing::info!(name = id, "虚拟输出已销毁");
        Ok(())
    }
}

// ---------------- Wayland handler 实现 ----------------

impl CompositorHandler for AgentDesk {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        // Window 的 bbox 缓存需要在每次 commit 时手动刷新
        if let Some(id) = self.window_for_surface(surface) {
            if let Some(r) = self.windows.get(&id) {
                r.element.on_commit();
            }
        }
        self.on_surface_commit(surface);
    }
}

impl BufferHandler for AgentDesk {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl ShmHandler for AgentDesk {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl XdgShellHandler for AgentDesk {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // 首个 configure：不限定尺寸（客户端自选），激活态在获得焦点时再发
        surface.send_configure();
        let id = self.next_window_id;
        self.next_window_id += 1;
        let wl = surface.wl_surface().clone();
        let pid = wl
            .client()
            .and_then(|c| c.get_credentials(&self.dh).ok())
            .map(|c| u32::try_from(c.pid).unwrap_or(0))
            .unwrap_or(0);
        let origin = detect_origin(pid);
        let element = Window::new_wayland_window(surface);
        self.windows.insert(
            id,
            WindowRecord {
                id,
                element,
                output: self.primary_output().map(|o| o.name.clone()).unwrap_or_default(),
                origin,
                pid,
                created_at_ms: now_ms(),
                minimized: false,
                mapped: false,
                last_surface_size: (0, 0),
            },
        );
        self.window_order.push(id);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        let _ = surface.send_configure();
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat, _serial: smithay::utils::Serial) {}

    fn reposition_request(&mut self, surface: PopupSurface, _positioner: PositionerState, token: u32) {
        surface.send_repositioned(token);
        let _ = surface.send_configure();
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|s| s.states.set(xdg_toplevel::State::Maximized));
        surface.send_configure();
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|s| s.states.unset(xdg_toplevel::State::Maximized));
        surface.send_configure();
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        surface.with_pending_state(|s| s.states.set(xdg_toplevel::State::Fullscreen));
        surface.send_configure();
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|s| s.states.unset(xdg_toplevel::State::Fullscreen));
        surface.send_configure();
    }

    fn ack_configure(&mut self, _surface: WlSurface, _configure: Configure) {}

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let Some(id) = self.id_of_toplevel(&surface) else { return };
        if self.focused == Some(id) {
            self.focused = None;
        }
        if let Some(r) = self.windows.remove(&id) {
            self.space.unmap_elem(&r.element);
        }
        self.window_order.retain(|x| *x != id);
        self.hub.broadcast(&Event::WindowClosed { id });
        // 焦点转移给最后创建的存活窗口
        if let Some(&next) = self.window_order.last() {
            let _ = self.focus_window(next);
        }
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        self.on_meta_changed(&surface, "title");
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        self.on_meta_changed(&surface, "app_id");
    }
}

impl AgentDesk {
    fn on_meta_changed(&mut self, surface: &ToplevelSurface, what: &'static str) {
        let Some(id) = self.id_of_toplevel(surface) else { return };
        if let Some(wj) = self.window_json(id) {
            self.hub.broadcast(&Event::WindowUpdated { id, changed: vec![what.into()], window: wj });
        }
    }
}

impl OutputHandler for AgentDesk {}

impl SeatHandler for AgentDesk {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}

    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: CursorImageStatus) {}
}

smithay::delegate_compositor!(AgentDesk);
smithay::delegate_shm!(AgentDesk);
smithay::delegate_xdg_shell!(AgentDesk);
smithay::delegate_seat!(AgentDesk);
smithay::delegate_output!(AgentDesk);
smithay::delegate_viewporter!(AgentDesk);

// ---------------- 辅助函数 ----------------

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn bbox_size_of(element: &Window) -> (i32, i32) {
    let b = element.bbox();
    (b.size.w, b.size.h)
}

fn detect_origin(pid: u32) -> WindowOrigin {
    match std::fs::read_link(format!("/proc/{pid}/exe")) {
        Ok(p) if p.to_string_lossy().contains("xwayland-satellite") => WindowOrigin::X11,
        Ok(_) => WindowOrigin::Wayland,
        Err(_) => WindowOrigin::Unknown,
    }
}
