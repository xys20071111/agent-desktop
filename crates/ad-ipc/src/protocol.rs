//! 控制协议类型：请求参数（v1 全量）与事件。
//!
//! 方法编码为 JSON-RPC 的 `method` + `params`；`MethodCall` 由
//! [`MethodCall::parse`] 手工分发（比 serde 内部标签更可控：未知方法 →
//! METHOD_NOT_FOUND，参数错误 → INVALID_PARAMS，params 缺省视作空对象）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{codes, RpcError};

// ---------- 基础几何 ----------

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct PosJson {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct SizeJson {
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct GeometryJson {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

// ---------- 窗口 ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowKind {
    Toplevel,
    Layer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowOrigin {
    Wayland,
    X11,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowState {
    Activated,
    Fullscreen,
    Maximized,
    Minimized,
    Resizing,
}

/// `windows.list/get` 返回的窗口描述。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowJson {
    pub id: u64,
    pub kind: WindowKind,
    pub origin: WindowOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    pub pid: u32,
    pub geometry: GeometryJson,
    pub surface_size: SizeJson,
    pub states: Vec<WindowState>,
    pub output: String,
    pub focused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_size: Option<SizeJson>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_size: Option<SizeJson>,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WindowFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<WindowOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<WindowKind>,
}

impl WindowFilter {
    pub fn matches(&self, w: &WindowJson) -> bool {
        if let Some(a) = &self.app_id {
            if w.app_id.as_deref() != Some(a.as_str()) {
                return false;
            }
        }
        if let Some(t) = &self.title_contains {
            match &w.title {
                Some(title) if title.contains(t.as_str()) => {}
                _ => return false,
            }
        }
        if let Some(o) = &self.origin {
            if w.origin != *o {
                return false;
            }
        }
        if let Some(o) = &self.output {
            if w.output != *o {
                return false;
            }
        }
        if let Some(k) = &self.kind {
            if w.kind != *k {
                return false;
            }
        }
        true
    }
}

// ---------- 服务器信息 ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewerInfoJson {
    pub visible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<SizeJson>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputJson {
    pub id: String,
    pub width: i32,
    pub height: i32,
    pub scale: f64,
    pub position: PosJson,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct X11StatusJson {
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfoJson {
    pub name: String,
    pub version: String,
    pub api_version: u32,
    pub pid: u32,
    pub uptime_ms: u64,
    pub socket: String,
    pub wayland_display: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    pub viewer: ViewerInfoJson,
    pub renderer: String,
    pub outputs: Vec<OutputJson>,
    pub x11: X11StatusJson,
}

// ---------- 截图 ----------

#[derive(Debug, Clone, Default)]
pub struct CaptureOptions {
    pub format: Option<String>,
    pub quality: Option<u8>,
    pub include_cursor: Option<bool>,
    pub to_path: Option<String>,
    pub via_fd: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageEncoding {
    Base64,
    File,
    Fd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageResultJson {
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub encoding: ImageEncoding,
    /// base64 数据（encoding=base64 时存在）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// 文件路径（encoding=file 时存在）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

// ---------- 方法 ----------

/// 全部 v1 控制方法。
#[derive(Debug, Clone)]
pub enum MethodCall {
    ServerInfo,
    ServerPing,
    ServerShutdown,
    Auth { token: String },

    WindowsList { filter: Option<WindowFilter> },
    WindowsGet { id: u64 },
    WindowsClose { id: u64 },
    WindowsFocus { id: u64 },
    WindowsRaise { id: u64 },
    WindowsSetGeometry {
        id: u64,
        x: Option<i32>,
        y: Option<i32>,
        width: Option<i32>,
        height: Option<i32>,
    },
    WindowsLaunch {
        argv: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
        x11: bool,
        wait_for_window_ms: Option<u64>,
    },

    ScreenCapture {
        output: Option<String>,
        opts: CaptureOptions,
    },
    WindowsScreenshot { id: u64, opts: CaptureOptions },

    InputMoveCursor { x: f64, y: f64 },
    InputMoveRelative { dx: f64, dy: f64 },
    InputButton { button: String, action: String },
    InputClick {
        x: Option<f64>,
        y: Option<f64>,
        button: Option<String>,
        count: Option<u32>,
    },
    InputScroll {
        dx: Option<f64>,
        dy: Option<f64>,
        x: Option<f64>,
        y: Option<f64>,
    },
    InputKey { key: String, action: Option<String> },
    InputCombo { keys: Vec<String>, hold_ms: Option<u64> },
    InputTypeText { text: String, delay_ms_per_key: Option<u64> },
    InputGetCursor,

    DisplayShow {
        output: Option<String>,
        width: Option<u32>,
        height: Option<u32>,
        scale: Option<f64>,
        title: Option<String>,
    },
    DisplayHide,
    DisplayStatus,

    OutputsList,
    OutputsCreate { width: i32, height: i32, name: Option<String> },
    OutputsDestroy { id: String },
    LayoutTile { output: Option<String>, direction: Option<String>, padding: Option<i32> },
    LayoutCascade { output: Option<String>, offset: Option<i32> },

    ClipboardGet { which: Option<String> },
    ClipboardSet { text: String },

    EventsSubscribe { types: Vec<String> },
    EventsUnsubscribe { subscription_id: u64 },

    X11Status,
}

impl MethodCall {
    /// 从 JSON-RPC 入站消息解析方法调用。
    pub fn parse(method: &str, params: Option<&Value>) -> Result<MethodCall, RpcError> {
        let empty = serde_json::Map::new();
        let m = match params {
            Some(Value::Object(m)) => m,
            Some(Value::Null) | None => &empty,
            Some(_) => return Err(RpcError::invalid_params("params 必须是对象")),
        };

        let call = match method {
            "server.info" => MethodCall::ServerInfo,
            "server.ping" => MethodCall::ServerPing,
            "server.shutdown" => MethodCall::ServerShutdown,
            "auth" => MethodCall::Auth { token: req_string(m, "token")? },

            "windows.list" => MethodCall::WindowsList {
                filter: match m.get("filter") {
                    Some(Value::Null) | None => None,
                    Some(v) => Some(serde_json::from_value(v.clone())
                        .map_err(|e| RpcError::invalid_params(format!("filter: {e}")))?),
                },
            },
            "windows.get" => MethodCall::WindowsGet { id: req_u64(m, "id")? },
            "windows.close" => MethodCall::WindowsClose { id: req_u64(m, "id")? },
            "windows.focus" => MethodCall::WindowsFocus { id: req_u64(m, "id")? },
            "windows.raise" => MethodCall::WindowsRaise { id: req_u64(m, "id")? },
            "windows.set_geometry" => MethodCall::WindowsSetGeometry {
                id: req_u64(m, "id")?,
                x: opt_i32(m, "x")?,
                y: opt_i32(m, "y")?,
                width: opt_i32(m, "width")?,
                height: opt_i32(m, "height")?,
            },
            "windows.launch" => MethodCall::WindowsLaunch {
                argv: req_string_vec(m, "argv")?,
                cwd: opt_string(m, "cwd")?,
                env: opt_string_pairs(m, "env")?,
                x11: opt_bool(m, "x11")?.unwrap_or(false),
                wait_for_window_ms: opt_u64(m, "wait_for_window_ms")?,
            },

            "screen.capture" => MethodCall::ScreenCapture {
                output: opt_string(m, "output")?,
                opts: parse_capture(m)?,
            },
            "windows.screenshot" => MethodCall::WindowsScreenshot {
                id: req_u64(m, "id")?,
                opts: parse_capture(m)?,
            },

            "input.move_cursor" => MethodCall::InputMoveCursor {
                x: req_f64(m, "x")?,
                y: req_f64(m, "y")?,
            },
            "input.move_relative" => MethodCall::InputMoveRelative {
                dx: req_f64(m, "dx")?,
                dy: req_f64(m, "dy")?,
            },
            "input.button" => MethodCall::InputButton {
                button: req_string(m, "button")?,
                action: req_string(m, "action")?,
            },
            "input.click" => MethodCall::InputClick {
                x: opt_f64(m, "x")?,
                y: opt_f64(m, "y")?,
                button: opt_string(m, "button")?,
                count: opt_u32(m, "count")?,
            },
            "input.scroll" => MethodCall::InputScroll {
                dx: opt_f64(m, "dx")?,
                dy: opt_f64(m, "dy")?,
                x: opt_f64(m, "x")?,
                y: opt_f64(m, "y")?,
            },
            "input.key" => MethodCall::InputKey {
                key: req_string(m, "key")?,
                action: opt_string(m, "action")?,
            },
            "input.combo" => MethodCall::InputCombo {
                keys: req_string_vec(m, "keys")?,
                hold_ms: opt_u64(m, "hold_ms")?,
            },
            "input.type_text" => MethodCall::InputTypeText {
                text: req_string(m, "text")?,
                delay_ms_per_key: opt_u64(m, "delay_ms_per_key")?,
            },
            "input.get_cursor" => MethodCall::InputGetCursor,

            "display.show" => MethodCall::DisplayShow {
                output: opt_string(m, "output")?,
                width: opt_u32(m, "width")?,
                height: opt_u32(m, "height")?,
                scale: opt_f64(m, "scale")?,
                title: opt_string(m, "title")?,
            },
            "display.hide" => MethodCall::DisplayHide,
            "display.status" => MethodCall::DisplayStatus,

            "outputs.list" => MethodCall::OutputsList,
            "outputs.create" => MethodCall::OutputsCreate {
                width: req_i32(m, "width")?,
                height: req_i32(m, "height")?,
                name: opt_string(m, "name")?,
            },
            "outputs.destroy" => MethodCall::OutputsDestroy { id: req_string(m, "id")? },
            "layout.tile" => MethodCall::LayoutTile {
                output: opt_string(m, "output")?,
                direction: opt_string(m, "direction")?,
                padding: opt_i32(m, "padding")?,
            },
            "layout.cascade" => MethodCall::LayoutCascade {
                output: opt_string(m, "output")?,
                offset: opt_i32(m, "offset")?,
            },

            "clipboard.get" => MethodCall::ClipboardGet { which: opt_string(m, "which")? },
            "clipboard.set" => MethodCall::ClipboardSet { text: req_string(m, "text")? },

            "events.subscribe" => MethodCall::EventsSubscribe { types: req_string_vec(m, "types")? },
            "events.unsubscribe" => MethodCall::EventsUnsubscribe { subscription_id: req_u64(m, "subscription_id")? },

            "x11.status" => MethodCall::X11Status,

            other => {
                return Err(RpcError::new(
                    codes::METHOD_NOT_FOUND,
                    format!("未知方法 {other}"),
                ))
            }
        };
        Ok(call)
    }
}

fn parse_capture(m: &serde_json::Map<String, Value>) -> Result<CaptureOptions, RpcError> {
    Ok(CaptureOptions {
        format: opt_string(m, "format")?,
        quality: opt_u8(m, "quality")?,
        include_cursor: opt_bool(m, "include_cursor")?,
        to_path: opt_string(m, "to_path")?,
        via_fd: opt_bool(m, "via_fd")?,
    })
}

fn req<'a>(m: &'a serde_json::Map<String, Value>, k: &str) -> Result<&'a Value, RpcError> {
    match m.get(k) {
        Some(v) if !v.is_null() => Ok(v),
        _ => Err(RpcError::invalid_params(format!("缺少字段 {k}"))),
    }
}

fn req_u64(m: &serde_json::Map<String, Value>, k: &str) -> Result<u64, RpcError> {
    req(m, k)?
        .as_u64()
        .ok_or_else(|| RpcError::invalid_params(format!("字段 {k} 应为非负整数")))
}

fn req_i32(m: &serde_json::Map<String, Value>, k: &str) -> Result<i32, RpcError> {
    req(m, k)?
        .as_i64()
        .and_then(|v| i32::try_from(v).ok())
        .ok_or_else(|| RpcError::invalid_params(format!("字段 {k} 应为整数")))
}

fn req_f64(m: &serde_json::Map<String, Value>, k: &str) -> Result<f64, RpcError> {
    req(m, k)?
        .as_f64()
        .ok_or_else(|| RpcError::invalid_params(format!("字段 {k} 应为数字")))
}

fn req_string(m: &serde_json::Map<String, Value>, k: &str) -> Result<String, RpcError> {
    req(m, k)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| RpcError::invalid_params(format!("字段 {k} 应为字符串")))
}

fn req_string_vec(m: &serde_json::Map<String, Value>, k: &str) -> Result<Vec<String>, RpcError> {
    req(m, k)?
        .as_array()
        .ok_or_else(|| RpcError::invalid_params(format!("字段 {k} 应为字符串数组")))?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| RpcError::invalid_params(format!("字段 {k} 应为字符串数组")))
        })
        .collect()
}

fn opt_u64(m: &serde_json::Map<String, Value>, k: &str) -> Result<Option<u64>, RpcError> {
    match m.get(k) {
        Some(Value::Null) | None => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| RpcError::invalid_params(format!("字段 {k} 应为非负整数"))),
    }
}

fn opt_u32(m: &serde_json::Map<String, Value>, k: &str) -> Result<Option<u32>, RpcError> {
    match m.get(k) {
        Some(Value::Null) | None => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|x| u32::try_from(x).ok())
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("字段 {k} 溢出"))),
    }
}

fn opt_u8(m: &serde_json::Map<String, Value>, k: &str) -> Result<Option<u8>, RpcError> {
    match m.get(k) {
        Some(Value::Null) | None => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|x| u8::try_from(x).ok())
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("字段 {k} 应为 1..=100"))),
    }
}

fn opt_i32(m: &serde_json::Map<String, Value>, k: &str) -> Result<Option<i32>, RpcError> {
    match m.get(k) {
        Some(Value::Null) | None => Ok(None),
        Some(v) => v
            .as_i64()
            .and_then(|x| i32::try_from(x).ok())
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("字段 {k} 应为整数"))),
    }
}

fn opt_f64(m: &serde_json::Map<String, Value>, k: &str) -> Result<Option<f64>, RpcError> {
    match m.get(k) {
        Some(Value::Null) | None => Ok(None),
        Some(v) => v.as_f64().map(Some).ok_or_else(|| RpcError::invalid_params(format!("字段 {k} 应为数字"))),
    }
}

fn opt_bool(m: &serde_json::Map<String, Value>, k: &str) -> Result<Option<bool>, RpcError> {
    match m.get(k) {
        Some(Value::Null) | None => Ok(None),
        Some(v) => v.as_bool().map(Some).ok_or_else(|| RpcError::invalid_params(format!("字段 {k} 应为布尔"))),
    }
}

fn opt_string(m: &serde_json::Map<String, Value>, k: &str) -> Result<Option<String>, RpcError> {
    match m.get(k) {
        Some(Value::Null) | None => Ok(None),
        Some(v) => v.as_str().map(str::to_owned).map(Some).ok_or_else(|| RpcError::invalid_params(format!("字段 {k} 应为字符串"))),
    }
}

fn opt_string_pairs(
    m: &serde_json::Map<String, Value>,
    k: &str,
) -> Result<Vec<(String, String)>, RpcError> {
    match m.get(k) {
        Some(Value::Null) | None => Ok(Vec::new()),
        Some(Value::Object(obj)) => obj
            .iter()
            .map(|(k2, v)| {
                let s = v.as_str().ok_or_else(|| RpcError::invalid_params(format!("env.{k2} 应为字符串")))?;
                Ok((k2.clone(), s.to_owned()))
            })
            .collect(),
        Some(_) => Err(RpcError::invalid_params(format!("字段 {k} 应为对象"))),
    }
}

// ---------- 事件 ----------

/// 服务端推送事件（`event/` 前缀的 notification）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Event {
    #[serde(rename = "event/server.hello")]
    ServerHello { info: ServerInfoJson },
    #[serde(rename = "event/window.created")]
    WindowCreated { window: WindowJson },
    #[serde(rename = "event/window.updated")]
    WindowUpdated {
        id: u64,
        changed: Vec<String>,
        window: WindowJson,
    },
    #[serde(rename = "event/window.closed")]
    WindowClosed { id: u64 },
    #[serde(rename = "event/focus.changed")]
    FocusChanged { previous_id: Option<u64>, id: Option<u64> },
    #[serde(rename = "event/display.changed")]
    DisplayChanged { visible: bool },
    #[serde(rename = "event/output.added")]
    OutputAdded { output: OutputJson },
    #[serde(rename = "event/output.removed")]
    OutputRemoved { id: String },
}

impl Event {
    /// 订阅匹配用的事件类型名（去掉 `event/` 前缀）。
    pub fn type_name(&self) -> &'static str {
        match self {
            Event::ServerHello { .. } => "server.hello",
            Event::WindowCreated { .. } => "window.created",
            Event::WindowUpdated { .. } => "window.updated",
            Event::WindowClosed { .. } => "window.closed",
            Event::FocusChanged { .. } => "focus.changed",
            Event::DisplayChanged { .. } => "display.changed",
            Event::OutputAdded { .. } => "output.added",
            Event::OutputRemoved { .. } => "output.removed",
        }
    }

    pub fn to_notification(&self) -> Result<crate::rpc::Outgoing, serde_json::Error> {
        let params = serde_json::to_value(self)?;
        // to_value(self) 得到 {"method": …, "params": …}（tag/content 编码），拆开复用
        let method = params
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let params = params.get("params").cloned().unwrap_or(Value::Null);
        Ok(crate::rpc::Outgoing::notification(method, params))
    }
}

/// 订阅模式匹配：`*`、精确名或段级通配 `window.*`。
pub fn pattern_matches(pattern: &str, name: &str) -> bool {
    pattern == "*" || pattern == name || {
        if let Some(prefix) = pattern.strip_suffix('*') {
            name.starts_with(prefix) && !name[prefix.len()..].contains('.')
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_methods() {
        let c = MethodCall::parse("server.info", None).unwrap();
        assert!(matches!(c, MethodCall::ServerInfo));

        let c = MethodCall::parse(
            "windows.set_geometry",
            Some(&serde_json::json!({"id": 3, "width": 800})),
        )
        .unwrap();
        assert!(matches!(
            c,
            MethodCall::WindowsSetGeometry { id: 3, width: Some(800), .. }
        ));

        let err = MethodCall::parse("nope.nope", None).unwrap_err();
        assert_eq!(err.code, codes::METHOD_NOT_FOUND);

        let err = MethodCall::parse("windows.get", None).unwrap_err();
        assert_eq!(err.code, codes::INVALID_PARAMS);
    }

    #[test]
    fn patterns() {
        assert!(pattern_matches("*", "window.created"));
        assert!(pattern_matches("window.*", "window.created"));
        assert!(pattern_matches("window.created", "window.created"));
        assert!(!pattern_matches("window.*", "window.created.deep"));
        assert!(!pattern_matches("focus.changed", "window.created"));
    }

    #[test]
    fn event_roundtrip() {
        let ev = Event::WindowClosed { id: 5 };
        let out = ev.to_notification().unwrap();
        let line = out.to_line().unwrap();
        assert!(line.contains("event/window.closed"));
        assert!(line.contains("\"id\":5"));
    }
}
