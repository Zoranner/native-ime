//! native-ime Linux PoC
//!
//! 用途：在 Linux 环境验证 IBus / Fcitx 4 / Fcitx5 连接，无需任何 GUI 框架。
//! 用法：
//!   RUST_LOG=debug cargo run -p ime-poc
//!   RUST_LOG=debug cargo run -p ime-poc -- --interactive
//!   RUST_LOG=debug cargo run -p ime-poc -- --surrounding-text "hello" --cursor 5
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

    let args = match PocArgs::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            log::error!("[POC] {}", e);
            return;
        }
    };

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
    maybe_set_surrounding_text(&engine, args.surrounding_text.as_ref());

    if args.interactive {
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct SurroundingTextArgs {
    text: String,
    cursor: i32,
    anchor: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct PocArgs {
    interactive: bool,
    surrounding_text: Option<SurroundingTextArgs>,
}

impl PocArgs {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut parsed = Self::default();
        let mut surrounding_text: Option<String> = None;
        let mut cursor: Option<i32> = None;
        let mut anchor: Option<i32> = None;
        let mut iter = args.into_iter();

        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--interactive" => parsed.interactive = true,
                "--surrounding-text" => {
                    let text = next_arg(&mut iter, "--surrounding-text")?;
                    surrounding_text = Some(text);
                }
                "--cursor" => {
                    let value = next_arg(&mut iter, "--cursor")?;
                    cursor = Some(parse_non_negative_i32("--cursor", &value)?);
                }
                "--anchor" => {
                    let value = next_arg(&mut iter, "--anchor")?;
                    anchor = Some(parse_non_negative_i32("--anchor", &value)?);
                }
                "--help" | "-h" => return Err(usage()),
                _ => return Err(format!("Unknown argument: {arg}\n{}", usage())),
            }
        }

        if surrounding_text.is_none() && (cursor.is_some() || anchor.is_some()) {
            return Err("--cursor/--anchor require --surrounding-text".to_string());
        }

        parsed.surrounding_text = surrounding_text.map(|text| {
            let default_cursor = i32::try_from(text.len()).unwrap_or(i32::MAX);
            let cursor = cursor.unwrap_or(default_cursor);
            let anchor = anchor.unwrap_or(cursor);

            SurroundingTextArgs {
                text,
                cursor,
                anchor,
            }
        });

        Ok(parsed)
    }
}

fn next_arg(iter: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    iter.next()
        .ok_or_else(|| format!("{flag} requires a value\n{}", usage()))
}

fn parse_non_negative_i32(flag: &str, value: &str) -> Result<i32, String> {
    let parsed = value
        .parse::<i32>()
        .map_err(|_| format!("{flag} must be a non-negative i32 byte offset: {value}"))?;

    if parsed < 0 {
        return Err(format!(
            "{flag} must be a non-negative i32 byte offset: {value}"
        ));
    }

    Ok(parsed)
}

fn usage() -> String {
    "Usage: ime-poc [--interactive] [--surrounding-text <text> [--cursor <n>] [--anchor <n>]]"
        .to_string()
}

fn maybe_set_surrounding_text(engine: &ImeEngine, args: Option<&SurroundingTextArgs>) {
    let Some(args) = args else {
        return;
    };

    log::info!(
        "[POC] Requested surrounding text: byte_len={} cursor={} anchor={}",
        args.text.len(),
        args.cursor,
        args.anchor
    );

    let caps = engine.capabilities();
    if caps.bits() & ImeCapabilities::SURROUNDING_TEXT.bits() == 0 {
        log::warn!(
            "[POC] Backend capability missing: surrounding_text. Skipping set_surrounding_text."
        );
        return;
    }

    engine.set_surrounding_text(&args.text, args.cursor, args.anchor);
    log::info!("[POC] set_surrounding_text called.");
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
    fn parses_surrounding_text_with_default_offsets() {
        let args = PocArgs::parse(["--surrounding-text", "你a"].map(String::from)).unwrap();

        assert_eq!(
            args.surrounding_text,
            Some(SurroundingTextArgs {
                text: "你a".to_string(),
                cursor: 4,
                anchor: 4,
            })
        );
    }

    #[test]
    fn parses_explicit_surrounding_text_offsets() {
        let args = PocArgs::parse(
            [
                "--interactive",
                "--surrounding-text",
                "hello",
                "--cursor",
                "2",
                "--anchor",
                "1",
            ]
            .map(String::from),
        )
        .unwrap();

        assert!(args.interactive);
        assert_eq!(
            args.surrounding_text,
            Some(SurroundingTextArgs {
                text: "hello".to_string(),
                cursor: 2,
                anchor: 1,
            })
        );
    }

    #[test]
    fn rejects_offsets_without_surrounding_text() {
        let err = PocArgs::parse(["--cursor", "1"].map(String::from)).unwrap_err();

        assert_eq!(err, "--cursor/--anchor require --surrounding-text");
    }

    #[test]
    fn rejects_negative_offsets() {
        let err =
            PocArgs::parse(["--surrounding-text", "hello", "--cursor", "-1"].map(String::from))
                .unwrap_err();

        assert!(err.contains("--cursor must be a non-negative i32 byte offset"));
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
