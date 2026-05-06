/// 输入法事件，由库产出，宿主通过 poll 取出后使用
#[derive(Debug, Clone)]
pub enum ImeEvent {
    /// preedit 文本更新（拼音候选阶段）
    ///
    /// `cursor_begin` / `cursor_end` 均为 **Unicode 字符索引**（即 `char` 计数，
    /// 非字节偏移）。IBus / Fcitx5 D-Bus 接口原生返回的即为字符位置，
    /// 宿主在渲染光标时应使用 `text.chars().nth(n)` 而非字节切片。
    Preedit {
        text: String,
        cursor_begin: i32,
        cursor_end: i32,
    },

    /// preedit 结束（用户取消或清空）
    PreeditEnd,

    /// 最终提交文本（用户确认选词）
    Commit { text: String },

    /// 输入法要求删除光标周围文本
    DeleteSurroundingText { before: u32, after: u32 },

    /// 输入法将按键原样转发给宿主（输入法不处理该键）
    ForwardKey { keyval: u32, state: KeyState },
}

/// 光标矩形（屏幕坐标），供输入法框架定位候选窗
#[derive(Debug, Clone, Copy, Default)]
pub struct CursorRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// 按键修饰符状态
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyState(pub u32);

impl KeyState {
    pub const SHIFT: u32 = 1 << 0;
    pub const LOCK: u32 = 1 << 1;
    pub const CTRL: u32 = 1 << 2;
    pub const ALT: u32 = 1 << 3;
    pub const RELEASE: u32 = 1 << 30;
}

/// 文本输入类型提示
#[derive(Debug, Clone, Copy, Default)]
pub enum ContentType {
    #[default]
    Normal,
    Password,
    Number,
    Phone,
    Url,
    Email,
}
