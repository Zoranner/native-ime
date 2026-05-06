//! native-ime Linux PoC
//!
//! 用途：在 Linux 环境验证 IBus / Fcitx 4 / Fcitx5 连接，无需任何 GUI 框架。
//! 用法：
//!   RUST_LOG=debug cargo run -p ime-poc
//!
//! 运行后：
//!   1. 程序自动发送一组测试按键（n i h a o + Return）
//!   2. 观察 [POC] process_key_event: handled=true/false 输出
//!   3. 当输入法产生候选词时，观察 Preedit / Commit 事件

use ime_core::{ImeEngine, ImeEvent};

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();

    log::info!("[POC] native-ime Linux PoC starting...");

    let (engine, backend_name) = match try_fcitx5().await {
        Some(r) => r,
        None => match try_fcitx4().await {
            Some(r) => r,
            None => match try_ibus().await {
                Some(r) => r,
                None => {
                    log::error!(
                        "[POC] No IME framework available (Fcitx5 / Fcitx 4 / IBus). Exiting."
                    );
                    return;
                }
            },
        },
    };

    log::info!("[POC] Connected to {}", backend_name);

    engine.focus_in();
    engine.set_cursor_rect(ime_core::CursorRect {
        x: 200,
        y: 300,
        width: 1,
        height: 20,
    });

    log::info!("[POC] Focus in, cursor rect set. Sending test key sequence: nihao + Return");

    // 发送 n i h a o（X11 keysym = ASCII 小写字母值）
    let test_keys: &[(u32, &str)] = &[
        (0x006e, "n"),
        (0x0069, "i"),
        (0x0068, "h"),
        (0x0061, "a"),
        (0x006f, "o"),
    ];

    for (keysym, label) in test_keys {
        let handled = engine.process_key_event(*keysym, 0, 0, false);
        log::info!(
            "[POC] process_key_event '{}' (0x{:04x}) keydown: handled={}",
            label,
            keysym,
            handled
        );
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        engine.process_key_event(*keysym, 0, 0, true);
        drain_events(&engine);

        tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;
    }

    // Return 确认选词（keysym 0xff0d）
    log::info!("[POC] Sending Return to confirm...");
    engine.process_key_event(0xff0d, 0, 0, false);
    tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;
    drain_events(&engine);

    // 等待剩余异步事件
    log::info!("[POC] Waiting for remaining events (2s)...");
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    drain_events(&engine);

    engine.focus_out();
    log::info!("[POC] Done.");
}

fn drain_events(engine: &ImeEngine) {
    while let Some(event) = engine.poll_event() {
        match &event {
            ImeEvent::Preedit {
                text,
                cursor_begin,
                cursor_end,
            } => {
                log::info!(
                    "[POC] EVENT Preedit text={:?} cursor=[{}..{}]",
                    text,
                    cursor_begin,
                    cursor_end
                );
            }
            ImeEvent::PreeditEnd => {
                log::info!("[POC] EVENT PreeditEnd");
            }
            ImeEvent::Commit { text } => {
                log::info!("[POC] EVENT Commit text={:?}", text);
            }
            ImeEvent::DeleteSurroundingText { before, after } => {
                log::info!(
                    "[POC] EVENT DeleteSurroundingText before={} after={}",
                    before,
                    after
                );
            }
            ImeEvent::ForwardKey { keyval, state } => {
                log::info!(
                    "[POC] EVENT ForwardKey keyval=0x{:x} state=0x{:x}",
                    keyval,
                    state.0
                );
            }
        }
    }
}

async fn try_fcitx5() -> Option<(ImeEngine, &'static str)> {
    let (event_tx, event_rx) = crossbeam_channel::bounded(64);
    match ime_fcitx5::Fcitx5Backend::connect(event_tx).await {
        Ok(backend) => {
            let engine = ImeEngine::new(Box::new(backend), event_rx);
            Some((engine, "Fcitx5"))
        }
        Err(e) => {
            log::debug!("[POC] Fcitx5 not available: {}", e);
            None
        }
    }
}

async fn try_fcitx4() -> Option<(ImeEngine, &'static str)> {
    let (event_tx, event_rx) = crossbeam_channel::bounded(64);
    match ime_fcitx4::Fcitx4Backend::connect(event_tx).await {
        Ok(backend) => {
            let engine = ImeEngine::new(Box::new(backend), event_rx);
            Some((engine, "Fcitx 4"))
        }
        Err(e) => {
            log::debug!("[POC] Fcitx 4 not available: {}", e);
            None
        }
    }
}

async fn try_ibus() -> Option<(ImeEngine, &'static str)> {
    let (event_tx, event_rx) = crossbeam_channel::bounded(64);
    match ime_ibus::IBusBackend::connect(event_tx).await {
        Ok(backend) => {
            let engine = ImeEngine::new(Box::new(backend), event_rx);
            Some((engine, "IBus"))
        }
        Err(e) => {
            log::debug!("[POC] IBus not available: {}", e);
            None
        }
    }
}
