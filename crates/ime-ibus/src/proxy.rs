//! IBus D-Bus proxy 定义
//!
//! 使用 zbus proxy 宏生成类型安全的接口包装。

use zbus::proxy;
use zbus::zvariant::OwnedObjectPath;

/// org.freedesktop.IBus 总线接口
#[proxy(
    interface = "org.freedesktop.IBus",
    default_service = "org.freedesktop.IBus",
    default_path = "/org/freedesktop/IBus"
)]
pub trait IBusBus {
    /// 创建输入上下文，返回 D-Bus 对象路径
    async fn create_input_context(&self, client_name: &str) -> zbus::Result<OwnedObjectPath>;
}

/// org.freedesktop.IBus.InputContext 接口
///
/// 注意：信号中的 `text` 参数是 IBusText（GVariant 格式 `(sa{sv}sv)`），
/// 此处用 `zbus::zvariant::Value` 接收，由上层解析具体字段。
#[proxy(
    interface = "org.freedesktop.IBus.InputContext",
    default_service = "org.freedesktop.IBus"
)]
pub trait IBusInputContext {
    // ---------- 方法 ----------

    /// 转发按键事件，返回是否被输入法处理
    async fn process_key_event(&self, keyval: u32, keycode: u32, state: u32) -> zbus::Result<bool>;

    /// 设置光标矩形（屏幕坐标）
    async fn set_cursor_location(&self, x: i32, y: i32, w: i32, h: i32) -> zbus::Result<()>;

    /// 设置 IME 能力 flags
    async fn set_capabilities(&self, caps: u32) -> zbus::Result<()>;

    /// 设置光标周围文本上下文
    async fn set_surrounding_text(
        &self,
        text: zbus::zvariant::Value<'_>,
        cursor_pos: u32,
        anchor_pos: u32,
    ) -> zbus::Result<()>;

    /// 设置输入内容类型提示
    async fn set_content_type(&self, purpose: u32, hints: u32) -> zbus::Result<()>;

    /// 通知输入法：输入框获得焦点
    async fn focus_in(&self) -> zbus::Result<()>;

    /// 通知输入法：输入框失去焦点
    async fn focus_out(&self) -> zbus::Result<()>;

    /// 重置输入状态
    async fn reset(&self) -> zbus::Result<()>;

    // ---------- 信号 ----------

    /// 输入法提交最终文本
    #[zbus(signal)]
    async fn commit_text(&self, text: zbus::zvariant::Value<'_>) -> zbus::Result<()>;

    /// 输入法更新 preedit 文本
    #[zbus(signal)]
    async fn update_preedit_text(
        &self,
        text: zbus::zvariant::Value<'_>,
        cursor_pos: u32,
        visible: bool,
    ) -> zbus::Result<()>;

    /// 输入法要求隐藏 preedit 文本（取消组合）
    #[zbus(signal)]
    async fn hide_preedit_text(&self) -> zbus::Result<()>;

    /// 输入法要求删除光标周围文本
    #[zbus(signal)]
    async fn delete_surrounding_text(&self, offset: i32, nchars: u32) -> zbus::Result<()>;

    /// 输入法将按键原样转发给宿主（输入法不处理该键）
    #[zbus(signal)]
    async fn forward_key_event(&self, keyval: u32, keycode: u32, state: u32) -> zbus::Result<()>;
}
