//! Fcitx 4 D-Bus proxy definitions.
//!
//! Fcitx 4 binds an input context to the D-Bus sender that created it. Keep all
//! method calls and signal subscriptions on the same zbus connection.

use zbus::proxy;

#[proxy(
    interface = "org.fcitx.Fcitx.InputMethod",
    default_service = "org.fcitx.Fcitx",
    default_path = "/inputmethod"
)]
pub trait Fcitx4InputMethod {
    #[zbus(name = "CreateICv3")]
    async fn create_ic_v3(
        &self,
        appname: &str,
        pid: i32,
    ) -> zbus::Result<(i32, bool, u32, u32, u32, u32)>;
}

#[proxy(
    interface = "org.fcitx.Fcitx.InputContext",
    default_service = "org.fcitx.Fcitx"
)]
pub trait Fcitx4InputContext {
    async fn focus_in(&self) -> zbus::Result<()>;
    async fn focus_out(&self) -> zbus::Result<()>;
    async fn reset(&self) -> zbus::Result<()>;

    async fn set_cursor_rect(&self, x: i32, y: i32, w: i32, h: i32) -> zbus::Result<()>;
    async fn set_capacity(&self, caps: u32) -> zbus::Result<()>;
    async fn set_surrounding_text(&self, text: &str, cursor: u32, anchor: u32) -> zbus::Result<()>;
    async fn set_surrounding_text_position(&self, cursor: u32, anchor: u32) -> zbus::Result<()>;

    async fn process_key_event(
        &self,
        keyval: u32,
        keycode: u32,
        state: u32,
        event_type: i32,
        time: u32,
    ) -> zbus::Result<i32>;

    #[zbus(signal)]
    async fn commit_string(&self, text: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn update_formatted_preedit(
        &self,
        segments: Vec<(String, i32)>,
        cursor_pos: i32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn forward_key(&self, keyval: u32, state: u32, event_type: i32) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn delete_surrounding_text(&self, offset: i32, nchar: u32) -> zbus::Result<()>;
}
