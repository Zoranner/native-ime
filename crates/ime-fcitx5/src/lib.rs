//! Fcitx5 D-Bus backend
//!
//! 直接使用 zbus proxy 宏实现 Fcitx5 D-Bus 接口，不依赖 fcitx5-dbus crate。
//! 覆盖 KDE / Arch / Manjaro 等使用 Fcitx5 的 Linux 桌面环境。

mod proxy;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use crossbeam_channel::Sender;
use futures_util::StreamExt;
use ime_core::{CursorRect, ImeBackend, ImeEvent, KeyState};
use proxy::{Fcitx5InputContextProxy, Fcitx5InputMethodProxy};
use tokio::runtime::Handle;
use tokio::sync::Mutex;

pub struct Fcitx5Backend {
    ctx: Arc<Mutex<Fcitx5InputContextProxy<'static>>>,
    /// 用于从任意线程向 tokio 运行时提交任务
    rt_handle: Handle,
    /// signal_loop 任务句柄；Drop 时 abort，确保运行时能正常关闭
    signal_loop_handle: tokio::task::JoinHandle<()>,
}

impl Drop for Fcitx5Backend {
    fn drop(&mut self) {
        self.signal_loop_handle.abort();
    }
}

impl Fcitx5Backend {
    /// 连接 Fcitx5 daemon，启动信号监听循环。
    ///
    /// `event_tx`：由调用方（handle.rs）创建，对应 `ImeEngine` 持有的 `event_rx`。
    /// 必须在 tokio 异步上下文中调用（`Handle::current()` 需有效）。
    pub async fn connect(event_tx: Sender<ImeEvent>) -> anyhow::Result<Self> {
        let conn = zbus::Connection::session().await?;

        // 检测 Fcitx5：尝试连接 InputMethod1 接口
        let im = Fcitx5InputMethodProxy::builder(&conn)
            .build()
            .await
            .context("Fcitx5 not available on D-Bus")?;

        // 验证服务可达
        im.version()
            .await
            .context("Fcitx5 InputMethod not responding")?;

        // 创建 InputContext，传递程序标识
        let properties: Vec<(&str, &str)> = vec![("program", "native-ime")];
        let (ic_path, _uuid) = im.create_input_context(properties).await?;
        log::debug!("[fcitx5] InputContext path: {:?}", ic_path);

        let ctx = Fcitx5InputContextProxy::builder(&conn)
            .path(ic_path)?
            .destination("org.fcitx.Fcitx5")?
            .build()
            .await?;

        // 设置能力 flags：surrounding-text = bit0，preedit = bit1
        ctx.set_capability(0b011).await?;

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

impl ImeBackend for Fcitx5Backend {
    fn focus_in(&self) {
        let ctx = self.ctx.clone();
        self.rt_handle.spawn(async move {
            let ctx = ctx.lock().await;
            if let Err(e) = ctx.focus_in().await {
                log::warn!("[fcitx5] focus_in error: {}", e);
            }
        });
    }

    fn focus_out(&self) {
        let ctx = self.ctx.clone();
        self.rt_handle.spawn(async move {
            let ctx = ctx.lock().await;
            if let Err(e) = ctx.focus_out().await {
                log::warn!("[fcitx5] focus_out error: {}", e);
            }
        });
    }

    fn set_cursor_rect(&self, rect: CursorRect) {
        let ctx = self.ctx.clone();
        self.rt_handle.spawn(async move {
            let ctx = ctx.lock().await;
            if let Err(e) = ctx
                .set_cursor_rect(rect.x, rect.y, rect.width, rect.height)
                .await
            {
                log::warn!("[fcitx5] set_cursor_rect error: {}", e);
            }
        });
    }

    fn set_surrounding_text(&self, text: &str, cursor: i32, anchor: i32) {
        let ctx = self.ctx.clone();
        let text = text.to_owned();
        // cursor/anchor 在正常用法下非负；saturating 转换避免 as-cast 产生超大值
        let cursor_u = cursor.max(0) as u32;
        let anchor_u = anchor.max(0) as u32;
        self.rt_handle.spawn(async move {
            let ctx = ctx.lock().await;
            if let Err(e) = ctx.set_surrounding_text(&text, cursor_u, anchor_u).await {
                log::warn!("[fcitx5] set_surrounding_text error: {}", e);
            }
        });
    }

    fn process_key_event(&self, keyval: u32, keycode: u32, state: u32, is_release: bool) -> bool {
        // 同 IBus：用 std::sync mpsc + rt_handle.spawn，可从任意线程安全调用
        let (resp_tx, resp_rx) = std::sync::mpsc::sync_channel::<bool>(1);
        let ctx = self.ctx.clone();

        self.rt_handle.spawn(async move {
            let ctx = ctx.lock().await;
            let result = ctx
                .process_key_event(keyval, keycode, state, is_release, 0u32)
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
                log::warn!("[fcitx5] reset error: {}", e);
            }
        });
    }
}

/// 监听 Fcitx5 D-Bus 信号，将事件推入 ImeEngine 队列。
///
/// `ready_tx`：所有流订阅完成后发送，通知 `connect()` 可以安全返回。
async fn signal_loop(
    ctx: Arc<Mutex<Fcitx5InputContextProxy<'static>>>,
    event_tx: Sender<ImeEvent>,
    ready_tx: tokio::sync::oneshot::Sender<()>,
) {
    let ctx_guard = ctx.lock().await;

    let mut commit_stream = match ctx_guard.receive_commit_string().await {
        Ok(s) => s,
        Err(e) => {
            log::error!("[fcitx5] subscribe commit-string error: {}", e);
            return;
        }
    };

    let mut preedit_stream = match ctx_guard.receive_update_preedit().await {
        Ok(s) => s,
        Err(e) => {
            log::error!("[fcitx5] subscribe update-preedit error: {}", e);
            return;
        }
    };

    let mut forward_key_stream = match ctx_guard.receive_forward_key().await {
        Ok(s) => s,
        Err(e) => {
            log::error!("[fcitx5] subscribe forward-key error: {}", e);
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
                    let text = args.text.to_string();
                    log::debug!("[fcitx5] commit: {:?}", text);
                    send_event(&event_tx, ImeEvent::Commit { text });
                }
            }
            Some(msg) = preedit_stream.next() => {
                if let Ok(args) = msg.args() {
                    // UpdatePreedit: Vec<(text_segment, format_flags)>, cursor_pos
                    let text: String = args
                        .texts
                        .iter()
                        .map(|(t, _flags)| t.as_str())
                        .collect();
                    let cursor = args.cursor_pos;
                    log::debug!("[fcitx5] preedit: {:?} cursor={}", text, cursor);
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
            Some(msg) = forward_key_stream.next() => {
                if let Ok(args) = msg.args() {
                    log::debug!(
                        "[fcitx5] forward-key: keyval=0x{:x} state=0x{:x} release={}",
                        args.keyval, args.state, args.is_release
                    );
                    // Fcitx5 将 is_release 作为独立参数传入，而 IBus 将其编码在 state bit 30。
                    // 统一编码为 KeyState::RELEASE，宿主只需检查一个字段即可。
                    let mut state = args.state;
                    if args.is_release {
                        state |= KeyState::RELEASE;
                    }
                    send_event(&event_tx, ImeEvent::ForwardKey {
                        keyval: args.keyval,
                        state: KeyState(state),
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
        log::warn!("[fcitx5] event queue full, dropping event: {}", e);
    }
}
