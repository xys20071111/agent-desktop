//! agent-desktop e2e 测试（M1 验收：窗口生命周期 + 控制平面）。
//!
//! 形态：子进程启动 agentdesk（XDG_RUNTIME_DIR 指向测试临时目录），
//! 通过 ad-ipc 客户端断言，子进程启动 ad-test-client 作为真实 Wayland 客户端。

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ad_ipc::client::Client;
use ad_ipc::protocol::Event;
use serde_json::{json, Value};

const TIMEOUT: Duration = Duration::from_secs(8);

struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct E2e {
    dir: PathBuf,
    control_sock: PathBuf,
    log_path: PathBuf,
    compositor: KillOnDrop,
    client: Client,
    _app: Option<KillOnDrop>,
}

impl E2e {
    fn start(name: &str) -> E2e {
        // 每个测试独立目录（wayland socket 名虽同，但 XDG_RUNTIME_DIR 隔离）。
        // 注意 unix socket 路径上限 SUN_LEN=108：必须落在仓库根的 target/ 下，
        // 放在 crates/agentdesk/target 会超长。
        let dir_raw = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("e2e-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir_raw).expect("创建临时 XDG_RUNTIME_DIR 失败");
        // unix socket 路径有 SUN_LEN=108 上限：canonicalize 消除 ../.. 段
        let dir = dir_raw.canonicalize().expect("canonicalize 失败");

        let log_path = dir.join("compositor.log");
        let compositor_log = std::fs::File::create(&log_path).expect("创建 compositor 日志失败");
        let compositor_err = compositor_log.try_clone().expect("clone 日志文件失败");
        let compositor = Command::new(env!("CARGO_BIN_EXE_agentdesk"))
            .env("XDG_RUNTIME_DIR", &dir)
            .env("RUST_LOG", "info")
            // 显式移除父会话环境：验证无父会话分支（CI 环境即此形态）
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .args(["--instance", "e2e", "--size", "640x480"])
            .stderr(compositor_err)
            .spawn()
            .expect("启动 agentdesk 失败");
        let compositor = KillOnDrop(compositor);

        let control_sock = dir.join("agentdesk").join("e2e.sock");
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if control_sock.exists() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "等待控制 socket 超时：{}",
                control_sock.display()
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        let client = Client::connect(&control_sock).expect("连接控制 socket 失败");
        E2e { dir, control_sock, compositor, client, _app: None, log_path }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.client.call(method, params).expect("RPC 调用失败")
    }

    /// 轮询直到谓词成立。
    fn wait_until<F>(&mut self, what: &str, mut pred: F) -> Value
    where
        F: FnMut(Value) -> Option<Value>,
    {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(v) = pred(self.call("windows.list", json!({}))) {
                return v;
            }
            assert!(Instant::now() < deadline, "等待超时：{what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 等待下一个指定类型的事件。
    fn wait_event(&mut self, type_name: &str) -> Event {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match self.client.try_next_event(Duration::from_millis(300)) {
                Ok(Some(ev)) if ev.type_name() == type_name => return ev,
                Ok(Some(_)) => continue,
                Ok(None) => {}
                Err(e) => panic!("读事件失败: {e}"),
            }
            assert!(Instant::now() < deadline, "等待事件超时：{type_name}");
        }
    }

    fn launch_app(&mut self) {
        let app = Command::new(env!("CARGO_BIN_EXE_ad-test-client"))
            .env("XDG_RUNTIME_DIR", &self.dir)
            .env("WAYLAND_DISPLAY", "agentdesk-e2e")
            .env("RUST_LOG", "warn")
            .args([
                "--color", "ff0000",
                "--alt-color", "00ff00",
                "--title", "E2E Window",
                "--app-id", "ad.test.client",
                "--width", "320",
                "--height", "240",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("启动 ad-test-client 失败");
        self._app = Some(KillOnDrop(app));
    }

    /// 轮询直到整屏截图中 (x,y) 像素等于期望颜色（BGRA 序）。
    fn wait_pixel(&mut self, params: Value, x: u64, y: u64, expect: [u8; 4], what: &str) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let v = self.call("screen.capture", params.clone());
            if raw_pixel(&v, x, y) == Some(expect) {
                return;
            }
            assert!(Instant::now() < deadline, "等待像素超时：{what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn shutdown(mut self) {
        self.client.call0("server.shutdown").expect("shutdown 失败");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.compositor.0.try_wait().expect("wait 失败") {
                Some(status) => {
                    assert!(status.success(), "agentdesk 异常退出: {status}");
                    break;
                }
                None => {
                    assert!(Instant::now() < deadline, "agentdesk 未在时限内退出");
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn e2e_window_lifecycle() {
    let mut e2e = E2e::start("lifecycle");

    // 事件订阅（hello 事件会先进队列，一并消费）
    e2e.call("events.subscribe", json!({ "types": ["*"] }));

    // 初始无窗口
    let list = e2e.call("windows.list", json!({}));
    assert_eq!(list["windows"].as_array().unwrap().len(), 0, "初始应为空桌面");

    // 启动客户端 → window.created
    e2e.launch_app();
    let ev = e2e.wait_event("window.created");
    let window = match &ev {
        Event::WindowCreated { window } => serde_json::to_value(window).unwrap(),
        _ => unreachable!(),
    };
    assert_eq!(window["app_id"], "ad.test.client");
    assert_eq!(window["title"], "E2E Window");
    assert_eq!(window["origin"], "wayland");
    assert!(window["pid"].as_u64().unwrap() > 0, "应有客户端 pid");
    let id = window["id"].as_u64().unwrap();

    // 客户端绘制后几何应为 320x240
    let _ = e2e.wait_until("客户端首帧 320x240", |list| {
        list["windows"]
            .as_array()?
            .iter()
            .find(|w| w["id"] == window["id"])
            .filter(|w| w["geometry"]["width"] == 320 && w["geometry"]["height"] == 240)
            .cloned()
    });

    // set_geometry：合成器 configure → 客户端按新尺寸重绘
    e2e.call(
        "windows.set_geometry",
        json!({ "id": id, "x": 16, "y": 16, "width": 400, "height": 300 }),
    );
    let _ = e2e.wait_until("set_geometry 生效 400x300", |list| {
        list["windows"]
            .as_array()?
            .iter()
            .find(|w| w["id"] == window["id"])
            .filter(|w| {
                w["geometry"]["width"] == 400
                    && w["geometry"]["height"] == 300
                    && w["geometry"]["x"] == 16
                    && w["geometry"]["y"] == 16
            })
            .cloned()
    });

    // 焦点
    e2e.call("windows.focus", json!({ "id": id }));
    let focused = e2e.wait_until("窗口获得焦点", |list| {
        list["windows"]
            .as_array()?
            .iter()
            .find(|w| w["id"] == window["id"])
            .filter(|w| w["focused"] == true && w["states"].as_array().unwrap().iter().any(|s| s == "activated"))
            .cloned()
    });
    let _ = focused;

    // 关闭 → window.closed → 列表为空
    e2e.call("windows.close", json!({ "id": id }));
    let ev = e2e.wait_event("window.closed");
    match ev {
        Event::WindowClosed { id: closed } => assert_eq!(closed, id),
        _ => unreachable!(),
    }
    let list = e2e.call("windows.list", json!({}));
    assert_eq!(list["windows"].as_array().unwrap().len(), 0, "关闭后桌面应为空");

    // x11.status：无 satellite 时不可用
    let x11 = e2e.call("x11.status", json!({}));
    assert_eq!(x11["available"], false);

    // display.show：无父会话 → no_parent_session（M4 分支）
    let err = e2e
        .client
        .call0("display.show")
        .expect_err("无父会话时 display.show 应失败");
    match err {
        ad_ipc::client::ClientError::Rpc(e) => {
            assert_eq!(e.code, ad_ipc::error::codes::NO_PARENT_SESSION);
        }
        other => panic!("应返回 Rpc 错误，得到 {other:?}"),
    }

    // 优雅退出
    e2e.shutdown();
}

#[test]
fn e2e_input_injection() {
    let mut e2e = E2e::start("input");
    e2e.call("events.subscribe", json!({ "types": ["*"] }));
    e2e.launch_app();

    // 等待窗口出现并完成首帧（级联布局首窗位于 (32,32)，320x240）
    let created = e2e.wait_event("window.created");
    let id = match &created {
        Event::WindowCreated { window } => window.id,
        _ => unreachable!(),
    };
    let _ = e2e.wait_until("客户端首帧 320x240", |list| {
        list["windows"]
            .as_array()?
            .iter()
            .find(|w| w["id"] == json!(id))
            .filter(|w| w["geometry"]["width"] == 320)
            .cloned()
    });

    // 指针移动到窗口中心 (192,152) 并点击 → 客户端标题打 '*'
    e2e.call("input.move_cursor", json!({ "x": 192.0, "y": 152.0 }));
    e2e.call("input.click", json!({ "button": "left", "count": 1 }));
    let _ = e2e.wait_until("点击后标题含 *", |list| {
        list["windows"]
            .as_array()?
            .iter()
            .find(|w| w["id"] == json!(id))
            .filter(|w| {
                w["focused"] == true
                    && w["title"]
                        .as_str()
                        .map(|t| t.contains('*'))
                        .unwrap_or(false)
            })
            .cloned()
    });

    // type_text：键盘焦点已在点击后转移 → 标题追加 "hi"
    e2e.call("input.type_text", json!({ "text": "hi" }));
    let _ = e2e.wait_until("type_text 后标题追加 hi", |list| {
        list["windows"]
            .as_array()?
            .iter()
            .find(|w| w["id"] == json!(id))
            .filter(|w| w["title"] == json!("E2E Window*hi"))
            .cloned()
    });

    // 光标位置回读
    let cursor = e2e.call("input.get_cursor", json!({}));
    assert_eq!(cursor["x"], json!(192.0));
    assert_eq!(cursor["y"], json!(152.0));

    e2e.shutdown();
}

fn raw_pixel(v: &Value, x: u64, y: u64) -> Option<[u8; 4]> {
    use base64::Engine as _;
    let data = v.get("data").and_then(Value::as_str)?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).ok()?;
    let w = v.get("width").and_then(Value::as_u64)?;
    let idx = ((y * w + x) * 4) as usize;
    let px: [u8; 4] = bytes.get(idx..idx + 4)?.try_into().ok()?;
    Some(px)
}

#[test]
fn e2e_screenshots() {
    let mut e2e = E2e::start("capture");
    e2e.call("events.subscribe", json!({ "types": ["*"] }));
    e2e.launch_app();
    let created = e2e.wait_event("window.created");
    let id = match &created {
        Event::WindowCreated { window } => window.id,
        _ => unreachable!(),
    };

    // 等客户端首帧出现：整屏 (100,100)（窗口 32,32 起 320x240）应为红色（BGRA: B,G,R,A）
    e2e.wait_pixel(
        json!({ "format": "raw" }),
        100, 100, [0, 0, 255, 255],
        "首帧红色",
    );

    // 单窗口截图：中心 (160,120) 红色，且尺寸为 320x240
    let shot = e2e.call("windows.screenshot", json!({ "id": id, "format": "raw" }));
    assert_eq!(shot["width"], json!(320), "窗口截图宽度");
    assert_eq!(shot["height"], json!(240), "窗口截图高度");
    assert_eq!(raw_pixel(&shot, 160, 120), Some([0, 0, 255, 255]), "窗口中心红色");

    // 点击窗口中心换色 → 整屏中心变绿（证明注入与渲染一致）
    e2e.call("input.click", json!({ "x": 192.0, "y": 152.0, "button": "left" }));
    e2e.wait_pixel(
        json!({ "format": "raw" }),
        100, 100, [0, 255, 0, 255],
        "点击后绿色",
    );

    // png 编码可用
    let png = e2e.call("screen.capture", json!({ "format": "png" }));
    assert!(png["data"].as_str().map(|d| d.len() > 100).unwrap_or(false), "png base64 应有内容");

    // jpeg 编码可用
    let jpeg = e2e.call("screen.capture", json!({ "format": "jpeg", "quality": 80 }));
    assert_eq!(jpeg["format"], json!("jpeg"));

    e2e.shutdown();
}

#[test]
fn e2e_launch_layout_outputs() {
    let mut e2e = E2e::start("launch");
    e2e.call("events.subscribe", json!({ "types": ["*"] }));

    // windows.launch：等待窗口出现
    let r = e2e.call(
        "windows.launch",
        json!({ "argv": [env!("CARGO_BIN_EXE_ad-test-client"), "--title", "L1", "--app-id", "ad.l1"], "wait_for_window_ms": 5000 }),
    );
    assert!(r["pid"].as_u64().unwrap() > 0, "应返回 pid");
    let id1 = r["window_id"].as_u64().expect("应在时限内出现窗口");

    let r2 = e2e.call(
        "windows.launch",
        json!({ "argv": [env!("CARGO_BIN_EXE_ad-test-client"), "--title", "L2", "--app-id", "ad.l2"], "wait_for_window_ms": 5000 }),
    );
    let id2 = r2["window_id"].as_u64().expect("第二个窗口应出现");

    // layout.tile grid：640x480 输出、pad 8 → 两列各 308x464
    e2e.call("layout.tile", json!({ "direction": "grid", "padding": 8 }));
    let _ = e2e.wait_until("窗口1 网格左格", |list| {
        list["windows"]
            .as_array()?
            .iter()
            .find(|w| w["id"] == json!(id1))
            .filter(|w| {
                w["geometry"]["x"] == 8 && w["geometry"]["width"] == 308
            })
            .cloned()
    });
    let _ = e2e.wait_until("窗口2 网格右格", |list| {
        list["windows"]
            .as_array()?
            .iter()
            .find(|w| w["id"] == json!(id2))
            .filter(|w| {
                w["geometry"]["x"] == 324 && w["geometry"]["width"] == 308
            })
            .cloned()
    });

    // outputs 增删
    let created = e2e.call("outputs.create", json!({ "width": 640, "height": 480, "name": "HEADLESS-2" }));
    assert_eq!(created["id"], "HEADLESS-2");
    let list = e2e.call("outputs.list", json!({}));
    assert_eq!(list["outputs"].as_array().unwrap().len(), 2);
    e2e.call("outputs.destroy", json!({ "id": "HEADLESS-2" }));
    let list = e2e.call("outputs.list", json!({}));
    assert_eq!(list["outputs"].as_array().unwrap().len(), 1);

    e2e.shutdown();
}

#[test]
fn e2e_daemon_mode() {
    // 独立目录（与 E2e::start 一致的短路径，避免 SUN_LEN 超限）
    let dir_raw = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .join(format!("e2e-daemon-{}", std::process::id()));
    std::fs::create_dir_all(&dir_raw).expect("创建目录失败");
    let dir = dir_raw.canonicalize().unwrap();

    // --daemon：父进程拉起守护子进程，就绪后退出并打印 JSON
    let mut parent = Command::new(env!("CARGO_BIN_EXE_agentdesk"))
        .env("XDG_RUNTIME_DIR", &dir)
        .env("RUST_LOG", "info")
        .args(["--instance", "e2e", "--size", "640x480", "--daemon"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动 agentdesk --daemon 失败");
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match parent.try_wait().expect("wait 失败") {
            Some(s) => break s,
            None => {
                assert!(Instant::now() < deadline, "daemon 父进程未按时退出");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    assert!(status.success(), "daemon 父进程异常退出: {status}");
    let mut out = String::new();
    parent
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    let info: Value = serde_json::from_str(out.trim()).expect("父进程应打印 daemon JSON");
    assert_eq!(info["daemon"], json!(true));
    let pid = info["pid"].as_u64().expect("应返回 daemon pid");

    // 控制平面可用
    let sock = dir.join("agentdesk").join("e2e.sock");
    let mut client = Client::connect(&sock).expect("连接失败");
    client.call0("server.ping").expect("ping 失败");

    // PID 文件存在且内容一致
    let pidfile = dir.join("agentdesk").join("e2e.pid");
    let file_pid: u64 = std::fs::read_to_string(&pidfile)
        .expect("pid 文件应存在")
        .trim()
        .parse()
        .expect("pid 文件内容应为数字");
    assert_eq!(file_pid, pid);

    // shutdown：socket 与 pid 文件都被清理
    client.call0("server.shutdown").expect("shutdown 失败");
    let deadline = Instant::now() + TIMEOUT;
    while sock.exists() {
        assert!(Instant::now() < deadline, "daemon 未按时退出");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!pidfile.exists(), "pid 文件应被清理");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn e2e_server_info() {
    let mut e2e = E2e::start("info");
    let info = e2e.call("server.info", json!({}));
    assert_eq!(info["name"], "agentdesk");
    assert_eq!(info["api_version"], 1);
    assert_eq!(info["parent_session"], Value::Null, "测试无父会话环境");
    assert_eq!(info["outputs"][0]["id"], "HEADLESS-1");
    assert_eq!(info["outputs"][0]["width"], 640);
    assert_eq!(info["outputs"][0]["height"], 480);
    e2e.shutdown();
}

// 防止 unused 警告：Read 用于未来调试输出
#[allow(dead_code)]
fn drain<T: Read>(_r: T) {}
