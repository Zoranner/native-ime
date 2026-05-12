//! Fcitx 4 D-Bus backend.
//!
//! This supports legacy Fcitx 4 deployments such as Kylin V10 with Sogou IME.

mod proxy;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use crossbeam_channel::Sender;
use futures_util::StreamExt;
use ime_core::{BackendKind, CursorRect, ImeBackend, ImeCapabilities, ImeEvent, KeyState};
use proxy::{Fcitx4InputContextProxy, Fcitx4InputMethodProxy};
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use zbus::zvariant::OwnedObjectPath;

const FCITX_KEY_PRESS: i32 = 0;
const FCITX_KEY_RELEASE: i32 = 1;

const CAPACITY_PREEDIT: u32 = 1 << 1;
const CAPACITY_FORMATTED_PREEDIT: u32 = 1 << 4;

pub struct Fcitx4Backend {
    ctx: Arc<Mutex<Fcitx4InputContextProxy<'static>>>,
    rt_handle: Handle,
    signal_loop_handle: tokio::task::JoinHandle<()>,
}

impl Drop for Fcitx4Backend {
    fn drop(&mut self) {
        self.signal_loop_handle.abort();
    }
}

impl Fcitx4Backend {
    pub async fn connect(event_tx: Sender<ImeEvent>) -> anyhow::Result<Self> {
        let conn = zbus::Connection::session().await?;

        let im = Fcitx4InputMethodProxy::builder(&conn)
            .build()
            .await
            .context("Fcitx 4 not available on D-Bus")?;

        let pid = std::process::id().min(i32::MAX as u32) as i32;
        let (icid, enabled, trigger_key_1, trigger_state_1, trigger_key_2, trigger_state_2) = im
            .create_ic_v3("native-ime", pid)
            .await
            .context("Fcitx 4 CreateICv3 failed")?;
        if icid < 0 {
            bail!("Fcitx 4 returned invalid input context id: {icid}");
        }

        log::debug!(
            "[fcitx4] InputContext id={} enabled={} trigger1=0x{:x}/0x{:x} trigger2=0x{:x}/0x{:x}",
            icid,
            enabled,
            trigger_key_1,
            trigger_state_1,
            trigger_key_2,
            trigger_state_2
        );

        let ctx_path = OwnedObjectPath::try_from(input_context_path(icid))
            .context("Fcitx 4 returned invalid input context path")?;
        let ctx = Fcitx4InputContextProxy::builder(&conn)
            .path(ctx_path)?
            .destination("org.fcitx.Fcitx")?
            .build()
            .await?;

        ctx.set_capacity(CAPACITY_PREEDIT | CAPACITY_FORMATTED_PREEDIT)
            .await
            .context("Fcitx 4 SetCapacity failed")?;

        let ctx = Arc::new(Mutex::new(ctx));
        let rt_handle = Handle::current();

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

impl ImeBackend for Fcitx4Backend {
    fn backend_kind(&self) -> BackendKind {
        BackendKind::Fcitx4
    }

    fn capabilities(&self) -> ImeCapabilities {
        fcitx4_capabilities()
    }

    fn focus_in(&self) {
        let ctx = self.ctx.clone();
        self.rt_handle.spawn(async move {
            let ctx = ctx.lock().await;
            if let Err(e) = ctx.focus_in().await {
                log::warn!("[fcitx4] focus_in error: {}", e);
            }
        });
    }

    fn focus_out(&self) {
        let ctx = self.ctx.clone();
        self.rt_handle.spawn(async move {
            let ctx = ctx.lock().await;
            if let Err(e) = ctx.focus_out().await {
                log::warn!("[fcitx4] focus_out error: {}", e);
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
                log::warn!("[fcitx4] set_cursor_rect error: {}", e);
            }
        });
    }

    fn set_surrounding_text(&self, text: &str, cursor: i32, anchor: i32) {
        let ctx = self.ctx.clone();
        let (text, cursor, anchor) = prepare_surrounding_text(text, cursor, anchor);
        self.rt_handle.spawn(async move {
            let ctx = ctx.lock().await;
            if let Err(e) = ctx.set_surrounding_text(&text, cursor, anchor).await {
                log::warn!("[fcitx4] set_surrounding_text error: {}", e);
            }
        });
    }

    fn process_key_event(&self, keyval: u32, keycode: u32, state: u32, is_release: bool) -> bool {
        let (resp_tx, resp_rx) = std::sync::mpsc::sync_channel::<bool>(1);
        let ctx = self.ctx.clone();
        let event_type = key_event_type(is_release);

        self.rt_handle.spawn(async move {
            let ctx = ctx.lock().await;
            let result = ctx
                .process_key_event(keyval, keycode, state, event_type, 0)
                .await
                .map(|handled| handled != 0)
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
                log::warn!("[fcitx4] reset error: {}", e);
            }
        });
    }
}

async fn signal_loop(
    ctx: Arc<Mutex<Fcitx4InputContextProxy<'static>>>,
    event_tx: Sender<ImeEvent>,
    ready_tx: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
) {
    let ctx_guard = ctx.lock().await;

    let mut commit_stream = match ctx_guard.receive_commit_string().await {
        Ok(s) => s,
        Err(e) => {
            let err = anyhow::anyhow!("Fcitx 4 subscribe commit-string failed: {e}");
            log::error!("[fcitx4] {}", err);
            drop(ctx_guard);
            send_signal_loop_ready(ready_tx, Err(err));
            return;
        }
    };

    let mut preedit_stream = match ctx_guard.receive_update_formatted_preedit().await {
        Ok(s) => s,
        Err(e) => {
            let err = anyhow::anyhow!("Fcitx 4 subscribe update-formatted-preedit failed: {e}");
            log::error!("[fcitx4] {}", err);
            drop(ctx_guard);
            send_signal_loop_ready(ready_tx, Err(err));
            return;
        }
    };

    let mut forward_key_stream = match ctx_guard.receive_forward_key().await {
        Ok(s) => s,
        Err(e) => {
            let err = anyhow::anyhow!("Fcitx 4 subscribe forward-key failed: {e}");
            log::error!("[fcitx4] {}", err);
            drop(ctx_guard);
            send_signal_loop_ready(ready_tx, Err(err));
            return;
        }
    };

    let mut delete_stream = match ctx_guard.receive_delete_surrounding_text().await {
        Ok(s) => s,
        Err(e) => {
            let err = anyhow::anyhow!("Fcitx 4 subscribe delete-surrounding-text failed: {e}");
            log::error!("[fcitx4] {}", err);
            drop(ctx_guard);
            send_signal_loop_ready(ready_tx, Err(err));
            return;
        }
    };

    drop(ctx_guard);
    send_signal_loop_ready(ready_tx, Ok(()));

    loop {
        tokio::select! {
            Some(msg) = commit_stream.next() => {
                if let Ok(args) = msg.args() {
                    let text = args.text.to_string();
                    log::debug!("[fcitx4] {}", text_event_summary("commit", &text, None));
                    send_event(&event_tx, ImeEvent::Commit { text });
                }
            }
            Some(msg) = preedit_stream.next() => {
                if let Ok(args) = msg.args() {
                    let text = formatted_preedit_text(&args.segments);
                    let cursor = args.cursor_pos;
                    log::debug!(
                        "[fcitx4] {}",
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
            Some(msg) = forward_key_stream.next() => {
                if let Ok(args) = msg.args() {
                    log::debug!(
                        "[fcitx4] {}",
                        forward_key_summary(args.state, args.event_type)
                    );
                    send_event(
                        &event_tx,
                        forward_key_event(args.keyval, args.state, args.event_type),
                    );
                }
            }
            Some(msg) = delete_stream.next() => {
                if let Ok(args) = msg.args() {
                    send_event(
                        &event_tx,
                        delete_surrounding_event(args.offset, args.nchar),
                    );
                }
            }
            else => break,
        }
    }
}

fn input_context_path(icid: i32) -> String {
    format!("/inputcontext_{icid}")
}

fn key_event_type(is_release: bool) -> i32 {
    if is_release {
        FCITX_KEY_RELEASE
    } else {
        FCITX_KEY_PRESS
    }
}

fn formatted_preedit_text(segments: &[(String, i32)]) -> String {
    segments
        .iter()
        .map(|(text, _format)| text.as_str())
        .collect()
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

fn forward_key_summary(state: u32, event_type: i32) -> String {
    format!("forward-key state=0x{state:x} type={event_type}")
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
        Err(_) => bail!("Fcitx 4 signal loop ended before initialization completed"),
    }
}

fn forward_key_event(keyval: u32, state: u32, event_type: i32) -> ImeEvent {
    let mut state = state;
    if event_type == FCITX_KEY_RELEASE {
        state |= KeyState::RELEASE;
    }
    ImeEvent::ForwardKey {
        keyval,
        state: KeyState(state),
    }
}

fn delete_surrounding_event(offset: i32, nchar: u32) -> ImeEvent {
    let before = (-offset).max(0) as u32;
    let after = (offset + nchar as i32).max(0) as u32;
    ImeEvent::DeleteSurroundingText { before, after }
}

fn prepare_surrounding_text(text: &str, cursor: i32, anchor: i32) -> (String, u32, u32) {
    (
        text.to_owned(),
        saturating_position(cursor),
        saturating_position(anchor),
    )
}

fn saturating_position(position: i32) -> u32 {
    position.max(0) as u32
}

fn fcitx4_capabilities() -> ImeCapabilities {
    ImeCapabilities::SURROUNDING_TEXT
        | ImeCapabilities::DELETE_SURROUNDING_TEXT
        | ImeCapabilities::PREEDIT
        | ImeCapabilities::COMMIT
        | ImeCapabilities::FORWARD_KEY
}

fn send_event(tx: &Sender<ImeEvent>, event: ImeEvent) {
    if let Err(e) = tx.try_send(event) {
        log::warn!("[fcitx4] event queue full, dropping event: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_input_context_path_from_icid() {
        assert_eq!(input_context_path(35), "/inputcontext_35");
    }

    #[test]
    fn maps_key_release_to_fcitx_event_type() {
        assert_eq!(key_event_type(false), 0);
        assert_eq!(key_event_type(true), 1);
    }

    #[test]
    fn joins_formatted_preedit_segments() {
        let segments = vec![("ni".to_owned(), 0), ("hao".to_owned(), 1)];
        assert_eq!(formatted_preedit_text(&segments), "nihao");
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
        let summary = forward_key_summary(KeyState::SHIFT, 1);

        assert_eq!(summary, "forward-key state=0x1 type=1");
        assert!(!summary.contains("ff0d"));
    }

    #[test]
    fn marks_forward_key_release_in_key_state() {
        let event = forward_key_event(0xff0d, KeyState::SHIFT, 1);
        match event {
            ImeEvent::ForwardKey { keyval, state } => {
                assert_eq!(keyval, 0xff0d);
                assert_eq!(state.0, KeyState::SHIFT | KeyState::RELEASE);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn converts_delete_surrounding_text_to_before_after_counts() {
        let event = delete_surrounding_event(-2, 3);
        match event {
            ImeEvent::DeleteSurroundingText { before, after } => {
                assert_eq!(before, 2);
                assert_eq!(after, 1);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn prepares_surrounding_text_with_saturating_cursor_and_anchor() {
        assert_eq!(
            prepare_surrounding_text("hello", -3, 7),
            ("hello".to_owned(), 0, 7)
        );
        assert_eq!(
            prepare_surrounding_text("hello", i32::MAX, i32::MIN),
            ("hello".to_owned(), i32::MAX as u32, 0)
        );
    }

    #[test]
    fn reports_fcitx4_surrounding_text_capabilities_without_content_type() {
        let caps = fcitx4_capabilities();
        let expected = ImeCapabilities::SURROUNDING_TEXT
            | ImeCapabilities::DELETE_SURROUNDING_TEXT
            | ImeCapabilities::PREEDIT
            | ImeCapabilities::COMMIT
            | ImeCapabilities::FORWARD_KEY;

        assert_eq!(caps, expected);
        assert_eq!(caps.bits() & ImeCapabilities::CONTENT_TYPE.bits(), 0);
    }

    #[tokio::test]
    async fn propagates_signal_loop_initialization_failure() {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

        send_signal_loop_ready(ready_tx, Err(anyhow::anyhow!("subscribe commit failed")));

        let err = wait_signal_loop_ready(ready_rx).await.unwrap_err();
        assert!(err.to_string().contains("subscribe commit failed"));
    }

    #[tokio::test]
    async fn reports_missing_signal_loop_initialization_result() {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
        drop(ready_tx);

        let err = wait_signal_loop_ready(ready_rx).await.unwrap_err();
        assert!(err
            .to_string()
            .contains("Fcitx 4 signal loop ended before initialization completed"));
    }
}
