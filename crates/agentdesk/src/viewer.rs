//! 嵌套查看器：进程内 sctk 客户端线程，把主输出帧镜像到父会话窗口。
//!
//! 架构（docs/DESIGN.md §7.1）：合成器核心永远无头；"可见" = 启动本线程，
//! 在父会话开一个普通窗口，消费主循环按 30Hz 推送的帧（BGRA 内存）。
//! 崩溃隔离：查看器线程 panic 不影响合成器；窗口被父 WM 关闭 = hide。

use std::sync::{mpsc, Arc};
use std::time::Duration;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, FrameCallbackData};
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::reexports::calloop::channel::Sender as CalloopSender;
use smithay_client_toolkit::reexports::calloop::timer::{Timer, TimeoutAction};
use smithay_client_toolkit::reexports::calloop::{EventLoop, LoopHandle};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{wl_output, wl_shm, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry};

/// 主循环 → 查看器线程 的命令。
pub enum ViewerCommand {
    Frame { data: Arc<Vec<u8>>, width: i32, height: i32 },
    Exit,
}

/// 查看器线程 → 主循环 的事件。
#[derive(Debug)]
pub enum ViewerEvent {
    /// 嵌套窗口被父 WM 关闭
    Closed,
    /// 查看器初始化失败（连接失败/协议缺失/panic）
    Failed(String),
}

/// 查看器句柄（主循环侧持有）。
#[derive(Clone)]
pub struct ViewerHandle {
    cmd: mpsc::Sender<ViewerCommand>,
}

impl ViewerHandle {
    pub fn send_frame(&self, data: Arc<Vec<u8>>, width: i32, height: i32) {
        let _ = self.cmd.send(ViewerCommand::Frame { data, width, height });
    }

    pub fn stop(&self) {
        let _ = self.cmd.send(ViewerCommand::Exit);
    }
}

pub struct ViewerParams {
    pub title: String,
    pub width: i32,
    pub height: i32,
    /// 回主循环的事件通道（calloop，线程安全）
    pub events: CalloopSender<ViewerEvent>,
}

/// 启动查看器线程（连接父会话；失败经 events 报告 Failed）。
pub fn spawn(params: ViewerParams) -> ViewerHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let events = params.events.clone();
    let _ = std::thread::Builder::new()
        .name("ad-viewer".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_viewer(params, cmd_rx)
            }));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    let _ = events.send(ViewerEvent::Failed(e));
                }
                Err(_) => {
                    let _ = events.send(ViewerEvent::Failed("viewer 线程 panic".into()));
                }
            }
        });
    ViewerHandle { cmd: cmd_tx }
}

fn run_viewer(params: ViewerParams, cmd_rx: mpsc::Receiver<ViewerCommand>) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("连接父会话失败: {e}"))?;
    let (globals, queue) = registry_queue_init(&conn).map_err(|e| format!("registry 初始化失败: {e}"))?;
    let qh = queue.handle();
    let mut evloop: EventLoop<ViewerApp> = EventLoop::try_new().map_err(|e| e.to_string())?;
    WaylandSource::new(conn.clone(), queue)
        .insert(evloop.handle())
        .map_err(|e| e.to_string())?;

    let compositor =
        CompositorState::bind(&globals, &qh).map_err(|_| "wl_compositor 不可用".to_string())?;
    let xdg = XdgShell::bind(&globals, &qh).map_err(|_| "xdg_shell 不可用".to_string())?;
    let shm = Shm::bind(&globals, &qh).map_err(|_| "wl_shm 不可用".to_string())?;

    let surface = compositor.create_surface(&qh);
    let window = xdg.create_window(surface, WindowDecorations::None, &qh);
    window.set_title(&params.title);
    window.set_app_id("agentdesk.viewer");
    window.commit();

    let pool = SlotPool::new((params.width * params.height * 4) as usize, &shm)
        .map_err(|e| format!("SlotPool 创建失败: {e}"))?;

    let (w0, h0) = (params.width, params.height);
    let mut app = ViewerApp {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        params,
        cmd_rx,
        qh: qh.clone(),
        window,
        pool,
        width: w0,
        height: h0,
        configured: false,
        buffer: None,
        pending_frame: None,
        exit: false,
    };

    // 命令轮询（16ms 粒度足够 30fps 帧流）
    let loop_handle: LoopHandle<ViewerApp> = evloop.handle();
    loop_handle
        .insert_source(
            Timer::from_duration(Duration::from_millis(16)),
            |_, _, app: &mut ViewerApp| {
                while let Ok(cmd) = app.cmd_rx.try_recv() {
                    match cmd {
                        ViewerCommand::Frame { data, width, height } => {
                            app.pending_frame = Some((data, width, height));
                            if app.configured {
                                app.draw();
                            }
                        }
                        ViewerCommand::Exit => {
                            app.exit = true;
                            return TimeoutAction::Drop;
                        }
                    }
                }
                TimeoutAction::ToDuration(Duration::from_millis(16))
            },
        )
        .map_err(|e| format!("挂载命令源失败: {e}"))?;

    while !app.exit {
        evloop
            .dispatch(Duration::from_millis(16), &mut app)
            .map_err(|e| format!("viewer dispatch: {e}"))?;
    }
    Ok(())
}

fn scale_nearest(src: &[u8], sw: i32, sh: i32, dw: i32, dh: i32) -> Vec<u8> {
    let mut out = vec![0u8; (dw * dh * 4) as usize];
    for y in 0..dh {
        let sy = (y * sh / dh).min(sh - 1);
        for x in 0..dw {
            let sx = (x * sw / dw).min(sw - 1);
            let s = ((sy * sw + sx) * 4) as usize;
            let d = ((y * dw + x) * 4) as usize;
            out[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    out
}

struct ViewerApp {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    params: ViewerParams,
    cmd_rx: mpsc::Receiver<ViewerCommand>,
    qh: QueueHandle<ViewerApp>,
    window: Window,
    pool: SlotPool,
    width: i32,
    height: i32,
    configured: bool,
    buffer: Option<Buffer>,
    /// 最近一帧（BGRA）：主循环 30Hz 推送
    pending_frame: Option<(Arc<Vec<u8>>, i32, i32)>,
    exit: bool,
}

impl ViewerApp {
    fn draw(&mut self) {
        let Some((data, fw, fh)) = self.pending_frame.take() else { return };
        let (dw, dh) = (self.width, self.height);
        let pixels = if (dw, dh) == (fw, fh) {
            (*data).clone()
        } else {
            scale_nearest(&data, fw, fh, dw, dh)
        };
        let stride = dw * 4;
        let buffer = self.buffer.get_or_insert_with(|| {
            self.pool
                .create_buffer(dw, dh, stride, wl_shm::Format::Argb8888)
                .expect("viewer buffer 创建失败")
                .0
        });
        let canvas = match self.pool.canvas(buffer) {
            Some(c) if c.len() == pixels.len() => c,
            _ => {
                let (b2, c2) = self
                    .pool
                    .create_buffer(dw, dh, stride, wl_shm::Format::Argb8888)
                    .expect("viewer buffer 重建失败");
                *buffer = b2;
                c2
            }
        };
        canvas.copy_from_slice(&pixels);
        self.window.wl_surface().damage_buffer(0, 0, dw, dh);
        self.window
            .wl_surface()
            .frame(&self.qh, FrameCallbackData(self.window.wl_surface().clone()));
        if buffer.attach_to(self.window.wl_surface()).is_ok() {
            self.window.commit();
        }
    }
}

impl CompositorHandler for ViewerApp {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl smithay_client_toolkit::output::OutputHandler for ViewerApp {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _c: &Connection, _q: &QueueHandle<Self>, _o: wl_output::WlOutput) {}
    fn update_output(&mut self, _c: &Connection, _q: &QueueHandle<Self>, _o: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _c: &Connection, _q: &QueueHandle<Self>, _o: wl_output::WlOutput) {}
}

impl WindowHandler for ViewerApp {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
        let _ = self.params.events.send(ViewerEvent::Closed);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        self.buffer = None;
        self.width = configure
            .new_size
            .0
            .map(|v| i32::try_from(v.get()).unwrap_or(self.width))
            .unwrap_or(self.width);
        self.height = configure
            .new_size
            .1
            .map(|v| i32::try_from(v.get()).unwrap_or(self.height))
            .unwrap_or(self.height);
        self.configured = true;
    }
}

impl ShmHandler for ViewerApp {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(ViewerApp);

impl ProvidesRegistryState for ViewerApp {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

smithay_client_toolkit::delegate_dispatch2!(ViewerApp);
