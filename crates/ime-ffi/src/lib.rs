//! C ABI 导出层
//!
//! 供任何可以加载 C 共享库的宿主使用（C#/P-Invoke、Python ctypes、Lua FFI、
//! GDScript 等）。所有函数保证：不 panic（内部 catch_unwind）、不抛异常、线程安全。

mod handle;

use std::ffi::CStr;
use std::os::raw::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};

use handle::ImeHandle;

// ============================================================
// 公开 C ABI
// ============================================================

/// 创建 IME 实例。
///
/// 自动检测当前系统的输入法框架（Fcitx5 优先，其次 IBus）。
/// 返回非 null 的不透明指针表示成功；返回 null 表示初始化失败
/// （输入法框架不可用，宿主应回退到自身的 IME 处理路径）。
#[no_mangle]
pub extern "C" fn ime_create() -> *mut ImeHandle {
    let result = catch_unwind(|| {
        #[cfg(unix)]
        {
            let _ = env_logger::try_init();
        }
        ImeHandle::create()
    });
    match result {
        Ok(Some(handle)) => Box::into_raw(Box::new(handle)),
        _ => std::ptr::null_mut(),
    }
}

/// 销毁 IME 实例，释放所有资源。
///
/// # Safety
/// `handle` 必须是由 `ime_create` 返回的有效指针，且只调用一次。
#[no_mangle]
pub unsafe extern "C" fn ime_destroy(handle: *mut ImeHandle) {
    if handle.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| {
        drop(Box::from_raw(handle));
    }));
}

/// 通知输入法：文本输入框获得焦点。
///
/// # Safety
/// `handle` 必须是有效的非 null 指针。
#[no_mangle]
pub unsafe extern "C" fn ime_focus_in(handle: *mut ImeHandle) {
    with_handle(handle, |h| h.focus_in());
}

/// 通知输入法：文本输入框失去焦点。
///
/// # Safety
/// `handle` 必须是有效的非 null 指针。
#[no_mangle]
pub unsafe extern "C" fn ime_focus_out(handle: *mut ImeHandle) {
    with_handle(handle, |h| h.focus_out());
}

/// 更新光标矩形（屏幕坐标），供输入法定位候选窗。
///
/// # Safety
/// `handle` 必须是有效的非 null 指针。
#[no_mangle]
pub unsafe extern "C" fn ime_set_cursor_rect(
    handle: *mut ImeHandle,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    with_handle(handle, |hdl| {
        hdl.set_cursor_rect(ime_core::CursorRect {
            x,
            y,
            width: w,
            height: h,
        });
    });
}

/// 更新光标周围文本。`text` 为 UTF-8 字符串，`cursor` / `anchor` 为字节偏移。
///
/// # Safety
/// `handle` 必须是有效的非 null 指针；`text` 为有效 null 结尾 UTF-8 字符串或 null。
#[no_mangle]
pub unsafe extern "C" fn ime_set_surrounding_text(
    handle: *mut ImeHandle,
    text: *const c_char,
    cursor: i32,
    anchor: i32,
) {
    with_handle(handle, |h| {
        let s = if text.is_null() {
            ""
        } else {
            match unsafe { CStr::from_ptr(text) }.to_str() {
                Ok(s) => s,
                Err(_) => return,
            }
        };
        h.set_surrounding_text(s, cursor, anchor);
    });
}

/// 返回当前 backend 类型。
///
/// 返回值：
/// - 0 = None / Unknown
/// - 1 = Fcitx5
/// - 2 = Fcitx4
/// - 3 = IBus
///
/// # Safety
/// `handle` 必须是有效的非 null 指针；null 返回 0。
#[no_mangle]
pub unsafe extern "C" fn ime_backend_kind(handle: *mut ImeHandle) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() {
            return 0i32;
        }
        let h = &*handle;
        h.backend_kind().as_abi()
    }));
    result.unwrap_or(0)
}

/// 返回当前 backend 能力位集合。
///
/// 位定义：
/// - bit 0 = Preedit event
/// - bit 1 = Commit event
/// - bit 2 = ForwardKey event
/// - bit 3 = DeleteSurroundingText event
/// - bit 4 = set_surrounding_text
/// - bit 5 = set_content_type
///
/// # Safety
/// `handle` 必须是有效的非 null 指针；null 返回 0。
#[no_mangle]
pub unsafe extern "C" fn ime_capabilities(handle: *mut ImeHandle) -> u32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() {
            return 0u32;
        }
        let h = &*handle;
        h.capabilities().bits()
    }));
    result.unwrap_or(0)
}

/// 更新输入类型提示。
///
/// `content_type`：
/// - 0 = Normal
/// - 1 = Password
/// - 2 = Number
/// - 3 = Phone
/// - 4 = Url
/// - 5 = Email
///
/// null handle 或未知枚举值会被忽略。
///
/// # Safety
/// `handle` 必须是有效的非 null 指针。
#[no_mangle]
pub unsafe extern "C" fn ime_set_content_type(handle: *mut ImeHandle, content_type: i32) {
    with_handle(handle, |h| {
        if let Some(content_type) = content_type_from_abi(content_type) {
            h.set_content_type(content_type);
        }
    });
}

/// 重置输入状态（焦点切换时调用）。
///
/// # Safety
/// `handle` 必须是有效的非 null 指针。
#[no_mangle]
pub unsafe extern "C" fn ime_reset(handle: *mut ImeHandle) {
    with_handle(handle, |h| h.reset());
}

/// 转发按键事件到输入法框架。
///
/// - `keyval`：X11 keysym（如字母 `'a'` = 0x0061，回车 = 0xff0d）；
///   宿主负责将自身的 KeyCode 转换为 keysym
/// - `keycode`：硬件扫描码（可传 0，多数输入法不依赖此值）
/// - `state`：X11 modifier mask（Shift=1, Lock=2, Ctrl=4, Alt/Mod1=8）
/// - `is_release`：0 = keydown，1 = keyup
///
/// 返回值：
/// - 1 = 输入法已消费此按键，宿主不应再处理
/// - 0 = 输入法未处理，宿主正常处理
///
/// # Safety
/// `handle` 必须是有效的非 null 指针。
#[no_mangle]
pub unsafe extern "C" fn ime_process_key_event(
    handle: *mut ImeHandle,
    keyval: u32,
    keycode: u32,
    state: u32,
    is_release: i32,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() {
            return 0i32;
        }
        let h = &*handle;
        if h.process_key_event(keyval, keycode, state, is_release != 0) {
            1
        } else {
            0
        }
    }));
    result.unwrap_or(0)
}

/// 取出下一个待处理事件。
///
/// 返回值（event_type 字段）：
/// - 0 = 无事件
/// - 1 = Preedit（预编辑文本，`text` + `cursor_begin` + `cursor_end` 有效）
/// - 2 = PreeditEnd（预编辑结束，`text` 为空）
/// - 3 = Commit（提交文本，`text` 有效）
/// - 4 = DeleteSurroundingText（`param1` = before，`param2` = after）
/// - 5 = ForwardKey（`param1` = keyval，`param2` = state）
///
/// # Safety
/// `handle` 和 `out_data` 必须是有效的非 null 指针。
#[no_mangle]
pub unsafe extern "C" fn ime_poll_event(
    handle: *mut ImeHandle,
    out_data: *mut ImeEventData,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() || out_data.is_null() {
            return 0i32;
        }
        let h = &*handle;
        match h.poll_event() {
            None => 0,
            Some(event) => {
                let data = &mut *out_data;
                fill_event_data(data, event);
                data.event_type
            }
        }
    }));
    result.unwrap_or(0)
}

// ============================================================
// C 侧数据结构
// ============================================================

/// 宿主读取 IME 事件的数据包
#[repr(C)]
pub struct ImeEventData {
    /// 事件类型（见 ime_poll_event 说明）
    pub event_type: i32,
    /// UTF-8 文本（preedit 或 commit 内容），null 结尾
    pub text: [u8; 2048],
    /// preedit 光标起始（Unicode 字符索引）
    pub cursor_begin: i32,
    /// preedit 光标结束（Unicode 字符索引）
    pub cursor_end: i32,
    /// 多用途参数1（DeleteSurroundingText: before_length; ForwardKey: keyval）
    pub param1: i32,
    /// 多用途参数2（DeleteSurroundingText: after_length; ForwardKey: state）
    pub param2: i32,
}

// ============================================================
// 内部工具函数
// ============================================================

unsafe fn with_handle<F: FnOnce(&ImeHandle)>(handle: *mut ImeHandle, f: F) {
    if handle.is_null() {
        return;
    }
    let _ = catch_unwind(AssertUnwindSafe(|| f(&*handle)));
}

fn content_type_from_abi(value: i32) -> Option<ime_core::ContentType> {
    match value {
        0 => Some(ime_core::ContentType::Normal),
        1 => Some(ime_core::ContentType::Password),
        2 => Some(ime_core::ContentType::Number),
        3 => Some(ime_core::ContentType::Phone),
        4 => Some(ime_core::ContentType::Url),
        5 => Some(ime_core::ContentType::Email),
        _ => None,
    }
}

fn fill_event_data(data: &mut ImeEventData, event: ime_core::ImeEvent) {
    // 只重置可能残留脏值的标量字段；text 由 write_text 按需写入（含 null 终止符）
    data.event_type = 0;
    data.cursor_begin = 0;
    data.cursor_end = 0;
    data.param1 = 0;
    data.param2 = 0;
    data.text[0] = 0; // 默认空字符串

    match event {
        ime_core::ImeEvent::Preedit {
            text,
            cursor_begin,
            cursor_end,
        } => {
            data.event_type = 1;
            write_text(&mut data.text, &text);
            data.cursor_begin = cursor_begin;
            data.cursor_end = cursor_end;
        }
        ime_core::ImeEvent::PreeditEnd => {
            data.event_type = 2;
        }
        ime_core::ImeEvent::Commit { text } => {
            data.event_type = 3;
            write_text(&mut data.text, &text);
        }
        ime_core::ImeEvent::DeleteSurroundingText { before, after } => {
            data.event_type = 4;
            data.param1 = before as i32;
            data.param2 = after as i32;
        }
        ime_core::ImeEvent::ForwardKey { keyval, state } => {
            data.event_type = 5;
            data.param1 = keyval as i32;
            data.param2 = state.0 as i32;
        }
    }
}

fn write_text(buf: &mut [u8; 2048], text: &str) {
    let bytes = text.as_bytes();
    // 预留 1 字节给 null 终止符
    let mut len = bytes.len().min(2047);
    // 若截断点落在 UTF-8 续字节（0b10xxxxxx）中间，向前退到字符边界，
    // 保证 buffer 内容始终是合法 UTF-8
    while len > 0 && !text.is_char_boundary(len) {
        len -= 1;
    }
    buf[..len].copy_from_slice(&bytes[..len]);
    buf[len] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_handle_calls_are_noops() {
        unsafe {
            ime_focus_in(std::ptr::null_mut());
            ime_focus_out(std::ptr::null_mut());
            ime_set_cursor_rect(std::ptr::null_mut(), 1, 2, 3, 4);
            ime_set_surrounding_text(std::ptr::null_mut(), std::ptr::null(), 0, 0);
            ime_set_content_type(std::ptr::null_mut(), 999);
            ime_reset(std::ptr::null_mut());
            assert_eq!(ime_backend_kind(std::ptr::null_mut()), 0);
            assert_eq!(ime_capabilities(std::ptr::null_mut()), 0);
            assert_eq!(ime_process_key_event(std::ptr::null_mut(), 0, 0, 0, 0), 0);
        }

        let mut event = ImeEventData {
            event_type: -1,
            text: [1; 2048],
            cursor_begin: -1,
            cursor_end: -1,
            param1: -1,
            param2: -1,
        };
        unsafe {
            assert_eq!(ime_poll_event(std::ptr::null_mut(), &mut event), 0);
            assert_eq!(
                ime_poll_event(std::ptr::null_mut(), std::ptr::null_mut()),
                0
            );
        }
    }

    #[test]
    fn write_text_writes_ascii_and_null_terminator() {
        let mut buf = [0u8; 2048];

        write_text(&mut buf, "abc");

        assert_eq!(&buf[..4], b"abc\0");
    }

    #[test]
    fn write_text_truncates_at_utf8_boundary() {
        let mut buf = [0u8; 2048];
        let text = format!("{}中", "a".repeat(2046));

        write_text(&mut buf, &text);

        assert_eq!(buf[2046], 0);
        assert!(std::str::from_utf8(&buf[..2046]).is_ok());
    }

    #[test]
    fn fill_event_data_clears_stale_values_for_preedit_end() {
        let mut event = ImeEventData {
            event_type: 3,
            text: [b'x'; 2048],
            cursor_begin: 7,
            cursor_end: 8,
            param1: 9,
            param2: 10,
        };

        fill_event_data(&mut event, ime_core::ImeEvent::PreeditEnd);

        assert_eq!(event.event_type, 2);
        assert_eq!(event.text[0], 0);
        assert_eq!(event.cursor_begin, 0);
        assert_eq!(event.cursor_end, 0);
        assert_eq!(event.param1, 0);
        assert_eq!(event.param2, 0);
    }

    #[test]
    fn invalid_content_type_values_are_ignored() {
        assert!(content_type_from_abi(0).is_some());
        assert!(content_type_from_abi(5).is_some());
        assert!(content_type_from_abi(-1).is_none());
        assert!(content_type_from_abi(7).is_none());
    }
}
