//! native-ime Linux PoC
//!
//! 用途：在 Linux 环境验证 IBus / Fcitx 4 / Fcitx5 连接，无需任何 GUI 框架。
//! 用法：
//!   RUST_LOG=debug cargo run -p ime-poc
//!   RUST_LOG=debug cargo run -p ime-poc -- --interactive
//!
//! 运行后：
//!   1. 默认自动发送一组测试按键（n i h a o + Return）
//!   2. interactive 模式从 stdin 读取普通文本行并逐字符发送
//!   3. 观察 backend diagnostics 与 process_key_event 输出
//!   4. 当输入法产生候选词时，观察 Preedit / Commit 事件

use std::io::{self, BufRead};

use ime_core::{BackendKind, ImeCapabilities, ImeEngine, ImeEvent};

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
    log::info!(
        "[POC] Backend diagnostics: name={} kind={} capabilities={}",
        backend_name,
        format_backend_kind(engine.backend_kind()),
        format_capabilities(engine.capabilities())
    );

    engine.focus_in();
    engine.set_cursor_rect(ime_core::CursorRect {
        x: 200,
        y: 300,
        width: 1,
        height: 20,
    });

    if std::env::args().any(|arg| arg == "--interactive") {
        log::info!(
            "[POC] Focus in, cursor rect set. Interactive mode: type text lines, Ctrl-D/Ctrl-Z to exit."
        );
        run_interactive(&engine).await;
    } else {
        log::info!("[POC] Focus in, cursor rect set. Sending test key sequence: nihao + Return");
        send_text_sequence(&engine, "nihao\n").await;
    }

    // 等待剩余异步事件
    log::info!("[POC] Waiting for remaining events (2s)...");
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    drain_events(&engine);

    engine.focus_out();
    log::info!("[POC] Done.");
}

async fn run_interactive(engine: &ImeEngine) {
    let stdin = io::stdin();

    for line in stdin.lock().lines() {
        match line {
            Ok(line) => {
                send_text_sequence(engine, &line).await;
                send_key(engine, 0xff0d, "Return").await;
            }
            Err(e) => {
                log::error!("[POC] Failed to read stdin: {}", e);
                break;
            }
        }
    }
}

async fn send_text_sequence(engine: &ImeEngine, text: &str) {
    for ch in text.chars() {
        match char_to_basic_x11_keysym(ch) {
            Some(keysym) => send_key(engine, keysym, &format_char_label(ch)).await,
            None => log::warn!(
                "[POC] Skipping unsupported non-ASCII char {:?}; PoC only maps basic X11 keysyms",
                ch
            ),
        }
    }
}

async fn send_key(engine: &ImeEngine, keysym: u32, label: &str) {
    let handled = engine.process_key_event(keysym, 0, 0, false);
    log::info!(
        "[POC] process_key_event '{}' (0x{:04x}) keydown: handled={}",
        label,
        keysym,
        handled
    );
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let handled = engine.process_key_event(keysym, 0, 0, true);
    log::info!(
        "[POC] process_key_event '{}' (0x{:04x}) keyup: handled={}",
        label,
        keysym,
        handled
    );
    drain_events(engine);

    tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;
    drain_events(engine);
}

fn char_to_basic_x11_keysym(ch: char) -> Option<u32> {
    match ch {
        '\n' | '\r' => Some(0xff0d),
        ch if ch.is_ascii() => Some(ch as u32),
        _ => None,
    }
}

fn format_char_label(ch: char) -> String {
    match ch {
        '\n' | '\r' => "Return".to_string(),
        ' ' => "Space".to_string(),
        _ => ch.to_string(),
    }
}

fn format_backend_kind(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Unknown => "unknown",
        BackendKind::Fcitx5 => "fcitx5",
        BackendKind::Fcitx4 => "fcitx4",
        BackendKind::IBus => "ibus",
    }
}

fn format_capabilities(caps: ImeCapabilities) -> String {
    let bits = caps.bits();
    let known = [
        (ImeCapabilities::PREEDIT, "preedit"),
        (ImeCapabilities::COMMIT, "commit"),
        (ImeCapabilities::FORWARD_KEY, "forward_key"),
        (
            ImeCapabilities::DELETE_SURROUNDING_TEXT,
            "delete_surrounding_text",
        ),
        (ImeCapabilities::SURROUNDING_TEXT, "surrounding_text"),
        (ImeCapabilities::CONTENT_TYPE, "content_type"),
    ];

    let mut names = Vec::new();
    let mut known_bits = 0;

    for (cap, name) in known {
        known_bits |= cap.bits();
        if bits & cap.bits() != 0 {
            names.push(name.to_string());
        }
    }

    let unknown_bits = bits & !known_bits;
    if unknown_bits != 0 {
        names.push(format!("unknown(0x{unknown_bits:08x})"));
    }

    if names.is_empty() {
        names.push("none".to_string());
    }

    format!("0x{bits:08x} [{}]", names.join(", "))
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

#[cfg(test)]
mod tests {
    use ime_core::ImeCapabilities;

    use super::*;

    #[test]
    fn maps_ascii_chars_to_x11_keysyms() {
        assert_eq!(char_to_basic_x11_keysym('a'), Some(0x0061));
        assert_eq!(char_to_basic_x11_keysym('Z'), Some(0x005a));
        assert_eq!(char_to_basic_x11_keysym('0'), Some(0x0030));
        assert_eq!(char_to_basic_x11_keysym(' '), Some(0x0020));
    }

    #[test]
    fn maps_line_breaks_to_return_keysym() {
        assert_eq!(char_to_basic_x11_keysym('\n'), Some(0xff0d));
        assert_eq!(char_to_basic_x11_keysym('\r'), Some(0xff0d));
    }

    #[test]
    fn rejects_non_ascii_chars_without_guessing_keyboard_layout() {
        assert_eq!(char_to_basic_x11_keysym('你'), None);
    }

    #[test]
    fn formats_capability_bits_as_human_readable_names() {
        let caps =
            ImeCapabilities::PREEDIT | ImeCapabilities::COMMIT | ImeCapabilities::FORWARD_KEY;

        assert_eq!(
            format_capabilities(caps),
            "0x00000007 [preedit, commit, forward_key]"
        );
    }

    #[test]
    fn formats_empty_capabilities_explicitly() {
        assert_eq!(
            format_capabilities(ImeCapabilities::NONE),
            "0x00000000 [none]"
        );
    }
}
