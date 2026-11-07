//! ad-test-client：agent-desktop e2e 测试客户端（sctk 0.21）。
//!
//! 行为（见 docs/DESIGN.md §13）：
//! - 创建 xdg_toplevel，纯色填充（`--color`），供截图像素断言
//! - 点击窗口在 `--color` / `--alt-color` 间切换（供输入注入断言）
//! - 收到的可打印键盘文本追加到窗口标题（供 type_text 断言）
//! - 收到 close 请求即退出

use std::time::Duration;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, FrameCallbackData};
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::reexports::calloop::{EventLoop, LoopHandle};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::xdg::window::{Window, WindowConfigure, WindowDecorations, WindowHandler};
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::Buffer;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::delegate_registry;

#[derive(Debug, clap::Parser)]
#[command(name = "ad-test-client", version, about = "agent-desktop e2e 测试客户端")]
struct Cli {
    /// 初始颜色 RRGGBB
    #[arg(long, default_value = "ff0000")]
    color: String,
    /// 点击后切换到的颜色 RRGGBB
    #[arg(long, default_value = "00ff00")]
    alt_color: String,
    /// 初始窗口标题
    #[arg(long, default_value = "ad-test-client")]
    title: String,
    /// app_id
    #[arg(long, default_value = "ad.test.client")]
    app_id: String,
    /// 初始尺寸
    #[arg(long, default_value = "320")]
    width: u32,
    #[arg(long, default_value = "240")]
    height: u32,
}

fn parse_color(s: &str) -> [u8; 3] {
    let v = u32::from_str_radix(s.trim_start_matches('#'), 16).expect("颜色应为 RRGGBB 十六进制");
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

fn main() {
    let cli: Cli = clap::Parser::parse();

    let conn = Connection::connect_to_env().expect("连接 WAYLAND_DISPLAY 失败");
    let (globals, event_queue) = registry_queue_init(&conn).expect("registry 初始化失败");
    let qh = event_queue.handle();
    let mut event_loop: EventLoop<TestClient> = EventLoop::try_new().expect("calloop 初始化失败");
    let loop_handle = event_loop.handle();
    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle)
        .expect("WaylandSource 挂载失败");

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor 不可用");
    let xdg_shell = XdgShell::bind(&globals, &qh).expect("xdg_shell 不可用");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm 不可用");

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::None, &qh);
    window.set_title(&cli.title);
    window.set_app_id(&cli.app_id);
    // 初始 commit（无 buffer）→ 等待合成器 configure 后再绘制
    window.commit();

    let pool = smithay_client_toolkit::shm::slot::SlotPool::new(
        (cli.width * cli.height * 4) as usize,
        &shm,
    )
    .expect("SlotPool 创建失败");

    let mut app = TestClient {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        cli_title: cli.title.clone(),
        title_extra: String::new(),
        colors: [parse_color(&cli.color), parse_color(&cli.alt_color)],
        color_idx: 0usize,
        exit: false,
        first_configure: true,
        pool,
        width: cli.width,
        height: cli.height,
        buffer: None,
        window,
        keyboard: None,
        pointer: None,
        loop_handle: event_loop.handle(),
    };

    loop {
        event_loop.dispatch(Duration::from_millis(16), &mut app).expect("dispatch 失败");
        if app.exit {
            break;
        }
    }
}

struct TestClient {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,

    cli_title: String,
    title_extra: String,
    colors: [[u8; 3]; 2],
    color_idx: usize,
    exit: bool,
    first_configure: bool,
    pool: smithay_client_toolkit::shm::slot::SlotPool,
    width: u32,
    height: u32,
    buffer: Option<Buffer>,
    window: Window,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    loop_handle: LoopHandle<'static, TestClient>,
}

impl TestClient {
    fn current_color(&self) -> [u8; 3] {
        self.colors[self.color_idx]
    }

    fn draw(&mut self, _conn: &Connection, qh: &QueueHandle<Self>) {
        let width = self.width;
        let height = self.height;
        let stride = self.width as i32 * 4;

        let [r, g, b] = self.current_color();
        let color = (0xFFu32 << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
        let bytes = color.to_le_bytes();

        let buffer = self.buffer.get_or_insert_with(|| {
            self.pool
                .create_buffer(width as i32, height as i32, stride, wl_shm::Format::Argb8888)
                .expect("create buffer")
                .0
        });

        let canvas = match self.pool.canvas(buffer) {
            Some(canvas) => canvas,
            None => {
                let (second, canvas) = self
                    .pool
                    .create_buffer(width as i32, height as i32, stride, wl_shm::Format::Argb8888)
                    .expect("create buffer");
                *buffer = second;
                canvas
            }
        };

        for chunk in canvas.chunks_exact_mut(4) {
            chunk.copy_from_slice(&bytes);
        }

        self.window
            .wl_surface()
            .damage_buffer(0, 0, self.width as i32, self.height as i32);
        // 请求下一帧（合成器以 30Hz tick 发送 frame done）
        self.window
            .wl_surface()
            .frame(qh, FrameCallbackData(self.window.wl_surface().clone()));
        buffer
            .attach_to(self.window.wl_surface())
            .expect("buffer attach");
        self.window.commit();
    }

    fn set_title_with(&mut self, text: &str) {
        let full = format!("{}{}", self.cli_title, text);
        self.window.set_title(&full);
        self.window.commit();
    }
}

impl CompositorHandler for TestClient {
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
        conn: &Connection,
        qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.draw(conn, qh);
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

impl smithay_client_toolkit::output::OutputHandler for TestClient {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: wl_output::WlOutput) {}
    fn update_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: wl_output::WlOutput) {}
}

impl WindowHandler for TestClient {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        self.buffer = None;
        self.width = configure.new_size.0.map(|v| v.get()).unwrap_or(320);
        self.height = configure.new_size.1.map(|v| v.get()).unwrap_or(240);
        if self.first_configure {
            self.first_configure = false;
            self.draw(conn, qh);
        }
    }
}

impl SeatHandler for TestClient {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            let keyboard = self
                .seat_state
                .get_keyboard(qh, &seat, None)
                .expect("keyboard 创建失败");
            self.keyboard = Some(keyboard);
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            let pointer = self.seat_state.get_pointer(qh, &seat).expect("pointer 创建失败");
            self.pointer = Some(pointer);
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_some() {
            self.keyboard.take().unwrap().release();
        }
        if capability == Capability::Pointer && self.pointer.is_some() {
            self.pointer.take().unwrap().release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for TestClient {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _keysyms: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        // 可打印字符追加到标题，供 type_text 断言
        if let Some(c) = event.keysym.key_char() {
            if !c.is_control() {
                self.title_extra.push(c);
                let extra = self.title_extra.clone();
                self.set_title_with(&extra);
            }
        }
    }

    fn repeat_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _event: KeyEvent,
    ) {
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _modifiers: Modifiers,
        _raw: RawModifiers,
        _layout: u32,
    ) {
    }
}

impl PointerHandler for TestClient {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if &event.surface != self.window.wl_surface() {
                continue;
            }
            match event.kind {
                PointerEventKind::Press { .. } => {
                    // 点击换色（M2 输入断言用）+ 标题打点
                    self.color_idx = 1 - self.color_idx;
                    self.buffer = None;
                    self.title_extra.push('*');
                    let extra = self.title_extra.clone();
                    self.set_title_with(&extra);
                    self.draw(_conn, _qh);
                }
                _ => {}
            }
        }
    }
}

impl ShmHandler for TestClient {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(TestClient);

impl ProvidesRegistryState for TestClient {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_dispatch2!(TestClient);
