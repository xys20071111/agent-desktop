# agent-desktop

面向 AI Agent 的无头 Wayland 合成器。基于 [Smithay](https://github.com/Smithay/smithay) 0.7，纯 Rust。

- **无头优先**：核心运行在虚拟输出上，服务器/容器内也能完整工作（截图、键鼠注入、事件）。
- **嵌套运行 + 画面热切换**：跑在用户已有 Wayland 会话里；`display.show/hide` 随时开关一个嵌套预览窗口，不影响窗口坐标与状态。
- **Agent 控制平面**：UDS + JSON-RPC 2.0——窗口清单（JSON）、整屏/单窗口截图（PNG/JPEG/raw）、键鼠注入、窗口/布局/输出控制、事件订阅、应用拉起。
- **XWayland 外置**：X11 应用由独立进程 [xwayland-satellite](https://github.com/Supreeeme/xwayland-satellite) 接入，本程序不集成 XWayland。

## 文档

- [设计文档](docs/DESIGN.md)——架构、关键决策、子系统设计、实现落地注记
- [控制协议规范](docs/CONTROL_API.md)——JSON-RPC 方法/事件/错误码全集与 adctl 用法

## 快速开始

构建（需要 libxkbcommon、libpixman-1 开发头文件；无需 libwayland/libudev/DRM）：

```bash
cargo build --release
```

一个完整会话（在本机 Wayland 会话中）：

```bash
./target/release/agentdesk --instance main --print-info &   # 无头启动
export AGENTDESK_SOCKET=/run/user/$UID/agentdesk/main.sock

agdctl launch --wait 8000 -- firefox       # 拉起应用（自动连入 agentdesk）
agdctl windows list                        # 窗口清单（JSON）
agdctl tile                                # 网格布局
agdctl screenshot screen -o desktop.png    # 整屏截图
agdctl screenshot 3 -o win.png             # 单窗口截图（不受遮挡）
agdctl click 640 360                       # 注入点击
agdctl type "hello"                        # 注入文本
agdctl show --size 1280x800                # 弹出嵌套预览窗口（热切换）
agdctl hide                                # 收起预览
agdctl watch                               # 订阅事件流
```

X11 应用：`agdctl launch --x11 --wait 8000 -- xterm`（自动拉起 `xwayland-satellite :100`）。

## 测试

```bash
cargo test --workspace -- --test-threads=1
```

e2e 为子进程形态（临时 `XDG_RUNTIME_DIR`，无 GPU 依赖）：窗口生命周期、输入注入像素断言、截图像素断言、launch/布局/输出。

## 状态

**M0–M5 已实现并验证**（e2e 5 项 + 单元测试 5 项全绿；嵌套查看器与注入/截图链路经真机演示）：

| 已有 | 说明 |
|---|---|
| 窗口管理 | xdg_shell 全生命周期、级联/网格布局、焦点、稳定 id |
| 截图 | 整屏 + 单窗口，png/jpeg/raw，base64/文件 |
| 键鼠注入 | 移动/点击/滚动/按键/组合/文本（US 布局，走真实输入路径） |
| 嵌套查看器 | show/hide 热切换，父会话窗口实时镜像，崩溃隔离 |
| 应用编排 | windows.launch（等待窗口出现）、xwayland-satellite 编排、x11.status |
| 多输出 | outputs.create/destroy/list（虚拟输出热插拔） |
| 事件 | window.created/updated/closed、focus/display/output 变化，通配订阅 |

计划中：剪贴板（set/get）、glow 渲染器（GPU 加速）、`frames.subscribe` 帧流、`via_fd` 零拷贝截图、X11 父会话查看器。见[设计文档 §15](docs/DESIGN.md)。

构建系统依赖（运行时）：libxkbcommon、libpixman-1；libEGL 可选（glow 渲染器启用后）。
