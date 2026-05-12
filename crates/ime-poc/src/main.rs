//! native-ime Linux PoC
//!
//! 用途：在 Linux 环境验证 IBus / Fcitx 4 / Fcitx5 连接，无需任何 GUI 框架。
//! 用法：
//!   RUST_LOG=debug cargo run -p ime-poc
//!   RUST_LOG=debug cargo run -p ime-poc -- --interactive
//!   RUST_LOG=debug cargo run -p ime-poc -- --surrounding-text "hello" --cursor 5
//!   RUST_LOG=debug cargo run -p ime-poc -- --log-text --interactive
//!
//! 运行后：
//!   1. 默认自动发送一组测试按键（n i h a o + Return）
//!   2. interactive 模式从 stdin 读取普通文本行并逐字符发送
//!   3. 观察 backend diagnostics 与 process_key_event 输出；默认日志不打印真实文本
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
    maybe_set_surrounding_text(&engine, args.surrounding_text.as_ref(), args.log_text);

    if args.interactive {
        log::info!(
            "[POC] Focus in, cursor rect set. Interactive mode: type text lines, Ctrl-D/Ctrl-Z to exit."
        );
        run_interactive(&engine, args.log_text).await;
    } else {
        log::info!("[POC] Focus in, cursor rect set. Sending default test key sequence.");
        send_text_sequence(&engine, "nihao\n", args.log_text).await;
    }

    // 等待剩余异步事件
    log::info!("[POC] Waiting for remaining events (2s)...");
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    drain_events(&engine, args.log_text);

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
    log_text: bool,
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
                "--log-text" => parsed.log_text = true,
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
    "Usage: ime-poc [--interactive] [--log-text] [--surrounding-text <text> [--cursor <n>] [--anchor <n>]]"
        .to_string()
}

fn maybe_set_surrounding_text(
    engine: &ImeEngine,
    args: Option<&SurroundingTextArgs>,
    log_text: bool,
) {
    let Some(args) = args else {
        return;
    };

    log::info!("[POC] {}", format_surrounding_text_request(args, log_text));

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

async fn run_interactive(engine: &ImeEngine, log_text: bool) {
    let stdin = io::stdin();

    for line in stdin.lock().lines() {
        match line {
            Ok(line) => {
                send_text_sequence(engine, &line, log_text).await;
                send_key(engine, 0xff0d, "Return", log_text).await;
            }
            Err(e) => {
                log::error!("[POC] Failed to read stdin: {}", e);
                break;
            }
        }
    }
}

async fn send_text_sequence(engine: &ImeEngine, text: &str, log_text: bool) {
    for ch in text.chars() {
        match char_to_basic_x11_keysym(ch) {
            Some(keysym) => {
                send_key(engine, keysym, &format_char_label(ch, log_text), log_text).await
            }
            None => log_unsupported_char(ch, log_text),
        }
    }
}

async fn send_key(engine: &ImeEngine, keysym: u32, label: &str, log_text: bool) {
    let handled = engine.process_key_event(keysym, 0, 0, false);
    log::info!(
        "[POC] process_key_event {} keydown: handled={}",
        format_key_event(label, keysym, log_text),
        handled
    );
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let handled = engine.process_key_event(keysym, 0, 0, true);
    log::info!(
        "[POC] process_key_event {} keyup: handled={}",
        format_key_event(label, keysym, log_text),
        handled
    );
    drain_events(engine, log_text);

    tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;
    drain_events(engine, log_text);
}

fn char_to_basic_x11_keysym(ch: char) -> Option<u32> {
    match ch {
        '\n' | '\r' => Some(0xff0d),
        ch if ch.is_ascii() => Some(ch as u32),
        _ => None,
    }
}

fn format_char_label(ch: char, log_text: bool) -> String {
    match ch {
        '\n' | '\r' => "Return".to_string(),
        ' ' => "Space".to_string(),
        _ if log_text => ch.to_string(),
        _ => "Character".to_string(),
    }
}

fn log_unsupported_char(ch: char, log_text: bool) {
    if log_text {
        log::warn!(
            "[POC] Skipping unsupported non-ASCII char {:?}; PoC only maps basic X11 keysyms",
            ch
        );
    } else {
        log::warn!(
            "[POC] Skipping unsupported non-ASCII char: byte_len={} char_count=1; PoC only maps basic X11 keysyms",
            ch.len_utf8()
        );
    }
}

fn format_key_event(label: &str, keysym: u32, log_text: bool) -> String {
    if log_text {
        return format!("'{label}' (0x{keysym:04x})");
    }

    if label == "Return" || label == "Space" {
        return label.to_string();
    }

    "Character".to_string()
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

fn format_text_summary(event_type: &str, text: &str) -> String {
    format!(
        "{event_type} byte_len={} char_count={}",
        text.len(),
        text.chars().count()
    )
}

fn format_event_text(
    event_type: &str,
    text: &str,
    cursor: Option<(i32, i32)>,
    log_text: bool,
) -> String {
    let title = event_title(event_type);
    let mut summary = format_text_summary(title, text);

    if let Some((begin, end)) = cursor {
        summary.push_str(&format!(" cursor=[{begin}..{end}]"));
    }

    if log_text {
        summary.push_str(&format!(" text={text:?}"));
    }

    summary
}

fn format_surrounding_text_request(args: &SurroundingTextArgs, log_text: bool) -> String {
    let mut summary = format_text_summary("Requested surrounding text", &args.text);
    summary.push_str(&format!(" cursor={} anchor={}", args.cursor, args.anchor));

    if log_text {
        summary.push_str(&format!(" text={:?}", args.text));
    }

    summary
}

fn event_title(event_type: &str) -> &'static str {
    match event_type {
        "commit" => "Commit",
        "preedit" => "Preedit",
        _ => "TextEvent",
    }
}

fn drain_events(engine: &ImeEngine, log_text: bool) {
    while let Some(event) = engine.poll_event() {
        match &event {
            ImeEvent::Preedit {
                text,
                cursor_begin,
                cursor_end,
            } => {
                log::info!(
                    "[POC] EVENT {}",
                    format_event_text(
                        "preedit",
                        text,
                        Some((*cursor_begin, *cursor_end)),
                        log_text
                    )
                );
            }
            ImeEvent::PreeditEnd => {
                log::info!("[POC] EVENT PreeditEnd");
            }
            ImeEvent::Commit { text } => {
                log::info!(
                    "[POC] EVENT {}",
                    format_event_text("commit", text, None, log_text)
                );
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
                    "[POC] EVENT {}",
                    format_forward_key_event(*keyval, state.0, log_text)
                );
            }
        }
    }
}

fn format_forward_key_event(keyval: u32, state: u32, log_text: bool) -> String {
    if log_text {
        format!("ForwardKey keyval=0x{keyval:x} state=0x{state:x}")
    } else {
        format!("ForwardKey state=0x{state:x}")
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

        assert!(!args.log_text);
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
    fn parses_log_text_as_explicit_opt_in() {
        let args = PocArgs::parse(["--log-text"].map(String::from)).unwrap();

        assert!(args.log_text);
    }

    #[test]
    fn formats_event_text_without_content_by_default() {
        let summary = format_event_text("commit", "密码a", None, false);

        assert_eq!(summary, "Commit byte_len=7 char_count=3");
        assert!(!summary.contains("密码a"));
    }

    #[test]
    fn formats_event_text_with_content_when_enabled() {
        let summary = format_event_text("preedit", "候选", Some((2, 2)), true);

        assert_eq!(
            summary,
            "Preedit byte_len=6 char_count=2 cursor=[2..2] text=\"候选\""
        );
    }

    #[test]
    fn formats_surrounding_text_without_content_by_default() {
        let args = SurroundingTextArgs {
            text: "上下文".to_string(),
            cursor: 9,
            anchor: 3,
        };

        let summary = format_surrounding_text_request(&args, false);

        assert_eq!(
            summary,
            "Requested surrounding text byte_len=9 char_count=3 cursor=9 anchor=3"
        );
        assert!(!summary.contains("上下文"));
    }

    #[test]
    fn formats_surrounding_text_with_content_when_enabled() {
        let args = SurroundingTextArgs {
            text: "上下文".to_string(),
            cursor: 9,
            anchor: 3,
        };

        let summary = format_surrounding_text_request(&args, true);

        assert_eq!(
            summary,
            "Requested surrounding text byte_len=9 char_count=3 cursor=9 anchor=3 text=\"上下文\""
        );
    }

    #[test]
    fn formats_key_event_without_recoverable_keysym_by_default() {
        let summary = format_key_event("a", 0x0061, false);

        assert_eq!(summary, "Character");
        assert!(!summary.contains("'a'"));
        assert!(!summary.contains("0061"));
    }

    #[test]
    fn formats_key_event_with_label_and_keysym_when_enabled() {
        assert_eq!(format_key_event("a", 0x0061, true), "'a' (0x0061)");
    }

    #[test]
    fn formats_forward_key_without_keyval_by_default() {
        let summary = format_forward_key_event(0x0061, 0, false);

        assert_eq!(summary, "ForwardKey state=0x0");
        assert!(!summary.contains("0061"));
    }

    #[test]
    fn formats_forward_key_with_keyval_when_enabled() {
        assert_eq!(
            format_forward_key_event(0x0061, 0, true),
            "ForwardKey keyval=0x61 state=0x0"
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
