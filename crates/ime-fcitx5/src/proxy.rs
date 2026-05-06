//! Fcitx5 D-Bus proxy 定义
//!
//! 参考 Fcitx5 D-Bus 接口文档：
//!   https://codocs.fcitx-im.org/fcitx5/dbus/InputMethod2.html

use zbus::proxy;
use zbus::zvariant::OwnedObjectPath;

/// org.fcitx.Fcitx5.InputMethod1 —— 全局接口，用于创建 InputContext
#[proxy(
    interface = "org.fcitx.Fcitx5.InputMethod1",
    default_service = "org.fcitx.Fcitx5",
    default_path = "/org/freedesktop/portal/inputmethod"
)]
pub trait Fcitx5InputMethod {
    /// 获取版本号（同时用于连通性检测）
    async fn version(&self) -> zbus::Result<String>;

    /// 创建 InputContext，返回 (ic_object_path, uuid)
    async fn create_input_context(
        &self,
        args: Vec<(&str, &str)>,
    ) -> zbus::Result<(OwnedObjectPath, String)>;
}

/// org.fcitx.Fcitx5.InputContext1 —— 每个输入场景的上下文接口
#[proxy(
    interface = "org.fcitx.Fcitx5.InputContext1",
    default_service = "org.fcitx.Fcitx5"
)]
pub trait Fcitx5InputContext {
    // ---------- 方法 ----------

    async fn focus_in(&self) -> zbus::Result<()>;
    async fn focus_out(&self) -> zbus::Result<()>;

    /// 设置光标矩形（屏幕坐标）
    async fn set_cursor_rect(&self, x: i32, y: i32, w: i32, h: i32) -> zbus::Result<()>;

    /// 设置光标周围文本
    async fn set_surrounding_text(&self, text: &str, cursor: u32, anchor: u32) -> zbus::Result<()>;

    /// 设置能力 flags（surrounding-text = bit0，preedit = bit1）
    async fn set_capability(&self, caps: u32) -> zbus::Result<()>;

    /// 转发按键事件
    /// 参数：keyval, keycode, state, is_release, time_ms
    /// 返回：是否被输入法处理
    async fn process_key_event(
        &self,
        keyval: u32,
        keycode: u32,
        state: u32,
        is_release: bool,
        time: u32,
    ) -> zbus::Result<bool>;

    async fn reset(&self) -> zbus::Result<()>;

    // ---------- 信号 ----------

    /// 输入法提交最终文本
    #[zbus(signal)]
    async fn commit_string(&self, text: &str) -> zbus::Result<()>;

    /// 输入法更新 preedit 文本
    /// texts: Vec<(text, format_flags)>, cursor_pos
    #[zbus(signal)]
    async fn update_preedit(&self, texts: Vec<(String, i32)>, cursor_pos: i32) -> zbus::Result<()>;

    /// 输入法将按键原样转发给宿主（输入法不处理该键）
    /// 对应 Fcitx5 D-Bus 接口中的 ForwardKey 信号
    #[zbus(signal)]
    async fn forward_key(&self, keyval: u32, state: u32, is_release: bool) -> zbus::Result<()>;
}
