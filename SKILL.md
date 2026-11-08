---
name: agent-desktop
description: 驱动 agent-desktop 无头 Wayland 合成器来操控 Linux 图形应用——拉起/管理窗口（JSON 清单）、截图观察（PNG/像素级）、键鼠注入、布局与多输出控制、嵌套实时预览。当任务需要在无头服务器或嵌套环境下自动化 GUI（驱动浏览器/编辑器、GUI 冒烟测试、视觉验证）时使用。
---

# agent-desktop：Agent 驱动 Linux GUI 的合成器

你（Agent）通过一个 Unix socket 上的 JSON-RPC 接口控制一台**无头 Wayland 合成器**。
所有 GUI 应用都运行在这台合成器里；你看不到它，但可以通过**截图**观察、通过**注入**操作，
与真实桌面行为完全一致（走 Wayland 标准协议）。人类同事想看时，`show` 一个嵌套预览窗口即可。

完整协议见仓库 `docs/CONTROL_API.md`；本文只讲 Agent 视角的实操。

## 0. 前提

```bash
cargo build --release          # 需要 libxkbcommon-dev、libpixman-1-dev（无需 libwayland/DRM/GPU）
BIN=target/release
```

## 1. 启动与连接

```bash
$BIN/agentdesk --instance main --print-info &      # 无头启动；stdout 打一行 JSON（含 socket 路径）
```

- **socket 发现**：`$AGENTDESK_SOCKET` → 否则 `$XDG_RUNTIME_DIR/agentdesk/<instance>.sock`
- 权限 0600，仅同用户可用；多实例用 `--instance` 隔离（socket 与 Wayland socket 名均独立）
- 关闭：`$BIN/adctl shutdown`（优雅退出，socket 文件自动清理）

`adctl` 是官方 CLI；`--socket`/`--json` 是全局参数（放子命令前）。

## 2. 核心循环（记住这个模式）

```
launch（拿 window_id） → list/get（看状态） → screenshot（看画面）
  → click/type/key（操作） → 等待（事件或轮询） → screenshot（验证结果）
```

```bash
export AGENTDESK_SOCKET=/run/user/$(id -u)/agentdesk/main.sock
A="$BIN/adctl"

# 1) 拉起应用：--wait 等窗口出现（上限 5000ms），响应返回 window_id —— 后续都用它
$A launch --wait 5000 -- firefox
$A launch --x11 --wait 5000 -- xterm        # X11 应用：自动拉起 xwayland-satellite :100

# 2) 看看有哪些窗口
$A windows list                             # 人读格式；--json 给原始 JSON
$A windows list --json --app-id org.mozilla.firefox

# 3) 看某个窗口长什么样（给你自己"看"，PNG 直接喂视觉模型）
$A screenshot 3 -o /tmp/win.png
$A screenshot screen -o /tmp/desktop.png    # 整屏

# 4) 操作
$A click 640 360                            # 左键单击（全局坐标）
$A click 640 360 --button left --count 2    # 双击
$A move 100 100 && $A move-rel 0 120        # 移动/相对移动
$A scroll -3                                # 向上滚 3 格（正=向下）
$A key Return && $A combo ctrl shift t      # 单键 / 组合键
$A type "hello world"                       # 文本注入（限 US 布局可打印字符）

# 5) 布局与窗口管理
$A tile                                     # 网格排列当前输出的所有窗口
$A windows geometry 3 --x 0 --y 0 --width 800 --height 600
$A windows focus 3                          # 置顶 + 键盘焦点
$A windows close 3

# 6) 人类想围观时（仅在嵌套 Wayland 会话下可用）
$A show --size 1280x800                     # 弹出实时预览窗口（30fps 镜像）
$A hide
```

## 3. 关键守则（违反会翻车）

1. **一切坐标来自 `windows.list`**。坐标系是全局布局坐标（与 `geometry` 字段同系，原点=主输出左上角，scale=1）。
   不要凭记忆点"上次的按钮位置"——先 `windows get` 拿最新几何。
2. **window_id 稳定且永不复用**。`launch` 返回的 `window_id` 是你的句柄；`-32001 window_not_found` 意味着它已关闭。
3. **show/hide 不动窗口**。嵌套预览只是镜像，开关它不影响任何坐标/状态——放心随时 show 给人类看。
4. **`type_text` 只支持 US 布局可打印字符**（ASCII/常用符号）。中文等返回 `-32006 unsupported_char`——不要重试，改走应用内输入法或其他路径。
5. **等待要轮询，不要盲睡**。窗口出现/状态变化用 `windows.get` 轮询（50–100ms 间隔）或 `watch` 事件流；
   合成器帧节拍是 30Hz，应用首帧绘制需要 1–2 帧时间。launch 后立刻截图可能拍到空白/旧帧。
6. **截图语义**：`windows.screenshot` 只含目标窗口自身（含子表面，保留 alpha），**不受遮挡影响**；
   `screen.capture` 是"整屏所见"。像素断言用 `format:"raw"`（BGRA8888，B 在第 0 字节）；给人看用默认 png。
7. **错误码**：`-32001` 窗口不存在、`-32002` 输出不存在、`-32004` 无父会话（show）、`-32005` 启动失败、
   `-32006` 字符不支持、`-32007` 未鉴权。收到后先读 `message`，不要盲目重试。
8. **多 Agent 并发**：允许多个连接；输入请求全局 FIFO 串行。协调好谁在操作，避免互相点击。

## 4. 等待策略（怎么"等页面加载完"）

合成器会推送事件。两种模式：

**轮询式（简单可靠）**：

```bash
until $A windows get $ID --json | grep -q '"width": *800'; do sleep 0.1; done
```

**事件式（`watch`）**：

```bash
$A watch window.* focus.changed        # 打印事件流（JSON 行）；Ctrl-C 退出
# 编程用法：连接后发 events.subscribe {"types":["window.*"]}，随后读 notification 行
```

事件类型：`window.created/updated/closed`、`focus.changed`、`display.changed`、`output.added/removed`。
支持通配：`*`、`window.*`。注意 `window.updated` 的 geometry 变化按 ~16ms 节流合并。

## 5. 原始 JSON-RPC（不用 adctl 时）

NDJSON 帧格式的 JSON-RPC 2.0：每行一条消息；连接后服务器先推 `event/server.hello`；
图片默认 base64 内联（`encoding:"base64"`）。Python 示例：

```python
import json, socket

class Desk:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(path)
        self.f = self.s.makefile("rwb")
        self.n = 0
        self.hello = self._read_event()          # event/server.hello

    def call(self, method, params=None):
        self.n += 1
        msg = {"jsonrpc": "2.0", "id": self.n, "method": method}
        if params is not None:
            msg["params"] = params
        self.f.write((json.dumps(msg) + "\n").encode()); self.f.flush()
        while True:
            obj = json.loads(self.f.readline())
            if obj.get("id") == self.n:
                return obj["result"] if "result" in obj else (_ for _ in ()).throw(RuntimeError(obj["error"]))

desk = Desk("/run/user/1000/agentdesk/main.sock")
win = desk.call("windows.launch", {"argv": ["firefox"], "wait_for_window_ms": 5000})
desk.call("windows.set_geometry", {"id": win["window_id"], "x": 0, "y": 0, "width": 1280, "height": 800})
shot = desk.call("windows.screenshot", {"id": win["window_id"], "format": "raw"})
# shot["data"] = base64；BGRA8888，B 在第 0 字节
```

手工调试：`socat - UNIX-CONNECT:$AGENTDESK_SOCKET`，然后逐行粘贴 JSON。

## 6. 多输出与 X11

- `outputs.create {"width":1920,"height":1080}` → 新虚拟输出排在右侧（坐标继续向右延伸），
  供多个 Agent 并行分区工作；`outputs.destroy` 会把其上窗口迁回主输出。
- X11 应用经 [xwayland-satellite](https://github.com/Supreeeme/xwayland-satellite) 外置接入：
  `launch --x11` 会确保 satellite 在跑（默认 display `:100`，`--x11-display N` 可改），
  X11 窗口在清单里 `origin:"x11"`，其余操作与 Wayland 窗口完全一致。
  已知坑：Java 应用需 `_JAVA_AWT_WM_NONREPARENTING=1`。
- 纯服务器（无任何图形会话）下一切照常，仅 `show` 返回 `-32004`（没有父会话可弹窗）。

## 7. 故障排查

| 症状 | 原因/处理 |
|---|---|
| 连接 socket 失败 | 实例没起/名字不对：`ls $XDG_RUNTIME_DIR/agentdesk/`；`agentdesk --print-info` 确认路径 |
| launch 返回 `window_id: null` | 应用没在 5s 内出窗（冷启动慢）。稍后 `windows list` 找 pid 对应窗口 |
| 截图全黑/旧画面 | 应用还没画首帧：轮询 `windows get` 直到 `surface_size` 非零再截 |
| 点击没反应 | 坐标不在任何窗口上（指针 enter 不发生）。先 `move` 到窗口几何内再点击 |
| `type_text` 报 -32006 | 非 US 布局字符（中文等），v1 不支持 |
| show 报 -32004 | 当前无父 Wayland 会话（纯服务器）；预览不可用，截图 API 不受影响 |
| X11 窗口空白 | `x11.status` 确认 satellite 活着；Java 见上；satellite 版本需 ≥0.8 |
