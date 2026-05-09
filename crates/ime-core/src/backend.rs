use crate::types::{BackendKind, ContentType, CursorRect, ImeCapabilities};

/// 平台输入法 backend 接口
///
/// 每个平台（IBus、Fcitx5 等）实现此 trait，engine 层通过 trait object 统一驱动。
///
/// 所有方法都可以从非 tokio 线程调用（例如游戏引擎主线程）。
/// 实现者自行负责将异步 D-Bus 调用桥接到内部运行时。
pub trait ImeBackend: Send + 'static {
    /// backend 类型。默认 Unknown，避免旧实现误报具体框架。
    fn backend_kind(&self) -> BackendKind {
        BackendKind::Unknown
    }

    /// backend 能力位。默认空集合，调用方必须按保守路径处理。
    fn capabilities(&self) -> ImeCapabilities {
        ImeCapabilities::NONE
    }

    /// 通知输入法：文本输入框获得焦点
    fn focus_in(&self);

    /// 通知输入法：文本输入框失去焦点
    fn focus_out(&self);

    /// 更新光标矩形，供输入法定位候选窗
    fn set_cursor_rect(&self, rect: CursorRect);

    /// 更新光标周围文本（surrounding text），部分输入法需要此上下文
    fn set_surrounding_text(&self, text: &str, cursor: i32, anchor: i32);

    /// 更新输入类型提示。默认不操作；需要区分输入类型的 backend 可覆盖。
    fn set_content_type(&self, _content_type: ContentType) {}

    /// 转发按键事件到输入法框架
    ///
    /// 返回 true 表示输入法已处理该按键，宿主不应再处理；
    /// 返回 false 表示输入法未处理，宿主正常处理。
    ///
    /// 此方法会同步等待输入法响应（最长约 200ms）。
    fn process_key_event(&self, keyval: u32, keycode: u32, state: u32, is_release: bool) -> bool;

    /// 重置输入状态（焦点切换时调用）
    fn reset(&self);
}
