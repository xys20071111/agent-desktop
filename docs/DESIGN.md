# agent-desktop 设计文档

> 面向 AI Agent 的无头 Wayland 合成器（headless-first、nested-capable），基于 Smithay，纯 Rust。
>
> 版本：v1 草案（2026-11-07）｜状态：设计定稿，待实现（M0 起）
> 控制协议细节见 [CONTROL_API.md](./CONTROL_API.md)

---

## 1. 概述

agent-desktop（守护进程 `agentdesk`）是一个为 AI Agent 日常驱动 GUI 而设计的 Wayland 合成器：

- **无头优先**：合成器核心永远运行在 headless 后端上，默认不打开任何可见窗口，可在无显示环境的服务器/容器中运行；
- **嵌套运行**：在用户已有的 Wayland 会话中作为普通用户进程运行；用户可随时开关一个嵌套"查看器"窗口观察合成器画面——**画面隐藏与否不影响合成器内部状态与 Agent 的任何操作**；
- **Agent 控制平面**：Unix domain socket + JSON-RPC 2.0，提供窗口清单（JSON）、整屏/单窗口截图（PNG/JPEG/raw）、键鼠注入、窗口与布局控制、事件订阅；
- **XWayland 外置**：X11 应用由独立进程 [xwayland-satellite](https://github.com/Supreeeme/xwayland-satellite) 接入，本程序不集成 XWayland。

技术基座：[Smithay](https://github.com/Smithay/smithay) 0.7（纯 Rust 合成器框架）。

```
一个典型使用场景：

  用户 Wayland 会话 (GNOME/Sway/…)
    └─ $ agentdesk --instance main        # 无头启动，屏幕上什么都没有
    └─ $ agdctl launch -- firefox         # Agent 让 firefox 连到 agentdesk
    └─ $ agdctl windows list              # JSON 拿到窗口清单
    └─ $ agdctl screenshot 3 -o v.png     # 单窗口截图给 VLM 看
    └─ $ agdctl click 640 360             # 注入鼠标点击
    └─ $ agdctl show                      # 用户想看时，弹出嵌套预览窗口
```

## 2. 目标与非目标

### 目标

| # | 目标 | 说明 |
|---|------|------|
| G1 | 确定性窗口管理 | 窗口有稳定自增 id、明确几何；布局/焦点可被 API 精确控制 |
| G2 | 全功能无头运行 | 无显示设备时截图、注入、事件等能力全部可用 |
| G3 | 画面可见性热切换 | show/hide 嵌套查看器，**不扰动**窗口坐标与状态 |
| G4 | 输入注入走真实路径 | 事件进入 seat 状态机，与真实设备不可区分 |
| G5 | 单窗口截图语义正确 | 只含目标窗口内容（含子表面/弹窗），不受遮挡影响 |
| G6 | 事件可观测 | 窗口生命周期、标题/几何变化、焦点、显示开关均可订阅 |
| G7 | X11 应用可用 | 通过 xwayland-satellite，对合成器透明 |
| G8 | 低运维 | 单二进制 + 一个 CLI；无特权要求；不依赖 DRM/udev/seatd |

### 非目标（v1）

- 不做完整桌面环境：无面板、通知守护、会话管理、锁屏、无障碍；
- 不做 DRM/standalone 独立会话（不抢占 VT，不直接管理物理显示器）；
- 不绘制窗口装饰（客户端装饰；X11 窗口无装饰）；
- 不做复杂平铺模型（scrolling/stacking），只提供浮动 + 网格排列；
- 不做多用户与网络远程访问（无 TCP/TLS；远程场景由 Agent 侧 SSH 端口转发 UDS 解决）；
- **不集成 XWayland**（明确需求，见 §9）。

## 3. 需求 → 设计决策映射

| 原始需求 | 决策 | 章节 |
|---|---|---|
| 基于 wlroots | 改用 **Smithay 0.7**（wlroots-rs 无维护，用户确认替换） | §5.1 |
| 语言 Rust | 纯 Rust 技术栈：smithay + sctk + serde，无自维护 FFI | §12 |
| 无头 | HeadlessBackend + 虚拟输出；核心永远无头 | §5.2 / §7.1 |
| 嵌套运行 | 作为父会话普通进程；查看器是进程内 sctk 客户端窗口 | §5.2 / §7.1 |
| 画面可见性切换 | `display.show/hide` 启停查看器线程，窗口坐标零扰动 | §5.2 / §7.1 |
| 查看窗口（JSON） | `windows.list/get`，WindowRecord 全量元数据 | §7.2 |
| 查看窗口（图片） | `windows.screenshot` / `screen.capture`，离屏渲染 | §7.4 |
| 模拟键鼠 | 注入 smithay seat 状态机；合成 keymap 支持文本输入 | §7.3 |
| XWayland 外置 | xwayland-satellite 嵌套合成器模式 | §9 |

## 4. 总体架构

```
                     ┌─────────────────────────────────────────────┐
                     │              父会话（用户桌面）              │
                     │        Wayland 合成器 (GNOME/Sway/…)        │
                     └──────────────────▲──────────────────────────┘
                                        │ sctk 客户端窗口（查看器，可随时开/关）
┌───────────────────────────────────────┼──────────────────────────────────────┐
│ agentdesk（单进程；calloop 主循环）    │                                      │
│                                       │                                      │
│  ┌─────────────────┐  帧+命令 channel ┌┴────────────┐                         │
│  │ 显示子系统        │◄──────────────►│ 查看器线程    │──► 嵌套预览窗口（可见）  │
│  │ HeadlessBackend  │                │ (sctk 0.21)  │                         │
│  │ + 虚拟输出 ×N     │                └─────────────┘                         │
│  └───────┬─────────┘                                                        │
│          │ RenderElements                                                   │
│  ┌───────▼─────────┐  ┌───────────────┐  ┌────────────────────┐              │
│  │ 窗口管理 (ad-wm) │  │ 输入注入       │  │ 截图 / 帧导出引擎    │              │
│  │ Space + 布局     │  │ seat 状态机    │  │ 离屏渲染 → PNG/JPEG │              │
│  └───────┬─────────┘  └───────▲───────┘  └─────────▲──────────┘              │
│          │                    │                    │                          │
│  ┌───────▼────────────────────┴────────────────────┴───────────────────────┐ │
│  │ 控制平面 (ad-ipc)：UDS + JSON-RPC 2.0 (NDJSON)，事件订阅                  │ │
│  └───────▲──────────────────────────────────────────────▲─────────────────┘ │
│          │                                              │                   │
│  ┌───────┴─────────┐   Wayland 客户端连接   ┌─────────────┴─────────────────┐ │
│  │ adctl / Agent    │◄────────────────────►│ Wayland 服务端                 │ │
│  └─────────────────┘                      │ xdg_shell·viewporter·dmabuf…  │ │
│                                           └─────────────▲─────────────────┘ │
└─────────────────────────────────────────────────────────┼───────────────────┘
                                                          │
                                          ┌───────────────┴────────────────┐
                                          │ 应用客户端                       │
                                          │  · Wayland 原生应用              │
                                          │  · xwayland-satellite（外置进程）│
                                          │      └─ Xwayland ── X11 应用    │
                                          └────────────────────────────────┘
```

| 组件 | 职责 |
|---|---|
| Wayland 服务端 | 接纳应用客户端；xdg_shell/layer_shell/decoration/dmabuf/activation 等协议 |
| 窗口管理（ad-wm） | 窗口生命周期、Space 场景、布局、焦点、WindowRecord |
| 显示子系统 | HeadlessBackend、虚拟输出、按需渲染、帧导出 |
| 查看器线程 | sctk 客户端，在父会话开窗口镜像虚拟输出帧 |
| 输入注入 | 维护虚拟光标/键盘，把合成事件送入 seat 状态机 |
| 截图/帧导出 | 输出级与窗口级离屏渲染、编码、传输 |
| 控制平面（ad-ipc） | JSON-RPC over UDS，方法分发，事件广播 |
| adctl | 命令行客户端（Agent 的 shell 入口） |

## 5. 关键决策与理由

### 5.1 Smithay 替代 wlroots

- 原设想：wlroots（C 库）+ Rust 绑定。社区唯一较完整的绑定 wlroots-rs 停留在 wlroots 0.16 时代（2019 年后基本无维护）；自维护 bindgen 绑定的成本与风险都过高。
- 现方案：**Smithay 0.7.0**（2025-06 发布）——纯 Rust，`backend`/`desktop`/`input`/`wayland` 模块覆盖全部需求，calloop 事件循环与我们的 IPC 天然同构。用户已确认此替换。
- 代价与对策：smithay API 仍在演进（0.6→0.7 有破坏性变更）→ 锁定 `=0.7.0`；wlroots 生态工具（grim/wayshot 等）不可直接使用 → 由自建控制平面与测试客户端替代。

### 5.2 无头核心 + 嵌套查看器（而非"真实嵌套输出"）

"嵌套 + 画面可见性切换"有两个候选方案：

- **方案 A（选定）**：核心只跑 `HeadlessBackend` + 虚拟输出；"可见" = 进程内启动一个 sctk 客户端线程（**查看器**），在父会话开一个普通窗口，镜像虚拟输出帧。
- 方案 B（否决）：运行时附加 wayland/x11 backend 作为真实输出；隐藏即销毁该输出。

选 A 的理由：

1. **坐标稳定性（对 Agent 最关键）**：方案 B 在隐藏时销毁输出，窗口必须迁移到其他输出，几何随之变化，破坏 Agent 的坐标假设；方案 A 下 show/hide 对窗口系统**零扰动**。
2. 单渲染路径：不需要同时维护两套 backend/allocator/renderer 组合及运行时热插拔。
3. 故障隔离：查看器线程崩溃不影响合成器与 Agent 会话。
4. 实现简单：sctk 0.21 的客户端窗口 + 帧回调成熟稳定。

代价：可见时每帧一次内存拷贝（1080p@30fps ≈ 250 MB/s，可接受）。后续可升级 memfd/dmabuf 零拷贝（v2）。父会话仅有 X11 无 Wayland 时，查看器可退化为 winit 实现（v2 扩展）。

### 5.3 输入注入走合成器内部路径

不使用 uinput / libei：无头环境没有会话特权问题，且事件直接进入 seat 状态机（与真实设备同一条路径），确定性和可测试性最好。键盘采用 wtype 同款"合成 keymap"方案支持任意 Unicode 文本（§7.3）。

### 5.4 控制平面 = UDS + JSON-RPC，而非自定义 Wayland 协议

Agent 生态（Python/TS 工具链）直接消费 JSON；请求/响应/事件复用、`socat` 手工调试、鉴权、多连接都是现成模式；与 Wayland 协议版本解耦。（v2 可选：把窗口元数据同步暴露为 `ext-foreign-toplevel-list`，供第三方任务栏等互操作。）

### 5.5 XWayland = 外置 xwayland-satellite

xwayland-satellite 是"嵌套合成器 + Wayland 客户端"：Xwayland 连接到 satellite，satellite 在宿主（本程序）上为每个 X11 窗口创建**普通 `xdg_toplevel`**（标题/app_id/几何从 X11 侧照搬，内容经缓冲转发）。对本程序而言，X11 窗口与普通 Wayland 窗口无异。

- 宿主必需协议：核心协议 + `xdg_shell` + `wp_viewporter`（全部在计划内）；
- 可选增强（启用）：linux-dmabuf、xdg-decoration、xdg-activation、fractional-scale、primary-selection；
- `xwayland_shell_v1`（smithay 的 `xwayland` feature）仅在 satellite 的 `-listenfd` 直连集成模式需要 → 推迟到 v2；
- 识别 X11 窗口：窗口创建时读 `/proc/<client pid>/exe` 判定 satellite 进程 → `origin: "x11"`（读到即缓存，失败降级为 unknown）。

## 6. 模块划分

```
agent-desktop/
├── Cargo.toml                 # workspace
├── docs/
│   ├── DESIGN.md              # 本文档
│   └── CONTROL_API.md         # 控制协议规范
├── crates/
│   ├── ad-ipc/                # 协议类型 + JSON-RPC + UDS server/client（不依赖 smithay）
│   ├── ad-wm/                 # 窗口模型/布局/输入注入/截图（依赖 smithay）
│   ├── agentdesk/             # bin + lib：组装 backend、事件循环、查看器
│   ├── adctl/                 # CLI（依赖 ad-ipc）
│   └── test-client/           # e2e 测试客户端（sctk，画色块/回显键盘输入）
└── .github/workflows/ci.yml
```

依赖方向：`adctl → ad-ipc ← agentdesk`；`agentdesk → ad-wm → smithay`。`test-client → sctk`。

关键类型（`ad-wm`）：

```rust
pub struct WindowId(u64);                      // 全局自增，永不复用

#[derive(Debug, Clone, Serialize)]
pub struct WindowRecord {
    pub id: WindowId,
    pub kind: WindowKind,                      // Toplevel | Layer
    pub origin: WindowOrigin,                  // Wayland | X11 | Unknown
    pub title: Option<String>,
    pub app_id: Option<String>,
    pub pid: u32,                              // wl_client 凭证
    pub geometry: Rectangle<i32, Global>,      // 布局写回的窗口几何
    pub surface_size: Size<i32, Buffer>,       // 最近一次 commit 的内容尺寸
    pub states: WindowStateSet,                // activated/fullscreen/maximized/minimized
    pub output: String,                        // 所在虚拟输出名
    pub created_at_ms: u64,
}

pub trait LayoutEngine {                        // 可替换；内置 Floating / Grid
    fn on_window_added(&mut self, ctx: &mut WmCtx, id: WindowId);
    fn on_window_removed(&mut self, ctx: &mut WmCtx, id: WindowId);
    fn on_focus(&mut self, ctx: &mut WmCtx, id: WindowId);
    fn arrange(&mut self, ctx: &mut WmCtx, output: &Output);
}
```

## 7. 核心子系统设计

### 7.1 显示子系统：虚拟输出、帧导出与查看器

**虚拟输出**。启动时创建 1 个虚拟输出（默认 1920×1080，`--size WxH` 覆盖），命名 `HEADLESS-1`。`outputs.create/destroy`（v1.1）可加更多，供并行 Agent 分区使用。输出从不因可见性变化而增删。

**按需渲染**。无消费者（无查看器、无流订阅）且无截图请求时不渲染，CPU 占用趋近 0。有消费者时按 damage 渲染；查看器侧再叠加帧率上限（默认 30fps）。

**帧导出引擎**。所有画面消费方（查看器、截图、v2 流订阅）共用一条导出管线：
`收集 RenderElements → 渲染到离屏缓冲（GlowRenderer FBO 或 pixman 内存）→ 读出 BGRA → 编码/传输`。

**查看器状态机**：

```
Hidden ──display.show──► Starting ──父会话连接成功+首帧──► Visible
  ▲                                                      │
  └────── display.hide / 嵌套窗口被父 WM 关闭 / 父会话断开 ──┘
```

- `display.show {output?, width?, height?, scale?, title?}`：启动查看器线程；窗口按请求尺寸或输出尺寸创建，等比缩放居中；
- `display.hide` 或用户关闭嵌套窗口 → 线程退出，回到 Hidden；
- 无父会话（`WAYLAND_DISPLAY`/`XDG_SESSION_TYPE` 均缺失）→ 返回错误 `no_parent_session`，核心功能不受影响；
- 状态变化推送 `event/display.changed`。

**合成光标**。headless 没有硬件光标：查看器可见时在输出上叠加一个小箭头 element 跟随注入的指针位置；截图 API 提供 `include_cursor` 开关（默认 false）。

### 7.2 窗口模型与布局

**生命周期**（xdg_shell 处理，全部由 smithay 提供协议实现，我们实现 handler trait）：

1. `xdg_wm_base.get_xdg_surface → new_toplevel`：分配 `WindowId`，写入 WindowRecord，交给布局引擎（默认级联放置），推送 `event/window.created`；
2. `initial_commit`：发送 configure（初始几何）；
3. 运行中：`title/app_id changed`、`geometry changed`（我们写回）、`states changed` → 更新记录、节流推送 `event/window.updated`；
4. `close`：API 或客户端主动销毁 → 清理 Space、推送 `event/window.closed`，`WindowId` 永不复用。

layer-shell 表面同样接纳（smithay `WlrLayerShellState`），在 WindowRecord 中 `kind: "layer"`，渲染在对应层，不参与布局平铺。

**窗口 JSON**（`windows.list/get` 返回，字段表见 [CONTROL_API.md](./CONTROL_API.md)）：`id/title/app_id/pid/origin/geometry/surface_size/states/output/created_at_ms`。

**布局**：

- 默认 `Floating`：新窗口按级联偏移放置（不遮挡已有窗口的左上角附近），完全由 `set_geometry` 驱动——Agent 的确定性优先；
- `layout.tile {direction}` / `layout.cascade`：一次性网格/级联排列当前输出的窗口，之后仍回到浮动；
- 最小/最大尺寸、全屏/最大化/最小化尊重客户端 `set_min_size` 等约束，状态变化推送事件。

**焦点**：`windows.focus` 同时置 `activated` 状态与 `seat.keyboard` 焦点；`event/focus.changed` 推送。

### 7.3 输入注入

**指针**。合成器维护虚拟光标位置（全局坐标，与 `windows.list` 的几何同一坐标系）：

- `move_cursor {x,y}`：绝对移动 → `PointerHandle::motion`（配合 `under_from_surface_tree` 计算命中的 surface 与相对坐标）+ `frame()`；
- `move_relative {dx,dy}`：相对移动，同时喂给 `relative-pointer` 协议；
- `button`：`PointerHandle::button`（evdev 按钮码）+ `frame()`；`click/double_click` 为组合；
- `scroll {dx,dy}`：`PointerHandle::axis`（离散滚动值 + 像素值）+ `frame()`。

**键盘**。seat 启动时即挂载一把**合成键盘**（无头下也有键盘），keymap 初始为 `us` 布局：

- `key {key, action}`：键名 → keysym（xkbcommon `keysym_from_name`，大小写不敏感）→ keycode → `KeyboardHandle::key` + 自动维护 modifier 状态；
- `combo {keys}`：全部按下（可选 hold），逆序释放；
- `type_text {text}`：Unicode → `Keysym::from_char` → keycode。keycode 来自**合成 keymap**：初始化一个包含 ASCII 可打印区与常用符号的 keymap；遇到未收录字符时重建 keymap 追加该字符的映射（低频操作，可接受），无法映射的字符返回 `unsupported_char` 错误；
- 无头下注入的键盘事件对客户端与真实键盘不可区分（同一条 `wl_keyboard` 路径）。

**通用语义**：事件时间戳取单调时钟；所有输入请求在主循环中串行执行（跨连接 FIFO）；注入动作可带 `x/y` 先移动光标（`click`/`scroll` 支持一步到位）。

### 7.4 截图与帧导出

三条捕获路径，共用离屏渲染管线：

| API | 语义 | 实现 |
|---|---|---|
| `screen.capture` | 某虚拟输出的完整画面 | 输出的 RenderElement 列表 → 离屏渲染 → 读出 |
| `windows.screenshot` | 单窗口自身内容 | 窗口 element + 子表面/弹窗（裁剪到窗口框）→ 离屏渲染；带 alpha，不受遮挡影响（满足 G5） |
| `frames.subscribe`（v2） | 输出帧流 | 同管线，按 fps 推送，供 VLM 实时观察；查看器复用 |

- 渲染器：`--renderer auto`（默认）= GlowRenderer（EGL/GPU）可用则用，否则 pixman 软件渲染；`glow`/`pixman` 可强制。GPU 不可用的服务器上依赖 mesa 软件栈（llvmpipe）保证 dmabuf 客户端可用（风险 R4）。
- 格式：`png`（默认，保留 alpha）、`jpeg`（quality 参数）、`raw`（BGRA8888 + stride，供程序化断言）。
- 传输：默认 base64 内联；`to_path`（合成器进程写文件）；`via_fd`（SCM_RIGHTS 传 memfd）。

### 7.5 剪贴板（v1.1）

`clipboard.set {text}`：以合成器身份创建 data source 提供文本（wayland + primary selection 双写）；`clipboard.get`：连接焦点窗口的 data offer 读文本。Agent 与 GUI 应用互拷文本的基础设施。smithay 的 DataDevice/PrimarySelection 状态机现成。

## 8. 控制平面（摘要）

完整规范见 [CONTROL_API.md](./CONTROL_API.md)。要点：

- **传输**：UDS `$XDG_RUNTIME_DIR/agentdesk/<instance>.sock`（默认 instance `main`），0600；NDJSON 帧格式的 JSON-RPC 2.0；长连接，支持服务器主动推送；
- **鉴权**：默认仅同用户（UDS 边界）；可选 `--token`；
- **方法分组**：`server.*` / `windows.*` / `screen.*` / `input.*` / `display.*` / `outputs.*`（v1.1）/ `layout.*` / `clipboard.*`（v1.1）/ `events.*` / `x11.*`；
- **事件**：`window.created/updated/closed`、`focus.changed`、`display.changed`、`output.added/removed`；`events.subscribe` 按类型订阅；
- **客户端**：`adctl`（clap，全命令 JSON 输出）+ `ad-ipc` Rust 库（可嵌入 Agent 框架）；
- 主循环内直接分发（calloop fd source），与合成器共享单线程状态，无锁。

## 9. XWayland 集成（xwayland-satellite，外置）

**启动编排**（不集成，只做编排与观测）：

```bash
# 前置：本程序已启动（WAYLAND_DISPLAY=agentdesk-main）
$ WAYLAND_DISPLAY=agentdesk-main xwayland-satellite :100 &
$ DISPLAY=:100 xterm &                     # X11 应用
```

- `windows.launch` 可代劳：`agdctl launch --x11` 会设置 `DISPLAY` 并把 `WAYLAND_DISPLAY` 指向本合成器，先拉起 satellite（若未运行）再拉起 X11 应用；
- `x11.status`：探测 `$XDG_RUNTIME_DIR/.X11-unix/X<n>` 与 satellite 进程，报告 display 号（尽力而为）；
- 配置项 `--x11-display 100`：约定 display 号，便于脚本。

**行为说明**：

- satellite 创建的窗口 = 普通 `xdg_toplevel`（app_id 来自 WM_CLASS），无需本程序任何特殊处理；
- 已启用协议（viewporter/dmabuf/decoration/activation/fractional-scale/primary-selection）覆盖 satellite 的必需+可选清单；
- `origin: "x11"` 由进程探测得出（§5.5）；
- 已知注意事项（来自 satellite README，写入用户文档）：Java 应用需 `_JAVA_AWT_WM_NONREPARENTING=1`；混合 DPI 下 satellite 取最小显示器 DPI。

**v2 预留**：`-listenfd` 直连模式（本程序持有 Xwayland 的 Wayland listen socket，按需拉起）+ smithay `xwayland_shell_v1` 服务端实现，可实现按需启动与更精确的 X11 关联。

## 10. 线程模型与事件循环

```
主线程  calloop 主循环
  ├─ wayland server dispatcher（客户端请求、surface commit）
  ├─ IPC listener + 每连接 fd source（JSON-RPC 解析、方法分发）
  ├─ calloop Timer（事件节流、查看器帧率上限、launch 超时）
  └─ channel ← 查看器命令（show/hide/resize）
查看器线程  自有 calloop
  ├─ sctk 连接父会话（wayland 客户端）
  ├─ 帧回调 → 请求主循环渲染 → 收帧 → shm buffer commit
  └─ channel → 状态回传（连接/关闭事件）
```

- 所有可变状态归属主循环单线程（calloop 的 `&mut Data` 模式），跨线程仅通过 channel 传所有权数据——无锁；
- 查看器崩溃（panic）由 `catch_unwind` 收敛为回到 Hidden 状态；
- 优雅退出：SIGTERM/SIGINT → `server.shutdown` → 通知所有客户端 → 销毁 socket 文件。

## 11. 安全模型

| 层面 | 措施 |
|---|---|
| 传输边界 | UDS only，socket 0600，目录 `$XDG_RUNTIME_DIR/agentdesk/` 0700；默认同用户可信 |
| 鉴权 | 可选 `--token <t>`：首条消息必须为 `auth`，否则连接关闭 |
| 进程拉起 | `windows.launch` 默认允许（本合成器本就为 Agent 而建）；`--deny-launch` 全局禁用；`--launch-allowlist FILE` 限定可执行文件（两种限制同时设置时 deny 优先） |
| 资源限制 | 最大连接 64；单条消息 ≤ 64 MiB；单次截图 raw ≤ 45 MiB（留 JSON/base64 头部空间）；事件订阅类型白名单、最多 16 项；帧订阅最多一个/连接、fps ≤ 30 且慢客户端单槽合并 |
| 文件写入 | `to_path` 仅允许绝对路径；通过目录 fd 锚定 + `openat(O_NOFOLLOW)` + 临时文件原子 rename 防 TOCTOU/符号链接逃逸；可用 `--capture-output-dir` 限制根目录（文件模式 0600） |
| 威胁模型 | 连上 socket ≈ 拿到用户级输入/观察能力（等同键盘+截屏权限），因此不向同用户之外暴露任何网络接口 |

沙箱化 Agent（容器/namespace）部署：把 socket 挂进沙箱即可，不需要其他共享。

## 12. 依赖与系统要求

**Cargo（关键）**：

| 依赖 | 版本 | 用途 |
|---|---|---|
| smithay | `=0.7.0`，`default-features = false`，features：`desktop`、`wayland_frontend`、`renderer_glow`、`renderer_pixman`、`backend_egl` | 合成器框架（刻意不启用 `backend_drm/udev/libinput/winit/xwayland`） |
| smithay-client-toolkit | `0.21` | 查看器线程 + test-client |
| wayland-server / wayland-client | `0.31`（随 smithay/sctk） | smithay 默认走纯 Rust wayland-backend 实现，无需 libwayland |
| calloop | `0.14` | 事件循环（smithay re-export） |
| xkbcommon | `0.8` | keymap/keysym（输入注入与文本输入） |
| serde / serde_json | latest | IPC 协议 |
| image / png | latest | 截图编码 |
| clap | `4` | adctl |
| tracing + tracing-subscriber | latest | 日志 |
| rustix / libc | latest | socket 权限、memfd |

**系统库**：libxkbcommon（必需）、libpixman（`renderer_pixman`）、libEGL（`renderer_glow`，可选，服务器无 GPU 时自动回退 pixman）。**不需要** libwayland/libinput/libudev/libseat/DRM。

## 13. 测试策略

**test-client**（`crates/test-client`，sctk）：创建 toplevel，纯色填充（颜色来自 argv），点击切换颜色，收到的键盘文本写入窗口标题——为 e2e 提供可断言的可观测面。

**e2e（进程内，headless + pixman，无 GPU 依赖）**：测试进程内启动 compositor lib（临时 `XDG_RUNTIME_DIR`、临时 socket），spawn test-client，通过 `ad-ipc` 客户端断言：

1. `windows.list` 出现窗口且 `app_id` 匹配；
2. `windows.screenshot (raw)` 中心像素 == 客户端绘制颜色；
3. `input.click` 中心 → 等待 `window.updated` → 截图像素变为第二颜色；
4. `input.type_text` → `windows.get` 的 title == 注入文本；
5. `display.show/hide` 在无父会话环境返回 `no_parent_session` 且不影响 2–4；
6. `windows.close` → `event/window.closed` 到达且 id 不复用。

**CI**：ubuntu-latest + `apt install libxkbcommon-dev libpixman-1-dev`；渲染走 pixman。人工冒烟：本机嵌套会话 + satellite + xterm。

## 14. 里程碑

| 里程碑 | 范围 | 验收 |
|---|---|---|
| M0 骨架 | workspace、headless backend、1 虚拟输出、渲染器选择 spike、IPC 骨架 | `agentdesk` 启动后 `agdctl server info` 返回 JSON |
| M1 窗口管理 | xdg_shell 全生命周期、Space/布局/焦点、`windows.*`、窗口事件 | e2e 1；手动开/关窗口事件正确 |
| M2 输入注入 | 指针/键盘/文本/组合全套 | e2e 3–4 |
| M3 截图 | screen + window、三种格式、三种传输 | e2e 2；肉眼校对 PNG |
| M4 查看器 | sctk 线程、show/hide 状态机、display 事件 | 本机嵌套会话演示（视频/截图） |
| M5 生态补全 | satellite 编排、`x11.*`、clipboard、outputs/layer-shell、adctl 打磨 | satellite + xterm 全流程 |
| M6 收尾 | 安全加固、文档、CI、打包（cargo-dist 可选） | CI 全绿 |

## 15. 风险与开放问题

**风险**：

| # | 风险 | 缓解 |
|---|---|---|
| R1 | smithay 0.7 → 后续版本 API 破坏性变更 | 锁定 `=0.7.0`；升级作为独立任务处理 |
| R2 | 窗口级离屏渲染的 element 收集细节（子表面/弹窗裁剪、变换） | 参考 anvil 渲染路径；M3 先做输出级，再做窗口级；raw 格式做像素级单测 |
| R3 | headless + glow 的输出缓冲/呈现路径（shm vs gbm）需 spike | M0 内完成 spike；不可行则 headless 固定 pixman、glow 仅用于父会话查看器 |
| R4 | 无 GPU 服务器上 dmabuf 客户端（firefox 等）的软件渲染路径 | 优先 glow+llvmpipe（mesa）；pixman 仅作 shm-only 兜底；文档标注限制 |
| R5 | satellite 版本行为差异 | 锁定 0.8.x 验证；集成测试用 `--x11-display` 隔离 |
| R6 | `type_text` 非拉丁文本（CJK）经 keymap 注入的覆盖面 | v1 接受（BMP 常用字可映射）；v2 评估 text-input 协议注入路径 |
| R7 | 查看器与父会话 WM 的交互（关闭=隐藏、置顶等）不可控 | 文档明确语义：父 WM 想怎么管就怎么管，我们只响应关闭事件 |

**开放问题**（后续版本讨论）：

- Q1 `frames.subscribe` 流式 API 的编解码与背压策略（v2，VLM 实时观察）；
- Q2 多虚拟输出的工作区语义（Agent 并行任务的分区模型）；
- Q3 远程查看（Wayland 侧 winit 查看器 / Web 查看器）；
- Q4 录制（video/gif 导出）；
- Q5 `ext-foreign-toplevel-list` 互操作暴露。

---

## 16. 实现落地注记（v0.1.0，2026-11-08）

M0–M5 已实现并验证（e2e 5 项 + 单元测试 5 项全绿）。与上文设计稿的实现差异与关键事实记录如下，供后续维护者对照源码。

### 16.1 smithay 0.7 的三个关键事实（与设计稿假设不同）

1. **0.7.0 没有 `backend::headless` 模块**（git master 才有）。§7.1 的"HeadlessBackend"落地为：**虚拟输出 = 普通 `Output` global**（`Output::new` + `create_global::<State>`），无任何 backend 对象；输出增删 = global 增删（`remove_global::<State>`）。呈现时钟 = calloop `Timer` 30Hz tick（headless 无 vblank）。
2. **`calloop-wayland-source` 是客户端侧适配器**；服务端接入用 smithay 自带的 `wayland::socket::ListeningSocketSource` + run 回调中 `display.dispatch_clients/flush_clients`。
3. **0.7 没有 `initial_commit` 钩子**：首个 configure 在 `XdgShellHandler::new_topleft` 中主动发送；`CompositorHandler::commit` 中必须同时调用 `on_commit_buffer_handler` 与 `Window::on_commit()`（否则 `bbox()` 恒为 0）。

### 16.2 其他实现偏差

| 设计稿 | 落地 | 理由 |
|---|---|---|
| §10 IPC 在主循环内以 fd source 处理 | 独立 IPC 线程（accept + 每连接 reader/writer）+ calloop `Channel` 投递 | 规避 calloop 动态 source 生命周期管理；边界仍为消息传递，无共享可变状态 |
| `windows.launch` 同步等待窗口 | **延迟响应**：请求挂起在 `pending_launches`，tick（30Hz）中发现窗口或超时后补发响应 | 在主循环回调里阻塞等待会饿死 Wayland dispatch，造成与被拉起客户端互相死锁 |
| 渲染器 auto（glow→pixman） | v1 固定 **pixman 软渲染**（glow 预留） | headless 确定性最高；渲染管线（离屏 Image + `draw_render_elements` + `copy_framebuffer`）已验证 |
| 键盘 keymap 动态重建（wtype 式） | 固定 **us 布局** + 内置 US 键码表（`ad-wm::keymap`，evdev 码 +8 喂 smithay） | 非 US 字符（CJK）v1 不支持，返回 `unsupported_char`（风险 R6 维持） |
| 单连接多订阅 | 每连接一个订阅（`subscription_id = conn_id`） | v1 够用；API 面不变 |

### 16.3 里程碑状态

- ✅ M0 骨架 + IPC（info/ping/shutdown/auth）
- ✅ M1 窗口管理 + `windows.*` + 事件 + test-client + e2e
- ✅ M2 输入注入（move/click/scroll/key/combo/type_text）
- ✅ M3 截图（screen/window × png/jpeg/raw × base64/file）
- ✅ M4 嵌套查看器（show/hide/status、父会话镜像、崩溃隔离、30Hz 推帧）
- ✅ M5 windows.launch（非阻塞等待）、layout.tile/cascade、outputs.create/destroy、xwayland-satellite 编排、x11.status
- ✅ M5b 剪贴板：set 发布到 DataDevice/PrimarySelection；get 对客户端 selection 走工作线程 fd 读取（2 秒超时），不阻塞主循环
- 🟨 M6 via_fd、`--deny-launch` / `--launch-allowlist`、`--capture-output-dir`（fd 锚定、openat no-follow、原子 rename）均已实现并测试；§11 安全模型各项已有代码路径，新增验证覆盖 token 三次失败断连、事件订阅白名单、截图 45 MiB 资源上限；Glow surfaceless EGL 离屏整屏/单窗口截图管线已接入（真实 Wayland 客户端窗口像素 e2e 已验证）；`frames.subscribe/unsubscribe` 已按 fps 推送 JPEG/base64 帧并测试；`auto` 仍默认 Pixman

### 16.4 已验证清单（e2e，`cargo test --workspace -- --test-threads=1`）

1. 窗口生命周期：created/updated/closed、稳定 id、级联布局、focus/geometry/close
2. 输入注入：点击换色（像素断言）+ 焦点转移 + type_text 进标题 + 光标回读
3. 截图：整屏/单窗口像素断言（红→绿证明注入与渲染一致）、png/jpeg 编码
4. launch/布局/输出：launch 等窗返回 window_id、网格几何精确断言、outputs 增删
5. server.info / display.show 的 no_parent_session 分支
6. clipboard.set/get：合成器发布内容回读 + 测试客户端发布 selection 后的异步读取
