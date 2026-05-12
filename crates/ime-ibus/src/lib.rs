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
use ime_core::{
    BackendKind, ContentType, CursorRect, ImeBackend, ImeCapabilities, ImeEvent, KeyState,
};
use proxy::{IBusBusProxy, IBusInputContextProxy};
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use zbus::zvariant::{Array, Signature, Value};

const IBUS_CAP_PREEDIT_TEXT: u32 = 1 << 0;
const IBUS_CAP_FOCUS: u32 = 1 << 3;
const IBUS_CAP_SURROUNDING_TEXT: u32 = 1 << 5;

const IBUS_INPUT_PURPOSE_FREE_FORM: u32 = 0;
const IBUS_INPUT_PURPOSE_NUMBER: u32 = 3;
const IBUS_INPUT_PURPOSE_PHONE: u32 = 4;
const IBUS_INPUT_PURPOSE_URL: u32 = 5;
const IBUS_INPUT_PURPOSE_EMAIL: u32 = 6;
const IBUS_INPUT_PURPOSE_PASSWORD: u32 = 8;

const IBUS_INPUT_HINT_NONE: u32 = 0;
const IBUS_INPUT_HINT_NO_SPELLCHECK: u32 = 1 << 1;

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

        ctx.set_capabilities(IBUS_CAP_PREEDIT_TEXT | IBUS_CAP_FOCUS | IBUS_CAP_SURROUNDING_TEXT)
            .await?;

        let ctx = Arc::new(Mutex::new(ctx));
        let rt_handle = Handle::current();

        // 信号监听循环在当前 tokio 运行时中 spawn；
        // 通过 oneshot 等待其完成所有 D-Bus 流订阅后再返回，
        // 避免 connect() 返回后立即调用 process_key_event 时产生锁竞争
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
        let signal_loop_handle = rt_handle.spawn(signal_loop(ctx.clone(), event_tx, ready_tx));
        if let Err(e) = wait_signal_loop_ready(ready_rx).await {
            signal_loop_handle.abort();
            return Err(e);
        }

        Ok(Self {
            ctx,
            rt_handle,
            signal_loop_handle,
        })
    }
}

impl ImeBackend for IBusBackend {
    fn backend_kind(&self) -> BackendKind {
        BackendKind::IBus
    }

    fn capabilities(&self) -> ImeCapabilities {
        ibus_capabilities()
    }

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
        let ctx = self.ctx.clone();
        let ibus_text = build_ibus_text(text);
        let cursor = cursor.max(0) as u32;
        let anchor = anchor.max(0) as u32;
        self.rt_handle.spawn(async move {
            let ctx = ctx.lock().await;
            if let Err(e) = ctx.set_surrounding_text(ibus_text, cursor, anchor).await {
                log::warn!("[ibus] set_surrounding_text error: {}", e);
            }
        });
    }

    fn set_content_type(&self, content_type: ContentType) {
        let ctx = self.ctx.clone();
        let (purpose, hints) = ibus_content_type(content_type);
        self.rt_handle.spawn(async move {
            let ctx = ctx.lock().await;
            if let Err(e) = ctx.set_content_type(purpose, hints).await {
                log::warn!("[ibus] set_content_type error: {}", e);
            }
        });
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
    ready_tx: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
) {
    let ctx_guard = ctx.lock().await;

    let mut commit_stream = match ctx_guard.receive_commit_text().await {
        Ok(s) => s,
        Err(e) => {
            let err = anyhow::anyhow!("IBus subscribe commit-text failed: {e}");
            log::error!("[ibus] {}", err);
            drop(ctx_guard);
            send_signal_loop_ready(ready_tx, Err(err));
            return;
        }
    };

    let mut preedit_stream = match ctx_guard.receive_update_preedit_text().await {
        Ok(s) => s,
        Err(e) => {
            let err = anyhow::anyhow!("IBus subscribe update-preedit-text failed: {e}");
            log::error!("[ibus] {}", err);
            drop(ctx_guard);
            send_signal_loop_ready(ready_tx, Err(err));
            return;
        }
    };

    let mut hide_preedit_stream = match ctx_guard.receive_hide_preedit_text().await {
        Ok(s) => s,
        Err(e) => {
            let err = anyhow::anyhow!("IBus subscribe hide-preedit-text failed: {e}");
            log::error!("[ibus] {}", err);
            drop(ctx_guard);
            send_signal_loop_ready(ready_tx, Err(err));
            return;
        }
    };

    let mut delete_stream = match ctx_guard.receive_delete_surrounding_text().await {
        Ok(s) => s,
        Err(e) => {
            let err = anyhow::anyhow!("IBus subscribe delete-surrounding-text failed: {e}");
            log::error!("[ibus] {}", err);
            drop(ctx_guard);
            send_signal_loop_ready(ready_tx, Err(err));
            return;
        }
    };

    let mut forward_key_stream = match ctx_guard.receive_forward_key_event().await {
        Ok(s) => s,
        Err(e) => {
            let err = anyhow::anyhow!("IBus subscribe forward-key-event failed: {e}");
            log::error!("[ibus] {}", err);
            drop(ctx_guard);
            send_signal_loop_ready(ready_tx, Err(err));
            return;
        }
    };

    // 释放锁后再发送就绪信号，确保 process_key_event 能立刻拿到锁
    drop(ctx_guard);
    send_signal_loop_ready(ready_tx, Ok(()));

    loop {
        tokio::select! {
            Some(msg) = commit_stream.next() => {
                if let Ok(args) = msg.args() {
                    let text = extract_ibus_text_string(&args.text);
                    log::debug!("[ibus] {}", text_event_summary("commit", &text, None));
                    send_event(&event_tx, ImeEvent::Commit { text });
                }
            }
            Some(msg) = preedit_stream.next() => {
                if let Ok(args) = msg.args() {
                    let text = extract_ibus_text_string(&args.text);
                    let cursor = args.cursor_pos as i32;
                    log::debug!(
                        "[ibus] {}",
                        text_event_summary("preedit", &text, Some(cursor))
                    );
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
                    log::debug!("[ibus] {}", forward_key_summary(args.state));
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

fn build_ibus_text(text: &str) -> Value<'static> {
    let attachments = std::collections::HashMap::<String, Value<'static>>::new();
    let attrs = build_empty_ibus_attr_list();

    Value::new(("IBusText", attachments, text.to_owned(), attrs))
}

fn build_empty_ibus_attr_list() -> Value<'static> {
    let attachments = std::collections::HashMap::<String, Value<'static>>::new();
    let attributes = Array::new(&Signature::Structure(
        vec![
            Signature::U32,
            Signature::U32,
            Signature::I32,
            Signature::U32,
        ]
        .into(),
    ));

    Value::new(("IBusAttrList", attachments, Value::Array(attributes)))
}

fn ibus_content_type(content_type: ContentType) -> (u32, u32) {
    match content_type {
        ContentType::Normal => (IBUS_INPUT_PURPOSE_FREE_FORM, IBUS_INPUT_HINT_NONE),
        ContentType::Password => (IBUS_INPUT_PURPOSE_PASSWORD, IBUS_INPUT_HINT_NO_SPELLCHECK),
        ContentType::Number => (IBUS_INPUT_PURPOSE_NUMBER, IBUS_INPUT_HINT_NONE),
        ContentType::Phone => (IBUS_INPUT_PURPOSE_PHONE, IBUS_INPUT_HINT_NONE),
        ContentType::Url => (IBUS_INPUT_PURPOSE_URL, IBUS_INPUT_HINT_NO_SPELLCHECK),
        ContentType::Email => (IBUS_INPUT_PURPOSE_EMAIL, IBUS_INPUT_HINT_NO_SPELLCHECK),
    }
}

fn ibus_capabilities() -> ImeCapabilities {
    ImeCapabilities::PREEDIT
        | ImeCapabilities::COMMIT
        | ImeCapabilities::FORWARD_KEY
        | ImeCapabilities::DELETE_SURROUNDING_TEXT
        | ImeCapabilities::SURROUNDING_TEXT
        | ImeCapabilities::CONTENT_TYPE
}

fn text_event_summary(event_type: &str, text: &str, cursor: Option<i32>) -> String {
    let mut summary = format!(
        "{event_type} byte_len={} char_count={}",
        text.len(),
        text.chars().count()
    );

    if let Some(cursor) = cursor {
        summary.push_str(&format!(" cursor={cursor}"));
    }

    summary
}

fn forward_key_summary(state: u32) -> String {
    format!("forward-key state=0x{state:x}")
}

fn send_signal_loop_ready(
    ready_tx: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    result: anyhow::Result<()>,
) {
    let _ = ready_tx.send(result);
}

async fn wait_signal_loop_ready(
    ready_rx: tokio::sync::oneshot::Receiver<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    match ready_rx.await {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!(
            "IBus signal loop ended before initialization completed"
        )),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_ibus_text_matches_extract_ibus_text_string() {
        let value = build_ibus_text("hello 输入法");

        assert_eq!(extract_ibus_text_string(&value), "hello 输入法");
    }

    #[test]
    fn formats_text_event_without_content() {
        let summary = text_event_summary("commit", "密码a", None);

        assert_eq!(summary, "commit byte_len=7 char_count=3");
        assert!(!summary.contains("密码a"));
    }

    #[test]
    fn formats_preedit_event_with_cursor_without_content() {
        let summary = text_event_summary("preedit", "候选", Some(2));

        assert_eq!(summary, "preedit byte_len=6 char_count=2 cursor=2");
        assert!(!summary.contains("候选"));
    }

    #[test]
    fn formats_forward_key_without_keyval() {
        let summary = forward_key_summary(KeyState::SHIFT);

        assert_eq!(summary, "forward-key state=0x1");
        assert!(!summary.contains("ff0d"));
    }

    #[test]
    fn build_ibus_text_uses_ibus_serializable_shape() {
        let value = build_ibus_text("abc");
        let Value::Structure(text) = value else {
            panic!("IBusText should be encoded as a structure");
        };

        let fields = text.fields();
        assert_eq!(fields.len(), 4);
        assert!(matches!(&fields[0], Value::Str(name) if name.as_str() == "IBusText"));
        assert!(matches!(&fields[1], Value::Dict(_)));
        assert!(matches!(&fields[2], Value::Str(text) if text.as_str() == "abc"));

        let Value::Value(attrs) = &fields[3] else {
            panic!("IBusText attrs field should be a variant");
        };
        let Value::Structure(attrs) = attrs.as_ref() else {
            panic!("IBusText attrs variant should wrap IBusAttrList");
        };
        assert!(matches!(&attrs.fields()[0], Value::Str(name) if name.as_str() == "IBusAttrList"));
    }

    #[test]
    fn content_type_maps_to_ibus_purpose_and_hints() {
        assert_eq!(
            ibus_content_type(ContentType::Normal),
            (IBUS_INPUT_PURPOSE_FREE_FORM, IBUS_INPUT_HINT_NONE)
        );
        assert_eq!(
            ibus_content_type(ContentType::Password),
            (IBUS_INPUT_PURPOSE_PASSWORD, IBUS_INPUT_HINT_NO_SPELLCHECK)
        );
        assert_eq!(
            ibus_content_type(ContentType::Number),
            (IBUS_INPUT_PURPOSE_NUMBER, IBUS_INPUT_HINT_NONE)
        );
        assert_eq!(
            ibus_content_type(ContentType::Phone),
            (IBUS_INPUT_PURPOSE_PHONE, IBUS_INPUT_HINT_NONE)
        );
        assert_eq!(
            ibus_content_type(ContentType::Url),
            (IBUS_INPUT_PURPOSE_URL, IBUS_INPUT_HINT_NO_SPELLCHECK)
        );
        assert_eq!(
            ibus_content_type(ContentType::Email),
            (IBUS_INPUT_PURPOSE_EMAIL, IBUS_INPUT_HINT_NO_SPELLCHECK)
        );
    }

    #[test]
    fn reports_ibus_text_context_capabilities() {
        assert_eq!(
            ibus_capabilities(),
            ImeCapabilities::PREEDIT
                | ImeCapabilities::COMMIT
                | ImeCapabilities::FORWARD_KEY
                | ImeCapabilities::DELETE_SURROUNDING_TEXT
                | ImeCapabilities::SURROUNDING_TEXT
                | ImeCapabilities::CONTENT_TYPE
        );
    }

    #[tokio::test]
    async fn propagates_signal_loop_initialization_failure() {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        send_signal_loop_ready(
            ready_tx,
            Err(anyhow::anyhow!("subscribe forward-key failed")),
        );

        let err = wait_signal_loop_ready(ready_rx).await.unwrap_err();
        assert!(err.to_string().contains("subscribe forward-key failed"));
    }

    #[tokio::test]
    async fn reports_missing_signal_loop_initialization_result() {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
        drop(ready_tx);

        let err = wait_signal_loop_ready(ready_rx).await.unwrap_err();
        assert!(err
            .to_string()
            .contains("IBus signal loop ended before initialization completed"));
    }
}
