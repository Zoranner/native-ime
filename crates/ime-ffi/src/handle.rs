//! ImeHandle：C ABI 层的不透明句柄

use crossbeam_channel::Sender;
use ime_core::{
    BackendKind, ContentType, CursorRect, ImeBackend, ImeCapabilities, ImeEngine, ImeEvent,
};

pub struct ImeHandle {
    engine: ImeEngine,
    /// tokio 运行时，drop 时自动 shutdown（等待所有任务结束）
    _runtime: tokio::runtime::Runtime,
}

impl ImeHandle {
    /// 自动检测输入法框架并创建句柄
    ///
    /// 检测顺序：Fcitx5 → Fcitx 4 → IBus → 失败返回 None
    pub fn create() -> Option<Self> {
        // 2 个 worker 线程：一个供信号循环 + 普通 async 任务，另一个供 process_key_event
        // 内部的 spawn + sync_channel recv 不阻塞 tokio 调度器
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("native-ime")
            .enable_all()
            .build()
            .ok()?;

        // channel 在此处创建：
        //   event_tx → 传给 backend.connect()，由信号循环写入
        //   event_rx → 传给 ImeEngine，由宿主通过 poll_event() 读取
        let (event_tx, event_rx) = crossbeam_channel::bounded::<ImeEvent>(64);

        let maybe_backend = runtime.block_on(async {
            if let Some(backend) = try_fcitx5(event_tx.clone()).await {
                log::info!("[native-ime] Using Fcitx5 backend");
                return Some(backend);
            }
            if let Some(backend) = try_fcitx4(event_tx.clone()).await {
                log::info!("[native-ime] Using Fcitx 4 backend");
                return Some(backend);
            }
            if let Some(backend) = try_ibus(event_tx.clone()).await {
                log::info!("[native-ime] Using IBus backend");
                return Some(backend);
            }
            log::warn!("[native-ime] No IME framework detected (Fcitx5 / Fcitx 4 / IBus)");
            None
        });

        let backend = maybe_backend?;
        let engine = ImeEngine::new(backend, event_rx);

        Some(Self {
            engine,
            _runtime: runtime,
        })
    }

    pub fn focus_in(&self) {
        self.engine.focus_in();
    }

    pub fn backend_kind(&self) -> BackendKind {
        self.engine.backend_kind()
    }

    pub fn capabilities(&self) -> ImeCapabilities {
        self.engine.capabilities()
    }

    pub fn focus_out(&self) {
        self.engine.focus_out();
    }

    pub fn set_cursor_rect(&self, rect: CursorRect) {
        self.engine.set_cursor_rect(rect);
    }

    pub fn set_surrounding_text(&self, text: &str, cursor: i32, anchor: i32) {
        self.engine.set_surrounding_text(text, cursor, anchor);
    }

    pub fn set_content_type(&self, content_type: ContentType) {
        self.engine.set_content_type(content_type);
    }

    pub fn reset(&self) {
        self.engine.reset();
    }

    pub fn process_key_event(
        &self,
        keyval: u32,
        keycode: u32,
        state: u32,
        is_release: bool,
    ) -> bool {
        self.engine
            .process_key_event(keyval, keycode, state, is_release)
    }

    pub fn poll_event(&self) -> Option<ime_core::ImeEvent> {
        self.engine.poll_event()
    }
}

async fn try_fcitx5(event_tx: Sender<ImeEvent>) -> Option<Box<dyn ImeBackend>> {
    match ime_fcitx5::Fcitx5Backend::connect(event_tx).await {
        Ok(backend) => Some(Box::new(backend)),
        Err(e) => {
            log::debug!("[native-ime] Fcitx5 not available: {}", e);
            None
        }
    }
}

async fn try_fcitx4(event_tx: Sender<ImeEvent>) -> Option<Box<dyn ImeBackend>> {
    match ime_fcitx4::Fcitx4Backend::connect(event_tx).await {
        Ok(backend) => Some(Box::new(backend)),
        Err(e) => {
            log::debug!("[native-ime] Fcitx 4 not available: {}", e);
            None
        }
    }
}

async fn try_ibus(event_tx: Sender<ImeEvent>) -> Option<Box<dyn ImeBackend>> {
    match ime_ibus::IBusBackend::connect(event_tx).await {
        Ok(backend) => Some(Box::new(backend)),
        Err(e) => {
            log::debug!("[native-ime] IBus not available: {}", e);
            None
        }
    }
}
