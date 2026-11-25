//! 控制平面方法分发：IpcRequest → AgentDesk 操作 → 响应/事件。

use serde_json::{json, Value};

use ad_ipc::error::{codes, RpcError};
use ad_ipc::protocol::MethodCall;
use ad_ipc::server::IpcRequest;

use crate::state::{now_ms, AgentDesk};
use smithay::wayland::selection::data_device::set_data_device_selection;
use smithay::wayland::selection::primary_selection::set_primary_selection;

pub fn handle_ipc(state: &mut AgentDesk, req: IpcRequest) {
    match req {
        IpcRequest::Connected { conn_id } => {
            let info = state.server_info();
            state
                .hub
                .notify(conn_id, &ad_ipc::protocol::Event::ServerHello { info });
        }
        IpcRequest::Call { conn_id, id, call } => {
            let fd_capture = match &call {
                MethodCall::ScreenCapture { output, opts } if opts.via_fd == Some(true) => Some((None, output.clone(), opts.clone())),
                MethodCall::WindowsScreenshot { id: wid, opts } if opts.via_fd == Some(true) => Some((Some(*wid), None, opts.clone())),
                _ => None,
            };
            if let Some((window_id, output, opts)) = fd_capture {
                let outcome = (|| {
                    let raw = if let Some(wid) = window_id { state.window_capture(wid)? } else { state.screen_capture(output.as_deref())? };
                    let (result, fd) = state.encode_capture_fd(raw, &opts)?;
                    let value = serde_json::to_value(result).map_err(|e| RpcError::internal(e.to_string()))?;
                    Ok((value, fd))
                })();
                match outcome {
                    Ok((value, fd)) => state.hub.respond_with_fd(conn_id, id, Ok(value), fd),
                    Err(error) => state.hub.respond(conn_id, Some(id), Err(error)),
                }
                return;
            }
            if let MethodCall::ClipboardGet { which } = call {
                match clipboard_get(
                    state,
                    conn_id,
                    id.clone(),
                    which.as_deref().unwrap_or("regular"),
                ) {
                    Ok(Some(value)) => state.hub.respond(conn_id, Some(id), Ok(value)),
                    Ok(None) => {}
                    Err(error) => state.hub.respond(conn_id, Some(id), Err(error)),
                }
                return;
            }
            if let MethodCall::ScreenCapture { opts, .. } | MethodCall::WindowsScreenshot { opts, .. } = &call {
                if let Some(path) = opts.to_path.as_deref() {
                    if let Err(error) = state.validate_capture_path(path) {
                        state.hub.respond(conn_id, Some(id), Err(error));
                        return;
                    }
                }
            }
            // windows.launch 需要等待窗口出现：走延迟响应路径（绝不阻塞主循环）
            if let MethodCall::WindowsLaunch {
                argv,
                cwd,
                env,
                x11,
                wait_for_window_ms,
            } = call
            {
                match launch_deferred(
                    state,
                    conn_id,
                    id.clone(),
                    argv,
                    cwd,
                    env,
                    x11,
                    wait_for_window_ms,
                ) {
                    LaunchOutcome::Immediate(v) => state.hub.respond(conn_id, Some(id), Ok(v)),
                    LaunchOutcome::Deferred => {}
                    LaunchOutcome::Error(e) => state.hub.respond(conn_id, Some(id), Err(e)),
                }
                return;
            }
            match dispatch_call(state, conn_id, call) {
                Ok(v) => state.hub.respond(conn_id, Some(id), Ok(v)),
                Err(e) => state.hub.respond(conn_id, Some(id), Err(e)),
            }
        }
        IpcRequest::Gone { conn_id } => {
            tracing::debug!(conn_id, "控制客户端断开");
            // 断开的连接不再补发 launch 响应
            state.pending_launches.retain(|p| p.conn_id != conn_id);
            state.frame_subscriptions.remove(&conn_id);
        }
    }
}

/// Request client-owned selection data over a socketpair; only the reader blocks,
/// while the compositor continues dispatching Wayland clients on its main loop.
fn clipboard_get(
    state: &mut AgentDesk,
    conn_id: u64,
    request_id: ad_ipc::rpc::Id,
    which: &str,
) -> Result<Option<Value>, RpcError> {
    use smithay::wayland::selection::data_device::request_data_device_client_selection;
    use smithay::wayland::selection::primary_selection::request_primary_client_selection;
    use std::os::unix::net::UnixStream;

    let primary = match which {
        "regular" => false,
        "primary" => true,
        other => {
            return Err(RpcError::invalid_params(format!(
                "未知剪贴板类型 {other}（可选 regular|primary）"
            )))
        }
    };
    let (mut reader, writer) =
        UnixStream::pair().map_err(|e| RpcError::internal(format!("剪贴板管道创建失败: {e}")))?;
    reader
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .map_err(|e| RpcError::internal(format!("剪贴板读取超时设置失败: {e}")))?;
    let fd: std::os::fd::OwnedFd = writer.into();
    let requested: Result<(), String> = if primary {
        request_primary_client_selection(&state.seat, "text/plain;charset=utf-8".into(), fd)
            .map_err(|e| format!("{e:?}"))
    } else {
        request_data_device_client_selection(&state.seat, "text/plain;charset=utf-8".into(), fd)
            .map_err(|e| format!("{e:?}"))
    };
    match requested {
        Ok(()) => {
            let sender = state.clipboard_results.clone();
            std::thread::Builder::new()
                .name("agentdesk-clipboard-read".into())
                .spawn(move || {
                    use std::io::Read;
                    let mut text = String::new();
                    let result = reader
                        .read_to_string(&mut text)
                        .map(|_| text)
                        .map_err(|e| format!("剪贴板读取失败: {e}"));
                    let _ = sender.send(crate::state::ClipboardResult {
                        conn_id,
                        request_id,
                        result,
                    });
                })
                .map_err(|e| RpcError::internal(format!("剪贴板读取线程创建失败: {e}")))?;
            Ok(None)
        }
        Err(error) if error.contains("ServerSideSelection") => {
            let text = if primary {
                state.primary_text.as_deref()
            } else {
                state.clipboard_text.as_deref()
            };
            Ok(Some(json!({"text": text})))
        }
        Err(error) if error.contains("NoSelection") => Ok(Some(json!({"text": null}))),
        Err(error) => Err(RpcError::invalid_params(format!(
            "剪贴板不支持 UTF-8 文本: {error}"
        ))),
    }
}

/// 方法分发结果：立即返回的值，或延迟补发的 launch。
pub enum DispatchOutcome {
    Value(Value),
    Deferred,
}

enum LaunchOutcome {
    Immediate(Value),
    Deferred,
    Error(RpcError),
}

#[allow(clippy::too_many_arguments)]
fn launch_deferred(
    state: &mut AgentDesk,
    conn_id: u64,
    request_id: ad_ipc::rpc::Id,
    argv: Vec<String>,
    cwd: Option<String>,
    env: Vec<(String, String)>,
    x11: bool,
    wait_for_window_ms: Option<u64>,
) -> LaunchOutcome {
    match state.launch_process(&argv, cwd.as_deref(), &env, x11) {
        Err(e) => LaunchOutcome::Error(e),
        Ok(pid) => {
            // 快速路径：窗口已存在
            if let Some(wid) = state.windows.values().find(|r| r.pid == pid).map(|r| r.id) {
                return LaunchOutcome::Immediate(json!({ "pid": pid, "window_id": wid }));
            }
            match wait_for_window_ms {
                None => LaunchOutcome::Immediate(json!({ "pid": pid, "window_id": null })),
                Some(ms) => {
                    state.pending_launches.push(crate::state::PendingLaunch {
                        pid,
                        conn_id,
                        request_id,
                        deadline: std::time::Instant::now()
                            + std::time::Duration::from_millis(ms.min(5000)),
                    });
                    LaunchOutcome::Deferred
                }
            }
        }
    }
}

/// 查看器线程事件：窗口被父 WM 关闭 / 初始化失败 → 回到隐藏态。
pub fn handle_viewer_event(state: &mut AgentDesk, ev: crate::viewer::ViewerEvent) {
    use crate::viewer::ViewerEvent;
    match ev {
        ViewerEvent::Closed => tracing::info!("嵌套查看器窗口被关闭"),
        ViewerEvent::Failed(e) => tracing::warn!("嵌套查看器失败: {e}"),
    }
    state.viewer = None;
    if state.viewer_visible {
        state.viewer_visible = false;
        state.viewer_size = None;
        state
            .hub
            .broadcast(&ad_ipc::protocol::Event::DisplayChanged { visible: false });
    }
}

fn dispatch_call(state: &mut AgentDesk, conn_id: u64, call: MethodCall) -> Result<Value, RpcError> {
    match call {
        MethodCall::ServerInfo => {
            serde_json::to_value(state.server_info()).map_err(|e| RpcError::internal(e.to_string()))
        }
        MethodCall::ServerPing => Ok(json!({ "pong": true, "ts_ms": now_ms() })),
        MethodCall::ServerShutdown => {
            state.shutting_down = true;
            Ok(json!({ "ok": true }))
        }
        MethodCall::Auth { token } => {
            let ok = state.token.is_none() || state.token.as_deref() == Some(token.as_str());
            state.hub.set_authenticated(conn_id, ok);
            if ok {
                Ok(json!({ "ok": true }))
            } else {
                Err(RpcError::new(codes::PERMISSION_DENIED, "令牌不匹配"))
            }
        }

        MethodCall::WindowsList { filter } => {
            Ok(json!({ "windows": state.list_windows(filter.as_ref()) }))
        }
        MethodCall::WindowsGet { id } => serde_json::to_value(
            state
                .window_json(id)
                .ok_or_else(|| RpcError::window_not_found(id))?,
        )
        .map_err(|e| RpcError::internal(e.to_string())),
        MethodCall::WindowsClose { id } => state.close_window(id).map(|_| json!({ "ok": true })),
        MethodCall::WindowsFocus { id } => state.focus_window(id).map(|_| json!({ "ok": true })),
        MethodCall::WindowsRaise { id } => state.raise_window(id).map(|_| json!({ "ok": true })),
        MethodCall::WindowsSetGeometry {
            id,
            x,
            y,
            width,
            height,
        } => state
            .set_geometry(id, x, y, width, height)
            .map(|_| json!({ "ok": true })),
        MethodCall::WindowsMinimize { id } => {
            state.minimize_window(id).map(|_| json!({ "ok": true }))
        }
        MethodCall::WindowsRestore { id } => {
            state.restore_window(id).map(|_| json!({ "ok": true }))
        }
        MethodCall::WindowsSetFullscreen { id, enabled } => {
            if !state.windows.contains_key(&id) {
                return Err(RpcError::window_not_found(id));
            }
            let toplevel = state
                .windows
                .get(&id)
                .and_then(|r| r.element.toplevel())
                .ok_or_else(|| RpcError::window_not_found(id))?;
            toplevel.with_pending_state(|pending| {
                if enabled {
                    pending.states.set(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen);
                    pending.size = Some(state.primary_output_size().into());
                } else {
                    pending.states.unset(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen);
                }
            });
            toplevel.send_configure();
            Ok(json!({ "ok": true }))
        }

        MethodCall::EventsSubscribe { types } => {
            const MAX_PATTERNS: usize = 16;
            const ALLOWED: &[&str] = &[
                "*", "window.*", "window.created", "window.updated", "window.closed",
                "focus.changed", "display.changed", "output.*", "output.added", "output.removed",
                "frames.frame",
            ];
            if types.len() > MAX_PATTERNS {
                return Err(RpcError::invalid_params(format!("events.subscribe 最多允许 {MAX_PATTERNS} 个类型")));
            }
            if let Some(unknown) = types.iter().find(|kind| !ALLOWED.contains(&kind.as_str())) {
                return Err(RpcError::invalid_params(format!("不允许订阅事件类型 {unknown}")));
            }
            state.hub.set_subscriptions(conn_id, types);
            Ok(json!({ "subscription_id": conn_id }))
        }
        MethodCall::EventsUnsubscribe { .. } => {
            state.hub.set_subscriptions(conn_id, Vec::new());
            Ok(json!({ "ok": true }))
        }

        MethodCall::X11Status => {
            serde_json::to_value(state.x11_status()).map_err(|e| RpcError::internal(e.to_string()))
        }
        MethodCall::DisplayStatus => Ok(json!({
            "visible": state.viewer_visible,
            "output": state.primary_output().map(|o| o.name.clone()),
            "window_size": state.viewer_size.map(|(w, h)| json!({"width": w, "height": h})),
            "parent_session": state.parent_session,
        })),
        MethodCall::DisplayShow {
            output: _,
            width,
            height,
            scale: _,
            title,
        } => {
            if state.parent_session != Some("wayland") {
                return Err(RpcError::new(
                    codes::NO_PARENT_SESSION,
                    "无父 Wayland 会话，嵌套查看器不可用（无头环境请使用截图 API）",
                ));
            }
            if state.viewer.is_none() {
                let (dw, dh) = state.primary_output_size();
                let w = width.map(|v| v as i32).unwrap_or(dw);
                let h = height.map(|v| v as i32).unwrap_or(dh);
                let title =
                    title.unwrap_or_else(|| format!("agentdesk ({})", state.wayland_socket_name));
                let handle = crate::viewer::spawn(crate::viewer::ViewerParams {
                    title,
                    width: w,
                    height: h,
                    events: state.viewer_events.clone(),
                });
                state.viewer = Some(handle);
                state.viewer_visible = true;
                state.viewer_size = Some((w, h));
                state
                    .hub
                    .broadcast(&ad_ipc::protocol::Event::DisplayChanged { visible: true });
                tracing::info!(width = w, height = h, "嵌套查看器已显示");
                Ok(json!({ "visible": true, "window_size": { "width": w, "height": h } }))
            } else {
                let (w, h) = state.viewer_size.unwrap_or((0, 0));
                Ok(json!({ "visible": true, "window_size": { "width": w, "height": h } }))
            }
        }
        MethodCall::DisplayHide => {
            if let Some(v) = state.viewer.take() {
                v.stop();
            }
            if state.viewer_visible {
                state.viewer_visible = false;
                state.viewer_size = None;
                state
                    .hub
                    .broadcast(&ad_ipc::protocol::Event::DisplayChanged { visible: false });
            }
            Ok(json!({ "visible": false }))
        }

        MethodCall::WindowsLaunch {
            argv,
            cwd,
            env,
            x11,
            wait_for_window_ms,
        } => {
            let pid = state.launch_process(&argv, cwd.as_deref(), &env, x11)?;
            // 等待该 pid 的窗口出现（阻塞轮询，v1 简化；上限 5s）
            let mut window_id = None;
            if let Some(ms) = wait_for_window_ms {
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(ms.min(5000));
                while window_id.is_none() && std::time::Instant::now() < deadline {
                    window_id = state.windows.values().find(|r| r.pid == pid).map(|r| r.id);
                    if window_id.is_none() {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                }
            }
            Ok(json!({ "pid": pid, "window_id": window_id }))
        }

        MethodCall::LayoutTile {
            output,
            direction,
            padding,
        } => {
            if let Some(d) = direction.as_deref() {
                if d != "grid" {
                    return Err(RpcError::invalid_params(format!(
                        "未知方向 {d}（v1 仅支持 grid）"
                    )));
                }
            }
            let name = output.unwrap_or_else(|| primary_name(state));
            let area = state
                .outputs
                .iter()
                .find(|o| o.name == name)
                .map(|o| o.size)
                .ok_or_else(|| {
                    RpcError::new(codes::OUTPUT_NOT_FOUND, format!("输出 {name} 不存在"))
                })?;
            let ids = windows_on_output(state, &name);
            let rects = ad_wm::layout::tile_grid(ids.len(), area, padding.unwrap_or(8));
            for (id, (x, y, w, h)) in ids.iter().zip(rects) {
                let _ = state.place_window(*id, x, y, Some(w), Some(h));
            }
            Ok(json!({ "arranged": ids.len() }))
        }
        MethodCall::LayoutCascade { output, offset } => {
            let name = output.unwrap_or_else(|| primary_name(state));
            let ids = windows_on_output(state, &name);
            let off = offset.unwrap_or(32);
            for (i, id) in ids.iter().enumerate() {
                let _ = state.place_window(
                    *id,
                    32 + (i as i32) * off,
                    32 + (i as i32) * off,
                    None,
                    None,
                );
            }
            Ok(json!({ "arranged": ids.len() }))
        }

        MethodCall::OutputsList => Ok(json!({ "outputs": state.outputs_json() })),
        MethodCall::OutputsCreate {
            width,
            height,
            name,
        } => {
            let id = state.output_create(width, height, name)?;
            Ok(json!({ "id": id }))
        }
        MethodCall::OutputsDestroy { id } => {
            state.output_destroy(&id)?;
            Ok(json!({ "ok": true }))
        }
        MethodCall::InputGetCursor => {
            Ok(json!({ "x": state.cursor_pos.x, "y": state.cursor_pos.y }))
        }

        MethodCall::ClipboardSet { text } => {
            let mime_types = vec![
                "text/plain;charset=utf-8".to_string(),
                "text/plain".to_string(),
            ];
            set_data_device_selection(&state.dh, &state.seat, mime_types.clone(), text.clone());
            set_primary_selection(&state.dh, &state.seat, mime_types, text.clone());
            state.clipboard_text = Some(text.clone());
            state.primary_text = Some(text);
            Ok(json!({ "ok": true }))
        }
        MethodCall::FramesSubscribe { output, fps } => {
            let output = output.unwrap_or_else(|| primary_name(state));
            if !state.outputs.iter().any(|item| item.name == output) {
                return Err(RpcError::new(codes::OUTPUT_NOT_FOUND, format!("输出 {output} 不存在")));
            }
            let fps = fps.unwrap_or(5);
            if !(1..=30).contains(&fps) { return Err(RpcError::invalid_params("frames.subscribe 的 fps 必须在 1..=30")); }
            state.frame_subscriptions.insert(conn_id, crate::state::FrameSubscription {
                output, fps, last_frame: std::time::Instant::now() - std::time::Duration::from_secs(2),
            });
            Ok(json!({"subscription_id":conn_id,"fps":fps,"format":"jpeg"}))
        }
        MethodCall::FramesUnsubscribe { subscription_id } => {
            if subscription_id != conn_id { return Err(RpcError::invalid_params("subscription_id 不属于当前连接")); }
            state.frame_subscriptions.remove(&conn_id);
            Ok(json!({"ok":true}))
        }
        MethodCall::ClipboardGet { which } => {
            let which = which.as_deref().unwrap_or("regular");
            let text = match which {
                "regular" => state.clipboard_text.as_deref(),
                "primary" => state.primary_text.as_deref(),
                other => {
                    return Err(RpcError::invalid_params(format!(
                        "未知剪贴板类型 {other}（可选 regular|primary）"
                    )))
                }
            };
            Ok(json!({ "text": text }))
        }

        MethodCall::ScreenCapture { output, opts } => {
            let raw = state.screen_capture(output.as_deref())?;
            let result = state.encode_capture(raw, &opts)?;
            serde_json::to_value(result).map_err(|e| RpcError::internal(e.to_string()))
        }
        MethodCall::WindowsScreenshot { id, opts } => {
            let raw = state.window_capture(id)?;
            let result = state.encode_capture(raw, &opts)?;
            serde_json::to_value(result).map_err(|e| RpcError::internal(e.to_string()))
        }

        MethodCall::InputMoveCursor { x, y } => {
            state.input_move_cursor(x, y);
            Ok(json!({ "ok": true }))
        }
        MethodCall::InputMoveRelative { dx, dy } => {
            state.input_move_relative(dx, dy);
            Ok(json!({ "ok": true }))
        }
        MethodCall::InputButton { button, action } => {
            let b = crate::input::parse_button(Some(&button))?;
            match action.as_str() {
                "press" => state.input_button(b, true),
                "release" => state.input_button(b, false),
                "click" => crate::input::click_at(state, None, None, b, 1),
                "double_click" => crate::input::click_at(state, None, None, b, 2),
                other => {
                    return Err(RpcError::invalid_params(format!(
                        "未知 action {other}（可选 press|release|click|double_click）"
                    )))
                }
            }
            Ok(json!({ "ok": true }))
        }
        MethodCall::InputClick {
            x,
            y,
            button,
            count,
        } => {
            let b = crate::input::parse_button(button.as_deref())?;
            crate::input::click_at(state, x, y, b, count.unwrap_or(1));
            Ok(json!({ "ok": true }))
        }
        MethodCall::InputScroll { dx, dy, x, y } => {
            if let (Some(x), Some(y)) = (x, y) {
                state.input_move_cursor(x, y);
            }
            state.input_scroll(dx.unwrap_or(0.0), dy.unwrap_or(0.0));
            Ok(json!({ "ok": true }))
        }
        MethodCall::InputKey { key, action } => {
            let act = match action.as_deref() {
                Some("press") => crate::input::KeyAction::Press,
                Some("release") => crate::input::KeyAction::Release,
                None | Some("tap") => crate::input::KeyAction::Tap,
                Some(other) => {
                    return Err(RpcError::invalid_params(format!(
                        "未知 action {other}（可选 press|release|tap）"
                    )))
                }
            };
            state.input_key(&key, act)?;
            Ok(json!({ "ok": true }))
        }
        MethodCall::InputCombo { keys, hold_ms } => {
            state.input_combo(&keys, hold_ms.unwrap_or(0))?;
            Ok(json!({ "ok": true }))
        }
        MethodCall::InputTypeText {
            text,
            delay_ms_per_key,
        } => {
            state.input_type_text(&text, delay_ms_per_key.unwrap_or(0))?;
            Ok(json!({ "ok": true }))
        }
    }
}

fn windows_on_output(state: &AgentDesk, name: &str) -> Vec<u64> {
    state
        .window_order
        .iter()
        .cloned()
        .filter(|id| {
            state
                .windows
                .get(id)
                .map(|r| r.mapped && r.output == name)
                .unwrap_or(false)
        })
        .collect()
}

fn primary_name(state: &AgentDesk) -> String {
    state
        .outputs
        .first()
        .map(|o| o.name.clone())
        .unwrap_or_else(|| "HEADLESS-1".into())
}

fn method_name(c: &MethodCall) -> &'static str {
    match c {
        MethodCall::ServerInfo => "server.info",
        MethodCall::ServerPing => "server.ping",
        MethodCall::ServerShutdown => "server.shutdown",
        MethodCall::Auth { .. } => "auth",
        MethodCall::WindowsList { .. } => "windows.list",
        MethodCall::WindowsGet { .. } => "windows.get",
        MethodCall::WindowsClose { .. } => "windows.close",
        MethodCall::WindowsFocus { .. } => "windows.focus",
        MethodCall::WindowsRaise { .. } => "windows.raise",
        MethodCall::WindowsSetGeometry { .. } => "windows.set_geometry",
        MethodCall::WindowsMinimize { .. } => "windows.minimize",
        MethodCall::WindowsRestore { .. } => "windows.restore",
        MethodCall::WindowsSetFullscreen { .. } => "windows.set_fullscreen",
        MethodCall::WindowsLaunch { .. } => "windows.launch",
        MethodCall::ScreenCapture { .. } => "screen.capture",
        MethodCall::WindowsScreenshot { .. } => "windows.screenshot",
        MethodCall::FramesSubscribe { .. } => "frames.subscribe",
        MethodCall::FramesUnsubscribe { .. } => "frames.unsubscribe",
        MethodCall::InputMoveCursor { .. } => "input.move_cursor",
        MethodCall::InputMoveRelative { .. } => "input.move_relative",
        MethodCall::InputButton { .. } => "input.button",
        MethodCall::InputClick { .. } => "input.click",
        MethodCall::InputScroll { .. } => "input.scroll",
        MethodCall::InputKey { .. } => "input.key",
        MethodCall::InputCombo { .. } => "input.combo",
        MethodCall::InputTypeText { .. } => "input.type_text",
        MethodCall::InputGetCursor => "input.get_cursor",
        MethodCall::DisplayShow { .. } => "display.show",
        MethodCall::DisplayHide => "display.hide",
        MethodCall::DisplayStatus => "display.status",
        MethodCall::OutputsList => "outputs.list",
        MethodCall::OutputsCreate { .. } => "outputs.create",
        MethodCall::OutputsDestroy { .. } => "outputs.destroy",
        MethodCall::LayoutTile { .. } => "layout.tile",
        MethodCall::LayoutCascade { .. } => "layout.cascade",
        MethodCall::ClipboardGet { .. } => "clipboard.get",
        MethodCall::ClipboardSet { .. } => "clipboard.set",
        MethodCall::EventsSubscribe { .. } => "events.subscribe",
        MethodCall::EventsUnsubscribe { .. } => "events.unsubscribe",
        MethodCall::X11Status => "x11.status",
    }
}
