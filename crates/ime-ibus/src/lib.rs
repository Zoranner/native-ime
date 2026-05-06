//! IBus D-Bus backend
//!
//! 通过 D-Bus 连接 IBus daemon，实现 ImeBackend trait。
//! 覆盖 GNOME / Ubuntu 等默认使用 IBus 的 Linux 桌面环境。
//!
//! IBus D-Bus 接口文档：
//!   https://ibus.github.io/docs/ibus-1.5/IBusInputContext.html

mod proxy;

use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Sender;
use futures_util::StreamExt;
use ime_core::{CursorRect, ImeBackend, ImeEvent, KeyState};
use proxy::{IBusBusProxy, IBusInputContextProxy};
use tokio::runtime::Handle;
use tokio::sync::Mutex;

pub struct IBusBackend {
    ctx: Arc<Mutex<IBusInputContextProxy<'static>>>,
    /// 用于从任意线程向 tokio 运行时提交任务
    rt_handle: Handle,
    /// signal_loop 任务句柄；Drop 时 abort，确保运行时能正常关闭
    signal_loop_handle: tokio::task::JoinHandle<()>,
}

impl Drop for IBusBackend {
    fn drop(&mut self) {
        self.signal_loop_handle.abort();
    }
}

impl IBusBackend {
    /// 连接 IBus daemon，启动信号监听循环。
    ///
    /// `event_tx`：由调用方（handle.rs）创建，对应 `ImeEngine` 持有的 `event_rx`。
    /// 必须在 tokio 异步上下文中调用（`Handle::current()` 需有效）。
    pub async fn connect(event_tx: Sender<ImeEvent>) -> anyhow::Result<Self> {
        let conn = zbus::Connection::session().await?;

        let bus = IBusBusProxy::new(&conn).await?;
        let ic_path = bus.create_input_context("native-ime").await?;

        let ctx = IBusInputContextProxy::builder(&conn)
            .path(ic_path)?
            .destination("org.freedesktop.IBus")?
            .build()
            .await?;

        // 能力 flags：
        //   IBUS_CAP_PREEDIT_TEXT     = 1
        //   IBUS_CAP_FOCUS            = 8
        //   IBUS_CAP_SURROUNDING_TEXT = 32
        ctx.set_capabilities(1 | 8 | 32).await?;

        let ctx = Arc::new(Mutex::new(ctx));
        let rt_handle = Handle::current();

        // 信号监听循环在当前 tokio 运行时中 spawn；
        // 通过 oneshot 等待其完成所有 D-Bus 流订阅后再返回，
        // 避免 connect() 返回后立即调用 process_key_event 时产生锁竞争
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let signal_loop_handle = rt_handle.spawn(signal_loop(ctx.clone(), event_tx, ready_tx));
        let _ = ready_rx.await;

        Ok(Self {
            ctx,
            rt_handle,
            signal_loop_handle,
        })
    }
}

impl ImeBackend for IBusBackend {
    fn focus_in(&self) {
        let ctx = self.ctx.clone();
        self.rt_handle.spawn(async move {
            let ctx = ctx.lock().await;
            if let Err(e) = ctx.focus_in().await {
                log::warn!("[ibus] focus_in error: {}", e);
            }
        });
    }

    fn focus_out(&self) {
        let ctx = self.ctx.clone();
        self.rt_handle.spawn(async move {
            let ctx = ctx.lock().await;
            if let Err(e) = ctx.focus_out().await {
                log::warn!("[ibus] focus_out error: {}", e);
            }
        });
    }

    fn set_cursor_rect(&self, rect: CursorRect) {
        let ctx = self.ctx.clone();
        self.rt_handle.spawn(async move {
            let ctx = ctx.lock().await;
            if let Err(e) = ctx
                .set_cursor_location(rect.x, rect.y, rect.width, rect.height)
                .await
            {
                log::warn!("[ibus] set_cursor_location error: {}", e);
            }
        });
    }

    fn set_surrounding_text(&self, text: &str, cursor: i32, anchor: i32) {
        // 明确降级：IBus surrounding text 需要构造 IBusText GVariant 并调用
        // SetSurroundingText。当前版本不声明支持该能力，避免宿主误以为上下文已生效。
        log::debug!(
            "[ibus] set_surrounding_text ignored: '{}' cursor={} anchor={} (unsupported in native-ime 0.1)",
            text,
            cursor,
            anchor
        );
    }

    fn process_key_event(&self, keyval: u32, keycode: u32, state: u32, is_release: bool) -> bool {
        // 将 is_release 编码进 IBus state 的高位 flag
        let mut key_state = state;
        if is_release {
            key_state |= KeyState::RELEASE;
        }

        // 用 std::sync mpsc + rt_handle.spawn 实现跨线程同步等待：
        // - 可在任意（非 tokio）线程调用，不依赖 block_in_place
        // - 超时 200ms 后返回 false，避免宿主主线程永久阻塞
        let (resp_tx, resp_rx) = std::sync::mpsc::sync_channel::<bool>(1);
        let ctx = self.ctx.clone();

        self.rt_handle.spawn(async move {
            let ctx = ctx.lock().await;
            let result = ctx
                .process_key_event(keyval, keycode, key_state)
                .await
                .unwrap_or(false);
            let _ = resp_tx.send(result);
        });

        resp_rx
            .recv_timeout(Duration::from_millis(200))
            .unwrap_or(false)
    }

    fn reset(&self) {
        let ctx = self.ctx.clone();
        self.rt_handle.spawn(async move {
            let ctx = ctx.lock().await;
            if let Err(e) = ctx.reset().await {
                log::warn!("[ibus] reset error: {}", e);
            }
        });
    }
}

/// 监听 IBus D-Bus 信号，将事件推入 ImeEngine 队列。
///
/// `ready_tx`：所有流订阅完成后发送，通知 `connect()` 可以安全返回。
async fn signal_loop(
    ctx: Arc<Mutex<IBusInputContextProxy<'static>>>,
    event_tx: Sender<ImeEvent>,
    ready_tx: tokio::sync::oneshot::Sender<()>,
) {
    let ctx_guard = ctx.lock().await;

    let mut commit_stream = match ctx_guard.receive_commit_text().await {
        Ok(s) => s,
        Err(e) => {
            log::error!("[ibus] Failed to subscribe commit-text: {}", e);
            return;
        }
    };

    let mut preedit_stream = match ctx_guard.receive_update_preedit_text().await {
        Ok(s) => s,
        Err(e) => {
            log::error!("[ibus] Failed to subscribe update-preedit-text: {}", e);
            return;
        }
    };

    let mut hide_preedit_stream = match ctx_guard.receive_hide_preedit_text().await {
        Ok(s) => s,
        Err(e) => {
            log::error!("[ibus] Failed to subscribe hide-preedit-text: {}", e);
            return;
        }
    };

    let mut delete_stream = match ctx_guard.receive_delete_surrounding_text().await {
        Ok(s) => s,
        Err(e) => {
            log::error!("[ibus] Failed to subscribe delete-surrounding-text: {}", e);
            return;
        }
    };

    let mut forward_key_stream = match ctx_guard.receive_forward_key_event().await {
        Ok(s) => s,
        Err(e) => {
            log::error!("[ibus] Failed to subscribe forward-key-event: {}", e);
            return;
        }
    };

    // 释放锁后再发送就绪信号，确保 process_key_event 能立刻拿到锁
    drop(ctx_guard);
    let _ = ready_tx.send(());

    loop {
        tokio::select! {
            Some(msg) = commit_stream.next() => {
                if let Ok(args) = msg.args() {
                    let text = extract_ibus_text_string(&args.text);
                    log::debug!("[ibus] commit: {:?}", text);
                    send_event(&event_tx, ImeEvent::Commit { text });
                }
            }
            Some(msg) = preedit_stream.next() => {
                if let Ok(args) = msg.args() {
                    let text = extract_ibus_text_string(&args.text);
                    let cursor = args.cursor_pos as i32;
                    log::debug!("[ibus] preedit: {:?} cursor={}", text, cursor);
                    if text.is_empty() {
                        send_event(&event_tx, ImeEvent::PreeditEnd);
                    } else {
                        send_event(&event_tx, ImeEvent::Preedit {
                            text,
                            cursor_begin: cursor,
                            cursor_end: cursor,
                        });
                    }
                }
            }
            Some(_) = hide_preedit_stream.next() => {
                log::debug!("[ibus] hide-preedit");
                send_event(&event_tx, ImeEvent::PreeditEnd);
            }
            Some(msg) = delete_stream.next() => {
                if let Ok(args) = msg.args() {
                    // IBus DeleteSurroundingText 语义：删除从 cursor+offset 起的 nchars 个字符。
                    // offset 为负表示光标前，正表示光标后。转换为 (before, after)：
                    //   before = max(0, -offset)           光标前要删除的字符数
                    //   after  = max(0, offset + nchars)   光标处及之后要删除的字符数
                    let before = (-args.offset).max(0) as u32;
                    let after = (args.offset + args.nchars as i32).max(0) as u32;
                    send_event(&event_tx, ImeEvent::DeleteSurroundingText { before, after });
                }
            }
            Some(msg) = forward_key_stream.next() => {
                if let Ok(args) = msg.args() {
                    log::debug!("[ibus] forward-key: keyval=0x{:x} state=0x{:x}", args.keyval, args.state);
                    send_event(&event_tx, ImeEvent::ForwardKey {
                        keyval: args.keyval,
                        state: KeyState(args.state),
                    });
                }
            }
            else => break,
        }
    }
}

/// 推送事件；队列满时打印警告而非静默丢弃
fn send_event(tx: &Sender<ImeEvent>, event: ImeEvent) {
    if let Err(e) = tx.try_send(event) {
        log::warn!("[ibus] event queue full, dropping event: {}", e);
    }
}

/// 从 IBusText GVariant 提取文本字符串
///
/// IBus D-Bus 信号中 text 参数类型为 `v`（variant），内部包裹 IBusText 结构体。
///
/// IBusText GVariant 格式：`(sa{sv}sv)`
///   - fields[0] = "IBusText"（类型名）
///   - fields[1] = a{sv}（attachment dict，通常为空）
///   - fields[2] = s（实际文本内容）← 目标字段
///   - fields[3] = v（IBusAttrList，文字属性）
fn extract_ibus_text_string(v: &zbus::zvariant::Value<'_>) -> String {
    // IBus 信号将 IBusText 包裹在 variant (`v`) 里，先解开外层
    let inner = match v {
        zbus::zvariant::Value::Value(boxed) => boxed.as_ref(),
        other => other,
    };

    if let zbus::zvariant::Value::Structure(s) = inner {
        let fields = s.fields();
        // 至少需要 3 个字段才能访问 fields[2]
        if fields.len() >= 3 {
            if let zbus::zvariant::Value::Str(text) = &fields[2] {
                return text.to_string();
            }
        }
    }

    String::new()
}
