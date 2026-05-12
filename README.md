# native-ime

Linux 原生 IME 客户端桥接库，供嵌入自定义渲染循环的宿主应用使用。

目标场景：游戏引擎（Bevy、Godot 等）、跨平台 UI 框架、离屏渲染器（CEF OSR、WebView2 等）、
以及任何自己管理渲染但需要系统输入法支持的应用。这类应用无法直接走 Wayland / X11
窗口系统的 IME 协议（因为没有真正的系统窗口），本库通过 D-Bus 直连 IBus / Fcitx 4 / Fcitx5，
完全绕开窗口系统约束。

## 架构

```
crates/
  ime-core/    核心类型与 backend trait（ImeEvent, ImeBackend, ImeEngine）
  ime-ibus/    IBus D-Bus backend（GNOME / Ubuntu）
  ime-fcitx4/  Fcitx 4 D-Bus backend（Kylin V10 / Sogou IME 等旧版环境）
  ime-fcitx5/  Fcitx5 D-Bus backend（KDE / Arch）
  ime-ffi/     C ABI 导出层 → 编译产物 libnative_ime.so
  ime-poc/     命令行验证程序（无 GUI，直接测试 Preedit / Commit 流程）
```

## C ABI（ime-ffi）

适合从任何能调用 C 共享库的语言使用（C#、Python、Lua、GDScript 等）。

| 函数 | 说明 |
|------|------|
| `ime_create()` | 自动检测 Fcitx5 / Fcitx 4 / IBus，返回不透明句柄；失败返回 null |
| `ime_destroy(handle)` | 销毁句柄，释放所有资源 |
| `ime_focus_in(handle)` | 通知输入法获得焦点 |
| `ime_focus_out(handle)` | 通知输入法失去焦点 |
| `ime_set_cursor_rect(handle, x, y, w, h)` | 提供光标位置，供输入法定位候选窗 |
| `ime_set_surrounding_text(handle, text, cursor, anchor)` | 提供光标周围文本上下文 |
| `ime_backend_kind(handle)` | 返回当前 backend 类型；null 返回 0 |
| `ime_capabilities(handle)` | 返回当前 backend 能力位；null 返回 0 |
| `ime_set_content_type(handle, type)` | 设置输入类型提示；null 或未知 type 会被忽略 |
| `ime_reset(handle)` | 重置输入状态 |
| `ime_process_key_event(handle, keysym, keycode, state, is_release)` | 转发按键（X11 keysym），返回 1 = 输入法已消费 |
| `ime_poll_event(handle, out_data)` | 取出下一个 IME 事件（Preedit / Commit 等），返回 0 = 无事件 |

`ime_process_key_event` 接受 **X11 keysym**（如字母 `'a'` = `0x0061`，回车 `0xff0d`）。
宿主负责将自身的按键表示转换为 keysym；每种框架只需维护一份转换映射。

`ime_create` 返回的 handle 由宿主管理生命周期。可以从任意线程调用同一个 handle，
但宿主必须保证 `ime_destroy` 不会与其他 handle 调用并发执行，且同一 handle 只能销毁一次。

### backend 类型与能力位

`ime_backend_kind` 返回值：

| 值 | backend |
|----|---------|
| 0 | None / Unknown |
| 1 | Fcitx5 |
| 2 | Fcitx 4 |
| 3 | IBus |

`ime_capabilities` 返回 `u32` 位集合：

| 位 | 能力 |
|----|------|
| bit 0 | Preedit event |
| bit 1 | Commit event |
| bit 2 | ForwardKey event |
| bit 3 | DeleteSurroundingText event |
| bit 4 | `ime_set_surrounding_text` |
| bit 5 | `ime_set_content_type` |

当前 Fcitx5 声明 Preedit / Commit / ForwardKey / surrounding text；Fcitx 4 声明
Preedit / Commit / ForwardKey / DeleteSurroundingText / surrounding text；IBus 声明
Preedit / Commit / ForwardKey / DeleteSurroundingText / surrounding text / content type。
未知 `type` 枚举值会被忽略。

`ime_set_content_type` 的 `type` 值：

| 值 | 类型 |
|----|------|
| 0 | Normal |
| 1 | Password |
| 2 | Number |
| 3 | Phone |
| 4 | Url |
| 5 | Email |

### 事件类型（ime_poll_event 返回值）

| 值 | 类型 | 有效字段 |
|----|------|----------|
| 0 | 无事件 | — |
| 1 | Preedit | text, cursor_begin, cursor_end |
| 2 | PreeditEnd | — |
| 3 | Commit | text |
| 4 | DeleteSurroundingText | param1 = before, param2 = after |
| 5 | ForwardKey | param1 = keysym, param2 = state |

## Rust API（ime-core / ime-ibus / ime-fcitx4 / ime-fcitx5）

如果宿主本身是 Rust 项目，可以直接依赖这些 crate，绕过 C ABI：

```toml
[dependencies]
ime-core = { path = "..." }
ime-ibus = { path = "..." }
ime-fcitx4 = { path = "..." }
ime-fcitx5 = { path = "..." }
```

使用 `ImeBackend` trait 和 `ImeEngine` 即可，不需要引入 `ime-ffi`。

## 构建

需要 Rust 工具链（[rustup.rs](https://rustup.rs)）。D-Bus 依赖仅在 Linux 上有效。

Linux 本机构建（C ABI 共享库）：

```bash
cargo build --release -p ime-ffi
# 产物：target/release/libnative_ime.so
```

Unity Linux 插件放置路径固定为：

```text
BrowserRenderer/Assets/Packages/Plugins/Linux/libnative_ime.so
```

构建完成后将 `libnative_ime.so` 复制到该目录。Unity 导入配置仅启用 Linux Editor
和 Linux x86_64 Player，不会影响 Windows 链路。

Linux 从 Windows 交叉编译（需要 [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild)）：

```powershell
cargo zigbuild --release -p ime-ffi --target x86_64-unknown-linux-gnu
# 产物：target/x86_64-unknown-linux-gnu/release/libnative_ime.so
```

运行 PoC（在 Linux 上验证 IBus / Fcitx 4 / Fcitx5 连接）：

```bash
RUST_LOG=debug cargo run -p ime-poc
# 手动输入文本行并逐键发送：
RUST_LOG=debug cargo run -p ime-poc -- --interactive
# 设置 surrounding text 后再发送默认测试按键：
RUST_LOG=debug cargo run -p ime-poc -- --surrounding-text "hello" --cursor 5
# 诊断时显式打印真实输入/事件文本：
RUST_LOG=debug cargo run -p ime-poc -- --log-text --interactive
```

PoC 启动后会打印当前 backend 名称、backend kind 和 capability bits；默认模式仍自动发送
`nihao + Return`，interactive 模式只做基础 X11 keysym 映射：ASCII 字符直接使用码位，
Return 使用 `0xff0d`，不尝试覆盖完整键盘布局。PoC 默认只打印事件类型、UTF-8 byte
length、字符数、cursor/anchor 等摘要，不打印真实输入、Preedit、Commit 或 surrounding text
内容；需要临时诊断文本本身时，必须显式传入 `--log-text`。
可通过 `--surrounding-text <text>` 在 `focus_in` 和 `set_cursor_rect` 后设置光标周围文本；
`--cursor <n>` / `--anchor <n>` 使用 UTF-8 byte offset，未传时默认 `cursor=text.len()`、
`anchor=cursor`。PoC 只打印文本 byte length、cursor 和 anchor；如果当前 backend 未声明
surrounding text capability，会打印提示并跳过调用。

## 回退机制

`ime_create` 在检测不到 IBus / Fcitx 4 / Fcitx5 时返回 null，宿主应据此回退到自己的 IME 处理路径。

在 EmbeddedBrowser Unity 端，`NativeImeBridge` 只在 Linux Editor / Linux Player 中尝试加载
`libnative_ime.so`。插件缺失、符号不匹配或 `ime_create` 返回 null 时，会继续使用现有
`ImeModule` 的 Unity IME 路径。

## 当前限制

- Fcitx5 backend 已接通 `set_surrounding_text`、光标矩形、按键处理和 Preedit / Commit 事件。
- Fcitx 4 backend 已接通 `set_surrounding_text`、光标矩形、按键处理和 Preedit / Commit 事件。Fcitx 4 的
  InputContext 绑定创建它的 D-Bus sender，因此必须由库内部复用同一个连接创建上下文、
  调用方法和接收信号；用 `dbus-send` 分多次手工调用 `/inputcontext_*` 会触发
  `Invalid sender`，不代表 backend 不可用。
- IBus backend 已接通 `set_surrounding_text`、`set_content_type`、光标矩形、按键处理和
  Preedit / Commit 事件。
- `DeleteSurroundingText` 已从 native-ime 透出到 Unity adapter，并通过 HeadlessBrowser IME
  队列映射为 Backspace / Delete 键事件。
- 当前 Unity 接入是 Linux-only 增强路径，不替换 Windows 或现有共享内存 IME 协议。
