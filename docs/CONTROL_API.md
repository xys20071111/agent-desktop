# agent-desktop 控制协议规范（v1）

Agent 与 `agentdesk` 合成器之间的全部交互走本协议。传输层为 Unix domain socket，应用层为 NDJSON 帧格式的 **JSON-RPC 2.0**。

- 合成器侧实现：`ad-ipc` crate
- 客户端：`adctl` CLI（shell/脚本）与 `ad-ipc` Rust 库（嵌入 Agent 框架）
- API 版本：`1`（`server.info.api_version`；新增方法只增不改，向后兼容）

---

## 1. 传输与帧格式

- **地址**：`$XDG_RUNTIME_DIR/agentdesk/<instance>.sock`，默认 instance 为 `main`；`--socket` 可完全自定义路径。客户端环境变量 `AGENTDESK_SOCKET` 优先。
- **帧格式**：NDJSON——每条消息（请求、响应、事件）为一行 UTF-8 JSON，以 `\n` 结尾。单行上限 64 MiB。
- **连接**：长连接、有状态（订阅绑定在连接上）；可多连接并存（≤64），每个连接独立订阅。
- **二进制**：图片等二进制数据默认 base64 内联；`via_fd` 模式下 memfd 以 SCM_RIGHTS 附加在对应响应帧之后（见 §5）。
- **握手**：连接建立后服务器立刻推送一条 notification：

```json
{"jsonrpc":"2.0","method":"event/server.hello","params":{"api_version":1,"name":"agentdesk","version":"0.1.0","instance":"main"}}
```

若服务器以 `--token` 启动，客户端的第一条**请求**必须是 `auth`（见 §4.1），否则任何其他请求返回 `-32007 permission_denied` 且连续 3 次失败后断开。

## 2. 通用约定

- **坐标系**：全局布局坐标（与窗口 `geometry` 同一坐标系），原点为主虚拟输出左上角，单位为逻辑像素（scale=1）。
- **窗口 id**：稳定自增 u64，从 1 开始，永不复用。
- **按钮名**：`left`、`right`、`middle`、`back`、`forward` 或数字（evdev 码）。
- **键名**：xkb keysym 名，大小写不敏感：`a`、`1`、`space`、`Return`、`Tab`、`Escape`、`F5`、`Left`、`Home`、`comma`……
- **修饰键**：`ctrl`、`alt`、`shift`、`super`（组合键写法 `["ctrl","shift","t"]`）。
- **时间**：毫秒级 Unix 时间戳字段以 `_ms` 结尾；注入事件内部时间戳为单调时钟。
- **幂等性**：除输入注入外，所有方法幂等；输入请求全局 FIFO 串行。

### 错误码

| 码 | 名称 | 说明 |
|---|---|---|
| -32700/-32600/-32601/-32602/-32603 | JSON-RPC 标准错误 | parse / invalid request / method not found / invalid params / internal |
| -32000 | internal_error | 通用内部错误，`data` 带细节 |
| -32001 | window_not_found | 窗口不存在或已关闭 |
| -32002 | output_not_found | 输出不存在 |
| -32003 | capture_failed | 截图渲染/编码失败 |
| -32004 | no_parent_session | 无父 Wayland 会话，查看器无法显示 |
| -32005 | launch_failed | `windows.launch` 的进程启动失败 |
| -32006 | unsupported_char | `type_text` 遇到无法映射的字符 |
| -32007 | permission_denied | 未通过 token 鉴权 / launch 被策略拒绝 |
| -32008 | busy | 资源上限（连接数/订阅数） |

## 3. 方法参考

### 3.1 server

| 方法 | 参数 | 返回 |
|---|---|---|
| `server.info` | – | 见下 |
| `server.ping` | – | `{"pong":true,"ts_ms":…}` |
| `server.shutdown` | – | `{"ok":true}`（随后进程退出） |
| `auth` | `{"token":"…"}` | `{"ok":true}` |

`server.info` 返回：

```json
{
  "name": "agentdesk", "version": "0.1.0", "api_version": 1,
  "pid": 4242, "uptime_ms": 1337,
  "socket": "/run/user/1000/agentdesk/main.sock",
  "wayland_display": "agentdesk-main",
  "parent_session": "wayland",
  "viewer": {"visible": false},
  "renderer": "glow",
  "outputs": [{"id":"HEADLESS-1","width":1920,"height":1080,"scale":1.0,"position":{"x":0,"y":0}}],
  "x11": {"available": false, "display": null}
}
```

### 3.2 windows

| 方法 | 参数 | 返回 |
|---|---|---|
| `windows.list` | `{"filter":{"app_id":"…","title_contains":"…","origin":"x11","output":"HEADLESS-1"}}`（均可省略） | `{"windows":[WindowJson…]}` |
| `windows.get` | `{"id":7}` | `WindowJson` |
| `windows.launch` | `{"argv":["firefox"],"cwd":"…","env":{"K":"V"},"x11":false,"wait_for_window_ms":5000}` | `{"pid":123,"window_id":7|null}` |
| `windows.close` | `{"id":7}` | `{"ok":true}` |
| `windows.focus` | `{"id":7}` | `{"ok":true}` |
| `windows.raise` | `{"id":7}` | `{"ok":true}` |
| `windows.set_geometry` | `{"id":7,"x":40,"y":40,"width":1024,"height":768}`（均可部分省略） | `{"ok":true}` |
| `windows.minimize` / `windows.restore` | `{"id":7}` | `{"ok":true}` |
| `windows.set_fullscreen` | `{"id":7,"enabled":true}` | `{"ok":true}` |

`windows.launch`：子进程环境自动注入 `WAYLAND_DISPLAY=agentdesk-<instance>`；`x11:true` 时改为注入 `DISPLAY=:<n>`（satellite 未运行则先拉起）。`wait_for_window_ms` 内出现该 pid 的窗口则回填 `window_id`，否则为 `null`（进程仍已启动）。

`WindowJson`：

```json
{
  "id": 7,
  "kind": "toplevel",
  "origin": "wayland",
  "title": "Mozilla Firefox",
  "app_id": "org.mozilla.firefox",
  "pid": 12345,
  "geometry": {"x": 40, "y": 40, "width": 1024, "height": 768},
  "surface_size": {"width": 1024, "height": 768},
  "states": ["activated"],
  "output": "HEADLESS-1",
  "focused": true,
  "min_size": null,
  "max_size": null,
  "created_at_ms": 1762498800000
}
```

`states` 取值：`activated`、`fullscreen`、`maximized`、`minimized`、`resizing`。`origin` 取值：`wayland`、`x11`、`unknown`。

### 3.3 截图

| 方法 | 参数 | 返回 |
|---|---|---|
| `screen.capture` | `{"output":"HEADLESS-1","format":"png","quality":90,"include_cursor":false,"to_path":"…","via_fd":false}` | ImageResult |
| `windows.screenshot` | `{"id":7,"format":"png","include_cursor":false,"to_path":"…","via_fd":false}` | ImageResult |

- `format`：`png`（默认，含 alpha）｜`jpeg`（仅 `screen.capture`，`quality` 1–100）｜`raw`（BGRA8888 顶层向下，`stride = width*4`）。
- `to_path` 优先于内联返回；`via_fd` 返回不带 `data`，fd 附加在响应上。

`ImageResult`：

```json
{"format":"png","width":1024,"height":768,"encoding":"base64",
 "data":"iVBORw0KGgo…","sha256":"…"}
```

`encoding` 取值：`base64` ｜ `file`（`data` 换成 `path`）｜ `fd`。

### 3.4 input

| 方法 | 参数 | 说明 |
|---|---|---|
| `input.move_cursor` | `{"x":640,"y":360}` | 绝对移动 |
| `input.move_relative` | `{"dx":-40,"dy":10}` | 相对移动 |
| `input.button` | `{"button":"left","action":"press"}` | `press`/`release`/`click`/`double_click` |
| `input.click` | `{"x":640,"y":360,"button":"left","count":1}` | 可先移动再点击，`count` 支持双击 |
| `input.scroll` | `{"dx":0,"dy":-3,"x":…,"y":…}` | 离散滚动格数，可先移动 |
| `input.key` | `{"key":"Return","action":"tap"}` | `press`/`release`/`tap` |
| `input.combo` | `{"keys":["ctrl","c"],"hold_ms":0}` | 顺序按下、逆序释放 |
| `input.type_text` | `{"text":"hello 你好","delay_ms_per_key":0}` | 逐字符 tap |
| `input.get_cursor` | – | `{"x":640,"y":360}` |

### 3.5 display（嵌套查看器可见性）

| 方法 | 参数 | 返回 |
|---|---|---|
| `display.show` | `{"output":"HEADLESS-1","width":1280,"height":800,"scale":1.0,"title":"agentdesk"}` | `{"visible":true,"window_size":{"width":1280,"height":800}}` |
| `display.hide` | – | `{"visible":false}` |
| `display.status` | – | `{"visible":false,"output":"HEADLESS-1","parent_session":"wayland"}` |

无父会话时 `display.show` 返回 `-32004 no_parent_session`，其他功能不受影响。用户手动关闭嵌套窗口等价于 `display.hide`，并推送 `event/display.changed`。

### 3.6 outputs / layout（v1.1）

| 方法 | 参数 | 说明 |
|---|---|---|
| `outputs.list` | – | 虚拟输出清单 |
| `outputs.create` | `{"width":1920,"height":1080,"name":"HEADLESS-2"}` | 新输出排在主输出右侧 |
| `outputs.destroy` | `{"id":"HEADLESS-2"}` | 其上窗口迁移到主输出 |
| `layout.tile` | `{"output":"HEADLESS-1","direction":"grid","padding":8}` | 一次性网格排列 |
| `layout.cascade` | `{"output":"HEADLESS-1","offset":32}` | 一次性级联排列 |

### 3.7 clipboard（v1.1）

| 方法 | 参数 | 返回 |
|---|---|---|
| `clipboard.set` | `{"text":"…"}` | `{"ok":true}`（wayland + primary 双写） |
| `clipboard.get` | `{"which":"regular"}` | `{"text":"…"}`（`regular`｜`primary`；无文本返回 `null`） |

### 3.8 events

| 方法 | 参数 | 返回 |
|---|---|---|
| `events.subscribe` | `{"types":["window.*","focus.changed"]}` | `{"subscription_id":1}` |
| `events.unsubscribe` | `{"subscription_id":1}` | `{"ok":true}` |

事件类型支持通配 `*`（仅段级）：`window.created`、`window.updated`、`window.closed`、`focus.changed`、`display.changed`、`output.added`、`output.removed`。

### 3.9 x11

| 方法 | 参数 | 返回 |
|---|---|---|
| `x11.status` | – | `{"available":true,"display":100,"socket":"/run/user/1000/.X11-unix/X100"}` |

尽力而为探测（X socket 存在性 + satellite 进程），不保证 X11 应用可用性。

## 4. 事件

服务器推送的 notification，`method` 以 `event/` 前缀：

```json
{"jsonrpc":"2.0","method":"event/window.created","params":{"window":{…WindowJson…}}}
{"jsonrpc":"2.0","method":"event/window.updated","params":{"id":7,"changed":["title","geometry"],"window":{…}}}
{"jsonrpc":"2.0","method":"event/window.closed","params":{"id":7}}
{"jsonrpc":"2.0","method":"event/focus.changed","params":{"previous_id":7,"id":9}}
{"jsonrpc":"2.0","method":"event/display.changed","params":{"visible":false}}
{"jsonrpc":"2.0","method":"event/output.added","params":{"output":{…}}}
```

节流策略：`window.updated` 中 geometry 连续变化按 ~16ms 合并；其余立即推送。

## 5. `via_fd` 图像传输

请求带 `"via_fd":true` 时，合成器创建 memfd（或 `to_path` 语义下的普通文件不适用此模式），图像写入后 fd 以 SCM_RIGHTS 附加在响应帧之后。客户端按"读一行 JSON → 立即 recvmsg 收 fd"处理。此模式避免大图 base64 的 ~33% 膨胀，适合高频截图。

## 6. adctl 命令映射

```
agdctl server info | ping | shutdown
agdctl auth <token>
agdctl windows list [--app-id X] [--title-contains X] [--origin x11] [--json]
agdctl windows get <id>
agdctl launch [--x11] [--wait 5000] -- <argv…>
agdctl close <id> | focus <id> | raise <id>
agdctl geometry <id> --x 40 --y 40 --width 1024 --height 768
agdctl screenshot <id|screen> [-o out.png] [--format png|jpeg|raw] [--cursor]
agdctl click <x> <y> [--button left] [--count 2]
agdctl move <x> <y> | move-rel <dx> <dy> | scroll <dy>
agdctl key <key> | combo <k1 k2 …> | type <text>
agdctl show [--size 1280x800] | hide | status
agdctl tile [--direction grid] | cascade
agdctl clipboard get | set <text>
agdctl watch [pattern…]            # 订阅并打印事件流
agdctl --socket PATH …              # 指定控制 socket
```

所有命令支持 `--json` 输出原始响应；错误以非零退出码 + stderr JSON 呈现。

## 7. 典型 Agent 会话

```jsonc
// 1. 看看现在有什么窗口
→ {"jsonrpc":"2.0","id":1,"method":"windows.list"}
← {"jsonrpc":"2.0","id":1,"result":{"windows":[{ "id":3, "app_id":"org.mozilla.firefox", "geometry":{"x":40,"y":40,"width":1024,"height":768}, …}]}}

// 2. 把窗口摆好并聚焦
→ {"jsonrpc":"2.0","id":2,"method":"windows.set_geometry","params":{"id":3,"x":0,"y":0,"width":1920,"height":1080}}
→ {"jsonrpc":"2.0","id":3,"method":"windows.focus","params":{"id":3}}

// 3. 截图观察
→ {"jsonrpc":"2.0","id":4,"method":"windows.screenshot","params":{"id":3,"format":"png"}}
← {"jsonrpc":"2.0","id":4,"result":{"format":"png","width":1920,"height":1080,"encoding":"base64","data":"…","sha256":"…"}}

// 4. 操作：点地址栏、输入网址、回车
→ {"jsonrpc":"2.0","id":5,"method":"input.click","params":{"x":640,"y":24}}
→ {"jsonrpc":"2.0","id":6,"method":"input.type_text","params":{"text":"https://example.com"}}
→ {"jsonrpc":"2.0","id":7,"method":"input.key","params":{"key":"Return","action":"tap"}}

// 5. 等待页面渲染后再截图（订阅事件或轮询）
→ {"jsonrpc":"2.0","id":8,"method":"screen.capture","params":{"format":"jpeg","quality":80}}
```

shell 等价：`agdctl launch -- firefox && agdctl geometry 3 --width 1920 --height 1080 && agdctl screenshot 3 -o page.png && agdctl click 640 24 && agdctl type "https://example.com" && agdctl key Return`。
