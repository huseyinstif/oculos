use crate::keys::KeyStep;
use crate::types::{ElementType, Rect, UiElement, WindowInfo};
use anyhow::Result;

/// Cross-platform UI automation backend.
///
/// Every OS implements this trait. OculOS never touches platform-specific code
/// outside these implementations.
///
/// Conventions for implementors:
/// - `oculos_id`s are derived from a stable element identity via
///   [`crate::registry::stable_id`] and stored in an
///   [`crate::registry::ElementRegistry`], so the same element keeps its id.
/// - Errors callers can act on are created with the helpers in
///   [`crate::error`] (`element_not_found`, `unsupported`, `invalid_input`…).
///   Never report success for an action that did not happen.
pub trait UiBackend: Send + Sync {
    // ── Discovery ──────────────────────────────────────────────────────────

    /// All visible top-level windows.
    fn list_windows(&self) -> Result<Vec<WindowInfo>>;

    /// Full UI element tree for the window owned by `pid`.
    fn get_ui_tree(&self, pid: u32) -> Result<UiElement>;

    /// Full UI element tree for a specific window handle (for apps with multiple windows).
    fn get_ui_tree_hwnd(&self, hwnd: usize) -> Result<UiElement>;

    /// Flat list of elements matching a text query and/or type filter.
    fn find_elements(
        &self,
        pid: u32,
        query: Option<&str>,
        element_type: Option<&ElementType>,
        interactive_only: bool,
    ) -> Result<Vec<UiElement>>;

    /// Same as find_elements but targets a specific HWND directly.
    fn find_elements_hwnd(
        &self,
        hwnd: usize,
        query: Option<&str>,
        element_type: Option<&ElementType>,
        interactive_only: bool,
    ) -> Result<Vec<UiElement>>;

    // ── Basic interactions ─────────────────────────────────────────────────

    /// Native invoke/click without moving the mouse.
    fn click_element(&self, oculos_id: &str) -> Result<()>;

    /// Set value via the ValuePattern (direct, no keyboard simulation).
    fn set_text(&self, oculos_id: &str, text: &str) -> Result<()>;

    /// Focus the element, then simulate keyboard input. `steps` comes from
    /// [`crate::keys::parse`], so it is already validated.
    fn send_keys(&self, oculos_id: &str, steps: &[KeyStep]) -> Result<()>;

    /// Move keyboard focus to this element.
    fn focus_element(&self, oculos_id: &str) -> Result<()>;

    // ── Pattern-specific interactions ──────────────────────────────────────

    /// Toggle a CheckBox / ToggleButton.
    fn toggle_element(&self, oculos_id: &str) -> Result<()>;

    /// Expand a ComboBox, TreeItem, or MenuItem.
    fn expand_element(&self, oculos_id: &str) -> Result<()>;

    /// Collapse a ComboBox, TreeItem, or MenuItem.
    fn collapse_element(&self, oculos_id: &str) -> Result<()>;

    /// Select a ListItem, RadioButton, or TabItem.
    fn select_element(&self, oculos_id: &str) -> Result<()>;

    /// Set a numeric value on a Slider or Spinner.
    fn set_range(&self, oculos_id: &str, value: f64) -> Result<()>;

    /// Scroll an element's container.
    /// direction: "up" | "down" | "left" | "right" | "page-up" | "page-down"
    fn scroll_element(&self, oculos_id: &str, direction: &str) -> Result<()>;

    /// Scroll this element into the visible viewport.
    fn scroll_into_view(&self, oculos_id: &str) -> Result<()>;

    // ── Window operations ──────────────────────────────────────────────────

    /// Bring the window for `pid` to the foreground.
    fn focus_window(&self, pid: u32) -> Result<()>;

    /// Close the window for `pid` gracefully.
    fn close_window(&self, pid: u32) -> Result<()>;

    // ── Highlight ────────────────────────────────────────────────────────

    /// Draw a temporary highlight rectangle around an element on-screen.
    /// Duration in milliseconds. Default no-op for unsupported platforms.
    fn highlight_element(&self, oculos_id: &str, duration_ms: u64) -> Result<Rect> {
        let _ = (oculos_id, duration_ms);
        Err(crate::error::unsupported(
            "Highlight is not supported on this platform",
        ))
    }

    // ── Screenshot ─────────────────────────────────────────────────────

    /// Capture a screenshot of the window identified by PID. Returns PNG bytes.
    fn screenshot_window(&self, pid: u32) -> Result<Vec<u8>> {
        let _ = pid;
        Err(crate::error::unsupported(
            "Screenshots are not supported on this platform",
        ))
    }

    /// Capture a screenshot of a specific element by its oculos_id. Returns PNG bytes.
    fn screenshot_element(&self, oculos_id: &str) -> Result<Vec<u8>> {
        let _ = oculos_id;
        Err(crate::error::unsupported(
            "Element screenshots are not supported on this platform",
        ))
    }
}

// ── Platform selection ────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
pub mod windows;
#[cfg(target_os = "windows")]
pub use windows::WindowsUiBackend as PlatformBackend;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub use macos::MacOsUiBackend as PlatformBackend;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
pub use linux::LinuxUiBackend as PlatformBackend;
