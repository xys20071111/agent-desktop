//! agent-desktop 控制平面协议与传输（UDS + JSON-RPC 2.0，NDJSON 帧）。
//!
//! 字段与方法约定见 `docs/CONTROL_API.md`。本 crate 不依赖 smithay，
//! 服务端（agentdesk）与客户端（adctl / Agent 框架）共用这里的类型。

pub mod client;
pub mod error;
pub mod protocol;
pub mod rpc;
pub mod server;
pub mod transport;

/// 控制协议版本（`server.info.api_version`）
pub const API_VERSION: u32 = 1;

/// 默认实例名
pub const DEFAULT_INSTANCE: &str = "main";

/// 默认控制 socket 路径：`$XDG_RUNTIME_DIR/agentdesk/<instance>.sock`
pub fn default_socket_path(instance: &str) -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    let mut p = std::path::PathBuf::from(dir);
    p.push("agentdesk");
    p.push(format!("{instance}.sock"));
    Some(p)
}
