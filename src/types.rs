use serde::{Deserialize, Serialize};

// ── Geometry ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

// ── Window ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    pub pid: u32,
    pub hwnd: usize,
    pub title: String,
    pub exe_name: String,
    pub rect: Rect,
    pub visible: bool,
}

// ── Element type ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "PascalCase")]
pub enum ElementType {
    Window,
    Button,
    SplitButton,
    Edit,
    Text,
    CheckBox,
    RadioButton,
    ComboBox,
    ListBox,
    ListItem,
    TreeView,
    TreeItem,
    Menu,
    MenuBar,
    MenuItem,
    TabControl,
    TabItem,
    ToolBar,
    StatusBar,
    ScrollBar,
    Slider,
    Spinner,
    ProgressBar,
    Image,
    Link,
    Group,
    Pane,
    Dialog,
    Document,
    DataGrid,
    DataItem,
    Header,
    HeaderItem,
    Table,
    TitleBar,
    ToolTip,
    Separator,
    Calendar,
    Thumb,
    Custom,
    Unknown,
}

// ── State types ───────────────────────────────────────────────────────────────

/// Toggle state for CheckBoxes, ToggleButtons, etc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToggleState {
    Off,
    On,
    Indeterminate,
}

/// Expand/Collapse state for ComboBoxes, TreeItems, etc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ExpandState {
    Collapsed,
    Expanded,
    PartiallyExpanded,
    LeafNode,
}

/// Range info for Sliders, Spinners, ProgressBars.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RangeInfo {
    pub value: f64,
    pub minimum: f64,
    pub maximum: f64,
    pub step: f64,
    pub read_only: bool,
}

// ── UI Element (the Virtual DOM node) ────────────────────────────────────────

/// A single node in the UI element tree.
///
/// The `actions` field is the key for AI agents — it explicitly lists every
/// operation that can be performed on this element via the interact API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiElement {
    /// Session-scoped unique ID. Use this for all /interact calls.
    pub oculos_id: String,

    /// Semantic element type.
    #[serde(rename = "type")]
    pub element_type: ElementType,

    /// Accessible name / label (what a screen reader would announce).
    pub label: String,

    /// Current text/value (Edit content, selected ComboBox item, etc.)
    pub value: Option<String>,

    /// Full text content for Document/RichText elements.
    pub text_content: Option<String>,

    /// Bounding box in screen coordinates.
    pub rect: Rect,

    // ── State ──────────────────────────────────────────────────────────────
    pub enabled: bool,
    pub focused: bool,
    pub is_keyboard_focusable: bool,

    /// For CheckBox, ToggleButton — "On" / "Off" / "Indeterminate"
    pub toggle_state: Option<ToggleState>,

    /// For ListItem, RadioButton, TabItem — is this currently selected?
    pub is_selected: Option<bool>,

    /// For ComboBox, TreeItem, MenuItem — expanded or collapsed?
    pub expand_state: Option<ExpandState>,

    /// For Slider, Spinner, ProgressBar — numeric range info.
    pub range: Option<RangeInfo>,

    // ── Metadata ───────────────────────────────────────────────────────────
    /// Developer-assigned automation ID (stable across runs).
    pub automation_id: Option<String>,
    pub class_name: Option<String>,
    pub help_text: Option<String>,
    pub keyboard_shortcut: Option<String>,

    // ── The key for AI agents ──────────────────────────────────────────────
    /// Explicit list of actions available on this element.
    ///
    /// Possible values:
    ///   "click"            → POST /interact/{id}/click
    ///   "set-text"         → POST /interact/{id}/set-text
    ///   "send-keys"        → POST /interact/{id}/send-keys
    ///   "toggle"           → POST /interact/{id}/toggle
    ///   "expand"           → POST /interact/{id}/expand
    ///   "collapse"         → POST /interact/{id}/collapse
    ///   "select"           → POST /interact/{id}/select
    ///   "set-range"        → POST /interact/{id}/set-range
    ///   "scroll"           → POST /interact/{id}/scroll
    ///   "scroll-into-view" → POST /interact/{id}/scroll-into-view
    ///   "focus"            → POST /interact/{id}/focus
    pub actions: Vec<String>,

    /// Child elements.
    pub children: Vec<UiElement>,
}

// ── Request payloads ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SetTextPayload {
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct SendKeysPayload {
    /// Text and `{KEY}` sequences to type into the element (see `keys.rs`).
    pub keys: String,
}

#[derive(Debug, Deserialize)]
pub struct SetRangePayload {
    pub value: f64,
}

#[derive(Debug, Deserialize)]
pub struct ScrollPayload {
    /// "up" | "down" | "left" | "right" | "page-up" | "page-down"
    pub direction: String,
}

#[derive(Debug, Deserialize)]
pub struct HighlightPayload {
    #[serde(default = "default_highlight_duration")]
    pub duration_ms: u64,
}

fn default_highlight_duration() -> u64 {
    2000
}

// ── ElementType helpers ───────────────────────────────────────────────────────

impl ElementType {
    /// Every variant, in declaration order.
    pub const ALL: &'static [ElementType] = &[
        ElementType::Window,
        ElementType::Button,
        ElementType::SplitButton,
        ElementType::Edit,
        ElementType::Text,
        ElementType::CheckBox,
        ElementType::RadioButton,
        ElementType::ComboBox,
        ElementType::ListBox,
        ElementType::ListItem,
        ElementType::TreeView,
        ElementType::TreeItem,
        ElementType::Menu,
        ElementType::MenuBar,
        ElementType::MenuItem,
        ElementType::TabControl,
        ElementType::TabItem,
        ElementType::ToolBar,
        ElementType::StatusBar,
        ElementType::ScrollBar,
        ElementType::Slider,
        ElementType::Spinner,
        ElementType::ProgressBar,
        ElementType::Image,
        ElementType::Link,
        ElementType::Group,
        ElementType::Pane,
        ElementType::Dialog,
        ElementType::Document,
        ElementType::DataGrid,
        ElementType::DataItem,
        ElementType::Header,
        ElementType::HeaderItem,
        ElementType::Table,
        ElementType::TitleBar,
        ElementType::ToolTip,
        ElementType::Separator,
        ElementType::Calendar,
        ElementType::Thumb,
        ElementType::Custom,
        ElementType::Unknown,
    ];

    /// The canonical (serialized) name, e.g. `"CheckBox"`.
    pub fn name(self) -> &'static str {
        match self {
            ElementType::Window => "Window",
            ElementType::Button => "Button",
            ElementType::SplitButton => "SplitButton",
            ElementType::Edit => "Edit",
            ElementType::Text => "Text",
            ElementType::CheckBox => "CheckBox",
            ElementType::RadioButton => "RadioButton",
            ElementType::ComboBox => "ComboBox",
            ElementType::ListBox => "ListBox",
            ElementType::ListItem => "ListItem",
            ElementType::TreeView => "TreeView",
            ElementType::TreeItem => "TreeItem",
            ElementType::Menu => "Menu",
            ElementType::MenuBar => "MenuBar",
            ElementType::MenuItem => "MenuItem",
            ElementType::TabControl => "TabControl",
            ElementType::TabItem => "TabItem",
            ElementType::ToolBar => "ToolBar",
            ElementType::StatusBar => "StatusBar",
            ElementType::ScrollBar => "ScrollBar",
            ElementType::Slider => "Slider",
            ElementType::Spinner => "Spinner",
            ElementType::ProgressBar => "ProgressBar",
            ElementType::Image => "Image",
            ElementType::Link => "Link",
            ElementType::Group => "Group",
            ElementType::Pane => "Pane",
            ElementType::Dialog => "Dialog",
            ElementType::Document => "Document",
            ElementType::DataGrid => "DataGrid",
            ElementType::DataItem => "DataItem",
            ElementType::Header => "Header",
            ElementType::HeaderItem => "HeaderItem",
            ElementType::Table => "Table",
            ElementType::TitleBar => "TitleBar",
            ElementType::ToolTip => "ToolTip",
            ElementType::Separator => "Separator",
            ElementType::Calendar => "Calendar",
            ElementType::Thumb => "Thumb",
            ElementType::Custom => "Custom",
            ElementType::Unknown => "Unknown",
        }
    }

    /// Comma-separated list of every accepted type name (for error messages / schemas).
    pub fn all_names() -> String {
        Self::ALL
            .iter()
            .map(|t| t.name())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl std::str::FromStr for ElementType {
    type Err = anyhow::Error;

    /// Case-insensitive; also accepts a few common aliases (Hyperlink, List, Tree, Tab…).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let wanted = s.trim();
        if let Some(t) = Self::ALL
            .iter()
            .find(|t| t.name().eq_ignore_ascii_case(wanted))
        {
            return Ok(*t);
        }
        let alias = match wanted.to_ascii_lowercase().as_str() {
            "hyperlink" => Some(ElementType::Link),
            "list" => Some(ElementType::ListBox),
            "tree" => Some(ElementType::TreeView),
            "tab" => Some(ElementType::TabControl),
            "textbox" | "textfield" | "input" => Some(ElementType::Edit),
            "checkbutton" => Some(ElementType::CheckBox),
            "label" | "statictext" => Some(ElementType::Text),
            "spinbutton" => Some(ElementType::Spinner),
            _ => None,
        };
        alias.ok_or_else(|| {
            crate::error::invalid_input(format!(
                "Unknown element type '{wanted}'. Valid types: {}",
                Self::all_names()
            ))
        })
    }
}

impl UiElement {
    /// A new element with the given id/type and neutral defaults
    /// (enabled, no state, no actions, no children).
    pub fn new(oculos_id: String, element_type: ElementType) -> Self {
        Self {
            oculos_id,
            element_type,
            label: String::new(),
            value: None,
            text_content: None,
            rect: Rect::default(),
            enabled: true,
            focused: false,
            is_keyboard_focusable: false,
            toggle_state: None,
            is_selected: None,
            expand_state: None,
            range: None,
            automation_id: None,
            class_name: None,
            help_text: None,
            keyboard_shortcut: None,
            actions: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Stand-in node returned when the depth limit is reached.
    pub fn depth_limit_placeholder(oculos_id: String) -> Self {
        let mut e = Self::new(oculos_id, ElementType::Unknown);
        e.enabled = false;
        e
    }
}

// ── Generic response wrapper ──────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ApiResponse<T: Serialize> {
    pub success: bool,
    pub data: Option<T>,
    pub error: Option<String>,
    /// Machine-readable error kind (`not_found`, `invalid_input`, `unsupported`,
    /// `timeout`, `permission_denied`, `forbidden`, `unauthorized`, `internal`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
}

impl<T: Serialize> ApiResponse<T> {
    pub fn ok(data: T) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
            code: None,
        }
    }
}

impl ApiResponse<()> {
    pub fn err(code: &'static str, msg: impl Into<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(msg.into()),
            code: Some(code),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn element_type_parsing_is_case_insensitive_and_complete() {
        for t in ElementType::ALL {
            assert_eq!(t.name().parse::<ElementType>().unwrap(), *t);
            assert_eq!(t.name().to_lowercase().parse::<ElementType>().unwrap(), *t);
            // serde name must match the canonical name
            assert_eq!(
                serde_json::to_value(t).unwrap(),
                serde_json::Value::String(t.name().to_string())
            );
        }
        assert_eq!(
            "hyperlink".parse::<ElementType>().unwrap(),
            ElementType::Link
        );
    }

    #[test]
    fn unknown_element_type_is_an_error() {
        let err = "Buton".parse::<ElementType>().unwrap_err();
        assert_eq!(
            crate::error::kind_of(&err),
            Some(crate::error::ErrorKind::InvalidInput)
        );
    }
}
