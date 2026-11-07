//! 阻塞式控制客户端（adctl 与测试使用）。
//!
//! `call` 会跳过服务器推送的事件；`try_next_event` 用于 `watch`。

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::error::RpcError;
use crate::protocol::Event;
use crate::rpc::Id;
use crate::transport;

/// 服务器推送的一条消息。
#[derive(Debug, Clone)]
pub enum ServerMsg {
    Response { id: Option<Id>, result: Result<Value, RpcError> },
    Notification { method: String, params: Value },
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("传输错误: {0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Rpc(#[from] RpcError),
    #[error("{0}")]
    Protocol(String),
}

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    events: VecDeque<Event>,
}

const POLL_SLICE: Duration = Duration::from_millis(50);

impl Client {
    pub fn connect(path: &Path) -> io::Result<Client> {
        let stream = transport::connect(path)?;
        stream.set_read_timeout(Some(POLL_SLICE))?;
        let writer = stream.try_clone()?;
        Ok(Client {
            reader: BufReader::new(stream),
            writer,
            next_id: 1,
            events: VecDeque::new(),
        })
    }

    /// 发起一次调用并等待响应（期间到达的事件进入内部队列）。
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        self.call_raw(method, Some(params))
    }

    pub fn call0(&mut self, method: &str) -> Result<Value, ClientError> {
        self.call_raw(method, None)
    }

    pub fn call_raw(&mut self, method: &str, params: Option<Value>) -> Result<Value, ClientError> {
        let id = Id::Num(self.next_id);
        self.next_id += 1;
        let mut obj = serde_json::Map::new();
        obj.insert("jsonrpc".into(), Value::from(crate::rpc::JSONRPC));
        obj.insert("id".into(), serde_json::to_value(&id).unwrap());
        obj.insert("method".into(), Value::from(method));
        if let Some(p) = params {
            obj.insert("params".into(), p);
        }
        let mut line = serde_json::to_string(&Value::Object(obj))
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        line.push('\n');
        self.writer.write_all(line.as_bytes())?;
        self.writer.flush()?;
        loop {
            match self.read_msg(None)?.expect("deadline=None 不应超时") {
                ServerMsg::Response { id: rid, result } => {
                    if rid.as_ref() == Some(&id) {
                        return result.map_err(ClientError::Rpc);
                    }
                    // 其他 id 的响应（不应该出现）——忽略
                }
                ServerMsg::Notification { method, params } => {
                    self.push_notification(&method, params);
                }
            }
        }
    }

    /// 等待下一个事件；超时返回 None。
    pub fn try_next_event(&mut self, timeout: Duration) -> Result<Option<Event>, ClientError> {
        if let Some(ev) = self.events.pop_front() {
            return Ok(Some(ev));
        }
        let deadline = Instant::now() + timeout;
        loop {
            match self.read_msg(Some(deadline))? {
                None => return Ok(None),
                Some(ServerMsg::Response { .. }) => {
                    return Err(ClientError::Protocol("收到意外的响应（没有未决请求）".into()));
                }
                Some(ServerMsg::Notification { method, params }) => {
                    if let Some(ev) = parse_event(&method, &params) {
                        return Ok(Some(ev));
                    }
                }
            }
        }
    }

    fn push_notification(&mut self, method: &str, params: Value) {
        if let Some(ev) = parse_event(method, &params) {
            self.events.push_back(ev);
        }
    }

    fn parse_msg(&self, line: &str) -> Result<ServerMsg, ClientError> {
        let v: Value = serde_json::from_str(line)
            .map_err(|e| ClientError::Protocol(format!("非法 JSON 行: {e}")))?;
        if let Some(method) = v.get("method").and_then(Value::as_str) {
            let params = v.get("params").cloned().unwrap_or(Value::Null);
            return Ok(ServerMsg::Notification { method: method.to_owned(), params });
        }
        let id = v.get("id").and_then(|x| serde_json::from_value(x.clone()).ok());
        let result = if let Some(err) = v.get("error") {
            Err(serde_json::from_value::<RpcError>(err.clone())
                .map_err(|e| ClientError::Protocol(format!("非法错误对象: {e}")))?)
        } else if let Some(res) = v.get("result") {
            Ok(res.clone())
        } else {
            return Err(ClientError::Protocol("消息既无 result 也无 error".into()));
        };
        Ok(ServerMsg::Response { id, result })
    }

    /// 读一行。`deadline` 为 None 表示无限等待；超时返回 None。
    /// 跨超时的半行数据保留在 `buf` 中继续累积。
    fn read_msg(&mut self, deadline: Option<Instant>) -> Result<Option<ServerMsg>, ClientError> {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            match self.reader.read_until(b'\n', &mut buf) {
                Ok(0) => {
                    return Err(ClientError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "服务器关闭连接",
                    )))
                }
                Ok(_) => {
                    if buf.last() == Some(&b'\n') {
                        let line = std::str::from_utf8(&buf)
                            .map_err(|_| ClientError::Protocol("非 UTF-8 行".into()))?;
                        let t = line.trim();
                        if t.is_empty() {
                            buf.clear();
                            continue;
                        }
                        return self.parse_msg(t).map(Some);
                    }
                    // 行未完（异常情况）：继续读
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    if let Some(d) = deadline {
                        if Instant::now() >= d {
                            return Ok(None);
                        }
                    }
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

fn parse_event(method: &str, params: &Value) -> Option<Event> {
    if !method.starts_with("event/") {
        return None;
    }
    let wrapped = serde_json::json!({ "method": method, "params": params });
    serde_json::from_value(wrapped).ok()
}
