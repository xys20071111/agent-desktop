//! JSON-RPC 错误码与构造辅助（见 docs/CONTROL_API.md §2）。

use serde::{Deserialize, Serialize};

/// 协议错误码。
pub mod codes {
    pub const PARSE: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL: i32 = -32603;
    /// 应用层通用错误
    pub const APP: i32 = -32000;
    pub const WINDOW_NOT_FOUND: i32 = -32001;
    pub const OUTPUT_NOT_FOUND: i32 = -32002;
    pub const CAPTURE_FAILED: i32 = -32003;
    pub const NO_PARENT_SESSION: i32 = -32004;
    pub const LAUNCH_FAILED: i32 = -32005;
    pub const UNSUPPORTED_CHAR: i32 = -32006;
    pub const PERMISSION_DENIED: i32 = -32007;
    pub const BUSY: i32 = -32008;
    pub const NOT_IMPLEMENTED: i32 = -32009;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl RpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self::new(codes::INVALID_PARAMS, msg)
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(codes::INTERNAL, msg)
    }

    pub fn app(msg: impl Into<String>) -> Self {
        Self::new(codes::APP, msg)
    }

    pub fn window_not_found(id: u64) -> Self {
        Self::new(codes::WINDOW_NOT_FOUND, format!("窗口 {id} 不存在"))
            .with_data(serde_json::json!({ "window_id": id }))
    }

    pub fn not_implemented(method: &str) -> Self {
        Self::new(codes::NOT_IMPLEMENTED, format!("方法 {method} 尚未实现"))
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)?;
        if let Some(d) = &self.data {
            write!(f, " ({d})")?;
        }
        Ok(())
    }
}

impl std::error::Error for RpcError {}
