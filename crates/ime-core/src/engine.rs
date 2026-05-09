//! ImeEngine：连接宿主 ↔ backend 的中间层
//!
//! 负责：
//! - 持有 backend 实例
//! - 接受外部传入的事件接收端（发送端由 backend 持有，写入由信号循环驱动）
//! - 提供线程安全的调用入口（C ABI 层直接调用）
//!
//! channel 生命周期：
//!   调用方 create (event_tx, event_rx) → event_tx 传给 backend.connect() → backend 信号循环通过
//!   event_tx 推送事件 → ImeEngine 通过 event_rx.poll_event() 取出 → 宿主消费

use crossbeam_channel::{Receiver, TryRecvError};

use crate::backend::ImeBackend;
use crate::types::{BackendKind, ContentType, CursorRect, ImeCapabilities, ImeEvent};

pub struct ImeEngine {
    backend: Box<dyn ImeBackend>,
    event_rx: Receiver<ImeEvent>,
}

impl ImeEngine {
    /// 创建 engine。
    ///
    /// `event_rx` 必须与传给 backend 的 `event_tx` 是同一 channel 对，
    /// 否则 `poll_event` 永远取不到事件。
    pub fn new(backend: Box<dyn ImeBackend>, event_rx: Receiver<ImeEvent>) -> Self {
        Self { backend, event_rx }
    }

    // --- 宿主调用接口 ---

    pub fn backend_kind(&self) -> BackendKind {
        self.backend.backend_kind()
    }

    pub fn capabilities(&self) -> ImeCapabilities {
        self.backend.capabilities()
    }

    pub fn focus_in(&self) {
        self.backend.focus_in();
    }

    pub fn focus_out(&self) {
        self.backend.focus_out();
    }

    pub fn set_cursor_rect(&self, rect: CursorRect) {
        self.backend.set_cursor_rect(rect);
    }

    pub fn set_surrounding_text(&self, text: &str, cursor: i32, anchor: i32) {
        self.backend.set_surrounding_text(text, cursor, anchor);
    }

    pub fn set_content_type(&self, content_type: ContentType) {
        self.backend.set_content_type(content_type);
    }

    pub fn process_key_event(
        &self,
        keyval: u32,
        keycode: u32,
        state: u32,
        is_release: bool,
    ) -> bool {
        self.backend
            .process_key_event(keyval, keycode, state, is_release)
    }

    pub fn reset(&self) {
        self.backend.reset();
    }

    /// 取出下一个待处理事件，无事件返回 None
    pub fn poll_event(&self) -> Option<ImeEvent> {
        match self.event_rx.try_recv() {
            Ok(ev) => Some(ev),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crossbeam_channel::{bounded, Sender};

    use super::*;
    use crate::types::{BackendKind, ImeCapabilities, KeyState};

    struct RecordingBackend {
        tx: Sender<ImeEvent>,
    }

    impl ImeBackend for RecordingBackend {
        fn backend_kind(&self) -> BackendKind {
            BackendKind::Fcitx5
        }

        fn capabilities(&self) -> ImeCapabilities {
            ImeCapabilities::PREEDIT | ImeCapabilities::COMMIT | ImeCapabilities::FORWARD_KEY
        }

        fn focus_in(&self) {}

        fn focus_out(&self) {}

        fn set_cursor_rect(&self, _rect: CursorRect) {}

        fn set_surrounding_text(&self, _text: &str, _cursor: i32, _anchor: i32) {}

        fn process_key_event(
            &self,
            keyval: u32,
            _keycode: u32,
            state: u32,
            _is_release: bool,
        ) -> bool {
            if keyval == 0x0061 {
                self.tx
                    .try_send(ImeEvent::ForwardKey {
                        keyval,
                        state: KeyState(state),
                    })
                    .is_ok()
            } else {
                false
            }
        }

        fn reset(&self) {}
    }

    #[test]
    fn poll_event_returns_none_when_queue_is_empty() {
        let (tx, rx) = bounded(1);
        let engine = ImeEngine::new(Box::new(RecordingBackend { tx }), rx);

        assert!(engine.poll_event().is_none());
    }

    #[test]
    fn poll_event_returns_none_when_queue_is_disconnected() {
        let (tx, rx) = bounded(1);
        drop(tx);

        let (backend_tx, _backend_rx) = bounded(1);
        let engine = ImeEngine::new(Box::new(RecordingBackend { tx: backend_tx }), rx);

        assert!(engine.poll_event().is_none());
    }

    #[test]
    fn process_key_event_can_enqueue_event_for_host_polling() {
        let (tx, rx) = bounded(1);
        let engine = ImeEngine::new(Box::new(RecordingBackend { tx }), rx);

        assert!(engine.process_key_event(0x0061, 0, KeyState::SHIFT, false));

        match engine.poll_event() {
            Some(ImeEvent::ForwardKey { keyval, state }) => {
                assert_eq!(keyval, 0x0061);
                assert_eq!(state.0, KeyState::SHIFT);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn backend_diagnostics_are_forwarded_from_backend() {
        let (tx, rx) = bounded(1);
        let engine = ImeEngine::new(Box::new(RecordingBackend { tx }), rx);

        assert_eq!(engine.backend_kind(), BackendKind::Fcitx5);
        assert_eq!(
            engine.capabilities().bits(),
            (ImeCapabilities::PREEDIT | ImeCapabilities::COMMIT | ImeCapabilities::FORWARD_KEY)
                .bits()
        );
    }
}
