//! UDS 控制服务器。
//!
//! 架构：独立 IPC 线程负责 accept 与每连接读写（阻塞 I/O，每连接两个线程：
//! reader + writer），请求经 calloop Channel 投递到合成器主循环处理；
//! 响应与事件由主循环通过 [`IpcHub`] 写回对应连接的 writer 通道。
//! 线程边界只有消息传递，无共享可变状态（除连接注册表本身的短临界区）。

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use calloop::channel::{Channel, Sender};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::error::{codes, RpcError};
use crate::protocol::{pattern_matches, Event, MethodCall};
use crate::rpc::{Id, Incoming, Outgoing};
use crate::transport;

#[derive(Debug, Clone)]
pub struct IpcServerConfig {
    pub path: PathBuf,
    /// 访问令牌；Some 时要求客户端先成功调用 `auth`
    pub token: Option<String>,
    pub max_line: usize,
    pub max_connections: usize,
}

impl Default for IpcServerConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("agentdesk.sock"),
            token: None,
            max_line: 64 * 1024 * 1024,
            max_connections: 64,
        }
    }
}

/// 投递到主循环的请求。
#[derive(Debug)]
pub enum IpcRequest {
    /// 新连接（用于向该连接推送 server.hello）
    Connected { conn_id: u64 },
    Call { conn_id: u64, id: Id, call: MethodCall },
    /// 连接关闭（当前仅用于日志/统计）
    Gone { conn_id: u64 },
}

struct ConnShared {
    writer: mpsc::Sender<String>,
    authenticated: Arc<AtomicBool>,
    auth_failures: Arc<AtomicU64>,
    patterns: Arc<Mutex<Vec<String>>>,
}

type ConnMap = Arc<Mutex<HashMap<u64, ConnShared>>>;

/// 主循环侧句柄：回写响应、广播事件。
#[derive(Clone)]
pub struct IpcHub {
    conns: ConnMap,
}

impl IpcHub {
    /// 回写一个请求的响应。
    pub fn respond(&self, conn_id: u64, id: Option<Id>, outcome: Result<Value, RpcError>) {
        self.send(conn_id, Outgoing::response(id, outcome));
    }

    /// 向所有订阅了该事件类型的连接广播。
    pub fn broadcast(&self, ev: &Event) {
        let name = ev.type_name();
        let out = match ev.to_notification() {
            Ok(o) => o,
            Err(e) => {
                warn!("事件序列化失败 {name}: {e}");
                return;
            }
        };
        let line = match out.to_line() {
            Some(l) => l,
            None => return,
        };
        let conns = self.conns.lock().unwrap();
        for c in conns.values() {
            if subscribed(&c.patterns.lock().unwrap(), name) {
                let _ = c.writer.send(line.clone());
            }
        }
    }

    /// 向单个连接推送事件（如连接建立时的 server.hello）。
    pub fn notify(&self, conn_id: u64, ev: &Event) {
        let Ok(out) = ev.to_notification() else { return };
        let Some(line) = out.to_line() else { return };
        let writer = self.conns.lock().unwrap().get(&conn_id).map(|c| c.writer.clone());
        if let Some(w) = writer {
            let _ = w.send(line);
        }
    }

    pub fn set_authenticated(&self, conn_id: u64, ok: bool) {
        if let Some(c) = self.conns.lock().unwrap().get(&conn_id) {
            c.authenticated.store(ok, Ordering::Relaxed);
        }
    }

    pub fn set_subscriptions(&self, conn_id: u64, patterns: Vec<String>) {
        if let Some(c) = self.conns.lock().unwrap().get(&conn_id) {
            *c.patterns.lock().unwrap() = patterns;
        }
    }

    pub fn connection_count(&self) -> usize {
        self.conns.lock().unwrap().len()
    }

    fn send(&self, conn_id: u64, out: Outgoing) {
        let line = match out.to_line() {
            Some(l) => l,
            None => return,
        };
        let writer = self.conns.lock().unwrap().get(&conn_id).map(|c| c.writer.clone());
        if let Some(w) = writer {
            if w.send(line).is_err() {
                debug!(conn_id, "writer 已退出，响应丢弃");
            }
        }
    }
}

fn subscribed(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|p| pattern_matches(p, name))
}

/// 控制服务器：把 `channel` 插入 calloop 主循环即可消费 [`IpcRequest`]。
pub struct IpcServer {
    pub channel: Channel<IpcRequest>,
    pub handle: IpcServerHandle,
}

/// 线程生命周期句柄：主循环结束后调用 [`IpcServerHandle::shutdown`]。
pub struct IpcServerHandle {
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
    path: PathBuf,
}

impl IpcServerHandle {
    /// 停止 accept 线程、断开全部连接并清理 socket 文件。
    pub fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(t) = self.accept_thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

impl IpcServer {
    /// 绑定 socket 并启动 IPC 线程。
    pub fn bind(cfg: IpcServerConfig) -> io::Result<(IpcServer, IpcHub)> {
        let listener = transport::bind_control_socket(&cfg.path)?;
        let (tx, channel) = calloop::channel::channel::<IpcRequest>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let conns: ConnMap = Arc::default();
        let next_id = Arc::new(AtomicU64::new(1));
        let cfg = Arc::new(cfg);
        let hub = IpcHub { conns: conns.clone() };
        let path = cfg.path.clone();
        let shutdown_thread = shutdown.clone();
        let thread = std::thread::Builder::new()
            .name("ad-ipc-accept".into())
            .spawn(move || accept_loop(listener, cfg, conns, tx, shutdown_thread, next_id))
            .map_err(io::Error::other)?;
        info!("IPC server listening on {}", path.display());
        Ok((
            IpcServer {
                channel,
                handle: IpcServerHandle { shutdown, accept_thread: Some(thread), path },
            },
            hub,
        ))
    }
}

fn accept_loop(
    listener: UnixListener,
    cfg: Arc<IpcServerConfig>,
    conns: ConnMap,
    tx: Sender<IpcRequest>,
    shutdown: Arc<AtomicBool>,
    next_id: Arc<AtomicU64>,
) {
    listener
        .set_nonblocking(true)
        .expect("listener nonblocking");
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => setup_conn(stream, &cfg, &conns, &tx, &next_id),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(80));
            }
            Err(e) => {
                warn!("accept 失败: {e}");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    // 收尾：丢弃所有连接（writer 通道关闭 → 客户端读到 EOF）
    conns.lock().unwrap().clear();
}

fn setup_conn(
    mut stream: UnixStream,
    cfg: &Arc<IpcServerConfig>,
    conns: &ConnMap,
    tx: &Sender<IpcRequest>,
    next_id: &Arc<AtomicU64>,
) {
    if conns.lock().unwrap().len() >= cfg.max_connections {
        let line = Outgoing::response(None, Err(RpcError::new(codes::BUSY, "连接数已达上限")))
            .to_line()
            .unwrap_or_default();
        let _ = stream.write_all(line.as_bytes());
        return;
    }
    let conn_id = next_id.fetch_add(1, Ordering::Relaxed);
    let (wtx, wrx) = mpsc::channel::<String>();
    let write_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            debug!(conn_id, "clone stream 失败: {e}");
            return;
        }
    };
    let _ = std::thread::Builder::new()
        .name(format!("ad-ipc-w{conn_id}"))
        .spawn(move || writer_loop(write_stream, wrx));

    let shared = ConnShared {
        writer: wtx,
        authenticated: Arc::new(AtomicBool::new(cfg.token.is_none())),
        auth_failures: Arc::new(AtomicU64::new(0)),
        patterns: Arc::new(Mutex::new(Vec::new())),
    };
    conns.lock().unwrap().insert(conn_id, shared);
    debug!(conn_id, "连接建立");
    let _ = tx.send(IpcRequest::Connected { conn_id });

    let cfg2 = cfg.clone();
    let conns2 = conns.clone();
    let tx2 = tx.clone();
    let authenticated = {
        let c = conns.lock().unwrap();
        c.get(&conn_id).map(|s| s.authenticated.clone()).unwrap()
    };
    let _ = std::thread::Builder::new()
        .name(format!("ad-ipc-r{conn_id}"))
        .spawn(move || conn_loop(conn_id, stream, cfg2, conns2, tx2, authenticated));
}

fn writer_loop(mut stream: UnixStream, rx: mpsc::Receiver<String>) {
    for line in rx {
        if stream.write_all(line.as_bytes()).is_err() || stream.flush().is_err() {
            break;
        }
    }
}

#[derive(PartialEq)]
enum Control {
    Continue,
    Close,
}

fn conn_loop(
    conn_id: u64,
    stream: UnixStream,
    cfg: Arc<IpcServerConfig>,
    conns: ConnMap,
    tx: Sender<IpcRequest>,
    authenticated: Arc<AtomicBool>,
) {
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        let limit = cfg.max_line as u64;
        let mut bounded = (&mut reader).take(limit.saturating_add(1));
        match bounded.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {
                if line.len() as u64 > limit {
                    warn!(conn_id, "单行超过上限，断开");
                    break;
                }
                let t = line.trim();
                if t.is_empty() {
                    continue;
                }
                if handle_line(conn_id, t, &cfg, &conns, &tx, &authenticated) == Control::Close {
                    break;
                }
            }
            Err(e) => {
                debug!(conn_id, "读取错误: {e}");
                break;
            }
        }
    }
    conns.lock().unwrap().remove(&conn_id);
    debug!(conn_id, "连接关闭");
    let _ = tx.send(IpcRequest::Gone { conn_id });
}

fn handle_line(
    conn_id: u64,
    line: &str,
    cfg: &Arc<IpcServerConfig>,
    conns: &ConnMap,
    tx: &Sender<IpcRequest>,
    authenticated: &Arc<AtomicBool>,
) -> Control {
    let raw: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            direct_respond(conns, conn_id, None, Err(RpcError::new(codes::PARSE, format!("解析失败: {e}"))));
            return Control::Continue;
        }
    };
    let msg_id: Option<Id> = raw.get("id").and_then(|v| serde_json::from_value(v.clone()).ok());
    let incoming: Incoming = match serde_json::from_value(raw.clone()) {
        Ok(i) => i,
        Err(e) => {
            direct_respond(conns, conn_id, msg_id, Err(RpcError::new(codes::INVALID_REQUEST, format!("非法消息: {e}"))));
            return Control::Continue;
        }
    };
    if incoming.jsonrpc != crate::rpc::JSONRPC {
        direct_respond(conns, conn_id, msg_id, Err(RpcError::new(codes::INVALID_REQUEST, "jsonrpc 必须为 \"2.0\"")));
        return Control::Continue;
    }
    let Some(id) = incoming.id else {
        // v1 忽略客户端 notification
        debug!(conn_id, "忽略 notification: {}", incoming.method);
        return Control::Continue;
    };

    // 鉴权闸门（auth 方法本身放行）
    if cfg.token.is_some() && incoming.method != "auth" && !authenticated.load(Ordering::Relaxed) {
        let failures = {
            let m = conns.lock().unwrap();
            match m.get(&conn_id) {
                Some(c) => {
                    let f = c.auth_failures.fetch_add(1, Ordering::Relaxed) + 1;
                    Some(f)
                }
                None => None,
            }
        };
        let f = failures.unwrap_or(0);
        direct_respond(
            conns,
            conn_id,
            Some(id),
            Err(RpcError::new(
                codes::PERMISSION_DENIED,
                format!("需要先 auth（已失败 {f} 次）"),
            )),
        );
        return if f >= 3 { Control::Close } else { Control::Continue };
    }

    match MethodCall::parse(&incoming.method, incoming.params.as_ref()) {
        Ok(call) => {
            if tx.send(IpcRequest::Call { conn_id, id, call }).is_err() {
                // 主循环已退出
                return Control::Close;
            }
            Control::Continue
        }
        Err(e) => {
            direct_respond(conns, conn_id, Some(id), Err(e));
            Control::Continue
        }
    }
}

fn direct_respond(conns: &ConnMap, conn_id: u64, id: Option<Id>, outcome: Result<Value, RpcError>) {
    let out = Outgoing::response(id, outcome);
    if let Some(line) = out.to_line() {
        let writer = conns.lock().unwrap().get(&conn_id).map(|c| c.writer.clone());
        if let Some(w) = writer {
            let _ = w.send(line);
        }
    }
}
