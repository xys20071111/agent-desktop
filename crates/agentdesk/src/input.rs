//! 输入注入：合成键鼠事件进入 seat 状态机（与真实设备同一条路径）。
//!
//! keycode 约定：注入 API 使用 evdev 码，+8 后喂给 smithay（xkb keycode），
//! 客户端最终收到 evdev 码（`WlSurface::key` 中 `raw_code() - 8`）。

use std::time::Duration;

use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState};
use smithay::desktop::utils::under_from_surface_tree;
use smithay::desktop::{Window, WindowSurfaceType};
use smithay::input::keyboard::{FilterResult, KeyboardHandle};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, RelativeMotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use smithay::wayland::seat::WaylandFocus;

use ad_ipc::error::{codes, RpcError};
use ad_wm::keymap::{resolve_button, resolve_char, resolve_key_name, ResolvedKey};

use crate::state::AgentDesk;

impl AgentDesk {
    fn now_ms_u32(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    /// 全局坐标处的指针焦点：(命中的 surface, 该 surface 原点的全局坐标)。
    fn pointer_focus_at(
        &self,
        pt: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let (window, loc) = self.space.element_under(pt)?;
        let wl = window.wl_surface()?;
        under_from_surface_tree(&wl, pt, loc, WindowSurfaceType::ALL)
            .map(|(s, l)| (s, l.to_f64()))
    }

    fn window_id_of(&self, window: &Window) -> Option<u64> {
        let ws = window.wl_surface()?;
        self.windows
            .iter()
            .find(|(_, r)| r.element.wl_surface().map(|s| s == ws).unwrap_or(false))
            .map(|(k, _)| *k)
    }

    fn keyboard(&self) -> Result<KeyboardHandle<Self>, RpcError> {
        self.seat
            .get_keyboard()
            .ok_or_else(|| RpcError::internal("合成键盘未初始化"))
    }

    fn key_event(&mut self, kb: &KeyboardHandle<Self>, evdev: u32, press: bool) {
        let time = self.now_ms_u32();
        kb.input(
            self,
            (evdev + 8).into(),
            if press { KeyState::Pressed } else { KeyState::Released },
            SERIAL_COUNTER.next_serial(),
            time,
            |_, _, _| FilterResult::<()>::Forward,
        );
    }

    // ---------------- 指针 ----------------

    pub fn input_move_cursor(&mut self, x: f64, y: f64) {
        self.pointer_motion_to(Point::from((x, y)), None);
    }

    pub fn input_move_relative(&mut self, dx: f64, dy: f64) {
        let cur = self.cursor_pos;
        self.pointer_motion_to(Point::from((cur.x + dx, cur.y + dy)), Some((dx, dy)));
    }

    fn pointer_motion_to(&mut self, pt: Point<f64, Logical>, rel: Option<(f64, f64)>) {
        self.cursor_pos = pt;
        let focus = self.pointer_focus_at(pt);
        let Some(pointer) = self.seat.get_pointer() else { return };
        let time = self.now_ms_u32();
        pointer.motion(
            self,
            focus.clone(),
            &MotionEvent {
                location: pt,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        if let Some((dx, dy)) = rel {
            pointer.relative_motion(
                self,
                focus,
                &RelativeMotionEvent {
                    delta: Point::from((dx, dy)),
                    delta_unaccel: Point::from((dx, dy)),
                    utime: u64::from(time) * 1000,
                },
            );
        }
        pointer.frame(self);
    }

    pub fn input_button(&mut self, button_evdev: u32, press: bool) {
        let Some(pointer) = self.seat.get_pointer() else { return };
        pointer.button(
            self,
            &ButtonEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time: self.now_ms_u32(),
                button: button_evdev,
                state: if press { ButtonState::Pressed } else { ButtonState::Released },
            },
        );
        pointer.frame(self);
        if press {
            // 点击获得键盘焦点（标准合成器行为）
            if let Some((window, _)) = self.space.element_under(self.cursor_pos) {
                if let Some(id) = self.window_id_of(window) {
                    let _ = self.focus_window(id);
                }
            }
        }
    }

    pub fn input_scroll(&mut self, dx: f64, dy: f64) {
        let Some(pointer) = self.seat.get_pointer() else { return };
        let time = self.now_ms_u32();
        let mut frame = AxisFrame::new(time).source(AxisSource::Wheel);
        if dy != 0.0 {
            frame = frame.value(Axis::Vertical, dy * 10.0);
        }
        if dx != 0.0 {
            frame = frame.value(Axis::Horizontal, dx * 10.0);
        }
        if dx != 0.0 || dy != 0.0 {
            frame.v120 = Some(((dx * 120.0) as i32, (dy * 120.0) as i32));
        }
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    // ---------------- 键盘 ----------------

    pub fn input_key(&mut self, name: &str, action: KeyAction) -> Result<(), RpcError> {
        let key = resolve_key_name(name)
            .ok_or_else(|| RpcError::invalid_params(format!("未知键名 {name}")))?;
        let kb = self.keyboard()?;
        match action {
            KeyAction::Press => {
                for m in &key.mods {
                    self.key_event(&kb, m.evdev(), true);
                }
                self.key_event(&kb, key.evdev, true);
            }
            KeyAction::Release => {
                self.key_event(&kb, key.evdev, false);
                for m in key.mods.iter().rev() {
                    self.key_event(&kb, m.evdev(), false);
                }
            }
            KeyAction::Tap => {
                for m in &key.mods {
                    self.key_event(&kb, m.evdev(), true);
                }
                self.key_event(&kb, key.evdev, true);
                self.key_event(&kb, key.evdev, false);
                for m in key.mods.iter().rev() {
                    self.key_event(&kb, m.evdev(), false);
                }
            }
        }
        Ok(())
    }

    pub fn input_combo(&mut self, keys: &[String], hold_ms: u64) -> Result<(), RpcError> {
        let resolved: Vec<ResolvedKey> = keys
            .iter()
            .map(|k| {
                resolve_key_name(k)
                    .ok_or_else(|| RpcError::invalid_params(format!("未知键名 {k}")))
            })
            .collect::<Result<_, _>>()?;
        let kb = self.keyboard()?;
        for k in &resolved {
            self.key_event(&kb, k.evdev, true);
        }
        if hold_ms > 0 {
            std::thread::sleep(Duration::from_millis(hold_ms));
        }
        for k in resolved.iter().rev() {
            self.key_event(&kb, k.evdev, false);
        }
        Ok(())
    }

    pub fn input_type_text(&mut self, text: &str, delay_ms_per_key: u64) -> Result<(), RpcError> {
        let kb = self.keyboard()?;
        for c in text.chars() {
            let key: ResolvedKey = resolve_char(c).ok_or_else(|| {
                RpcError::new(
                    codes::UNSUPPORTED_CHAR,
                    format!("字符 {c:?} 无法在 us 布局下注入"),
                )
            })?;
            for m in &key.mods {
                self.key_event(&kb, m.evdev(), true);
            }
            self.key_event(&kb, key.evdev, true);
            self.key_event(&kb, key.evdev, false);
            for m in key.mods.iter().rev() {
                self.key_event(&kb, m.evdev(), false);
            }
            if delay_ms_per_key > 0 {
                std::thread::sleep(Duration::from_millis(delay_ms_per_key));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Press,
    Release,
    Tap,
}

/// 点击注入的便捷封装：可选先移动光标。
pub fn click_at(
    state: &mut AgentDesk,
    x: Option<f64>,
    y: Option<f64>,
    button_evdev: u32,
    count: u32,
) {
    if let (Some(x), Some(y)) = (x, y) {
        state.input_move_cursor(x, y);
    }
    for _ in 0..count {
        state.input_button(button_evdev, true);
        state.input_button(button_evdev, false);
    }
}

/// 解析按钮参数（"left"/数字）。
pub fn parse_button(name: Option<&str>) -> Result<u32, RpcError> {
    resolve_button(name.unwrap_or("left"))
        .ok_or_else(|| RpcError::invalid_params("未知按钮名（可选 left|right|middle|back|forward|数字）"))
}
