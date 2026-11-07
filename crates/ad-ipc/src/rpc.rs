//! JSON-RPC 2.0 线路消息（NDJSON：每条消息一行 UTF-8 JSON）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::RpcError;

pub const JSONRPC: &str = "2.0";

/// JSON-RPC 请求/响应 id。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Num(u64),
    Str(String),
    Null,
}

/// 客户端入站消息（请求或 notification）。
#[derive(Debug, Clone, Deserialize)]
pub struct Incoming {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
    #[serde(default)]
    pub id: Option<Id>,
}

impl Incoming {
    /// v1 中客户端 notification 一律忽略（保留语义以便将来扩展）。
    pub fn is_request(&self) -> bool {
        self.id.is_some()
    }
}

/// 服务端出站消息：响应（带 id）或通知/事件（带 method/params）。
#[derive(Debug, Clone)]
pub enum Outgoing {
    Response { id: Option<Id>, result: Result<Value, RpcError> },
    Notification { method: String, params: Value },
}

impl Outgoing {
    pub fn response(id: Option<Id>, result: Result<Value, RpcError>) -> Self {
        Outgoing::Response { id, result }
    }

    pub fn notification(method: impl Into<String>, params: Value) -> Self {
        Outgoing::Notification { method: method.into(), params }
    }

    /// 编码为一行（含换行符）。序列化失败返回 None（不应发生在本 crate 的类型上）。
    pub fn to_line(&self) -> Option<String> {
        let mut map = serde_json::Map::new();
        map.insert("jsonrpc".into(), Value::from(JSONRPC));
        match self {
            Outgoing::Response { id, result } => {
                match result {
                    Ok(v) => {
                        map.insert("result".into(), v.clone());
                    }
                    Err(e) => {
                        let ev = serde_json::to_value(e).ok()?;
                        map.insert("error".into(), ev);
                    }
                }
                // None 序列化为 null：符合 JSON-RPC（如 parse error）约定
                let idv = match id {
                    Some(i) => serde_json::to_value(i).ok()?,
                    None => Value::Null,
                };
                map.insert("id".into(), idv);
            }
            Outgoing::Notification { method, params } => {
                map.insert("method".into(), Value::from(method.clone()));
                map.insert("params".into(), params.clone());
            }
        }
        let mut s = serde_json::to_string(&Value::Object(map)).ok()?;
        s.push('\n');
        Some(s)
    }
}
