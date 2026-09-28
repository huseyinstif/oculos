//! macOS backend built on the Accessibility API (`AXUIElement`).
//!
//! - The Accessibility permission is checked lazily on every operation, so
//!   granting it while the server runs takes effect immediately.
//! - `oculos_id`s are derived from `(pid, CFHash(element))`, so the same element
//!   keeps its id across tree/find calls.
//! - The attributes of an element are read in a single
//!   `AXUIElementCopyMultipleAttributeValues` round trip. Searches first read a
//!   small set (role, title, …) and only build full elements for matches.
//! - `hwnd` values are CoreGraphics window numbers (`kCGWindowNumber`); the hwnd
//!   endpoints map them to an AX window by matching the window frame.
//! - Keyboard and scroll input are synthesised with CGEvents: text as Unicode
//!   strings, shortcuts with US-ANSI virtual key codes.

use std::collections::HashMap;
use std::ffi::c_void;
use std::fmt;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use accessibility::AXUIElement;
use accessibility_sys as ax;
use accessibility_sys::AXError;
use anyhow::{anyhow, Result};
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{
    CFGetTypeID, CFHash, CFIndex, CFNullGetTypeID, CFRelease, CFType, CFTypeRef, TCFType,
};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::{CFDictionary, CFDictionaryGetTypeID, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringGetTypeID, CFStringRef};
use core_graphics::display::CGDisplay;
use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, CGKeyCode};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use core_graphics::window as cgw;
use dashmap::DashSet;

use crate::error;
use crate::keys::{Key, KeyStep, Modifier};
use crate::platform::UiBackend;
use crate::registry::{self, ElementRegistry};
use crate::types::{ElementType, ExpandState, RangeInfo, Rect, ToggleState, UiElement, WindowInfo};

// ── Tunables ──────────────────────────────────────────────────────────────────

/// Nodes deeper than this become `UiElement::depth_limit_placeholder`s.
const MAX_DEPTH: u32 = 48;
/// Maximum number of elements returned by a search.
const MAX_RESULTS: usize = 500;
/// Global AX messaging timeout. The system default (6 s per call) lets a single
/// hung application stall every request.
const MESSAGING_TIMEOUT_SECS: f32 = 1.5;
/// Consecutive `kAXErrorCannotComplete` replies after which a tree walk treats
/// the application as unresponsive and stops descending.
const MAX_WALK_TIMEOUTS: u32 = 3;
/// Bound on the id-collision probe loop (practically never more than 1 probe).
const MAX_ID_PROBES: u32 = 64;
/// `CGEventKeyboardSetUnicodeString` accepts at most 20 UTF-16 units per event.
const MAX_UNICODE_CHUNK: usize = 20;
/// Pause after each posted keyboard event.
const KEY_EVENT_DELAY: Duration = Duration::from_millis(4);
/// Pause after moving focus before typing.
const FOCUS_SETTLE: Duration = Duration::from_millis(60);
/// Polling interval / attempts while waiting for an activated app to become frontmost.
const ACTIVATE_POLL: Duration = Duration::from_millis(50);
const ACTIVATE_ATTEMPTS: u32 = 10;
/// Chromium builds its accessibility tree asynchronously once enabled.
const MANUAL_AX_SETTLE: Duration = Duration::from_millis(250);
/// Time the window server gets to route a scroll event before the pointer is restored.
const SCROLL_SETTLE: Duration = Duration::from_millis(50);
/// Allowed difference (points) between a CG window frame and its AX frame.
const FRAME_TOLERANCE: f64 = 2.0;

const PERMISSION_MSG: &str =
    "Grant Accessibility permission: System Settings → Privacy & Security → Accessibility";

// ── Raw CoreGraphics calls not covered by core-graphics 0.23 ──────────────────
// (it cannot set an event's location, and its CGEvent hides the raw pointer).

/// `kCGHIDEventTap`
const HID_EVENT_TAP: u32 = 0;
/// `kCGScrollEventUnitLine`
const SCROLL_UNIT_LINE: u32 = 1;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventCreateScrollWheelEvent2(
        source: *mut c_void,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> *mut c_void;
    fn CGEventSetLocation(event: *mut c_void, location: CGPoint);
    fn CGEventPost(tap: u32, event: *mut c_void);
}

// ── Element registry ──────────────────────────────────────────────────────────

/// `AXUIElementRef`s are immutable CF objects; the AX API accepts them from any thread.
#[derive(Clone)]
struct SafeElement(AXUIElement);
unsafe impl Send for SafeElement {}
unsafe impl Sync for SafeElement {}

// ── Permission & global setup ─────────────────────────────────────────────────

/// Whether the global messaging timeout has been applied while trusted.
static TIMEOUT_APPLIED: AtomicBool = AtomicBool::new(false);

fn set_global_messaging_timeout() {
    // Setting the timeout on the system-wide element changes the global default.
    let system = AXUIElement::system_wide();
    let err = unsafe {
        ax::AXUIElementSetMessagingTimeout(system.as_concrete_TypeRef(), MESSAGING_TIMEOUT_SECS)
    };
    if err != ax::kAXErrorSuccess {
        tracing::debug!(
            "AXUIElementSetMessagingTimeout failed: {}",
            ax_error_name(err)
        );
    }
}

/// Fails with `permission_denied` until the Accessibility permission is granted.
fn ensure_trusted() -> Result<()> {
    if !unsafe { ax::AXIsProcessTrusted() } {
        return Err(error::permission_denied(PERMISSION_MSG));
    }
    if !TIMEOUT_APPLIED.swap(true, Ordering::Relaxed) {
        set_global_messaging_timeout();
    }
    Ok(())
}

// ── AX error mapping ──────────────────────────────────────────────────────────

/// What an AX call was about, for error messages.
#[derive(Clone, Copy)]
enum Subject<'a> {
    Element(&'a str),
    App(i32),
    Window(usize),
}

impl Subject<'_> {
    /// The "it no longer exists" error for this subject.
    fn gone(self) -> anyhow::Error {
        match self {
            Subject::Element(id) => error::element_not_found(id),
            Subject::App(pid) => error::not_found(format!(
                "No accessible application with PID {pid} (it is not running, has no UI, or is not responding)"
            )),
            Subject::Window(hwnd) => error::not_found(format!(
                "Window {hwnd} no longer exists or is not responding — call GET /windows again"
            )),
        }
    }
}

impl fmt::Display for Subject<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Subject::Element(id) => write!(f, "element '{id}'"),
            Subject::App(pid) => write!(f, "application with PID {pid}"),
            Subject::Window(hwnd) => write!(f, "window {hwnd}"),
        }
    }
}

/// Errors meaning the target is gone / unreachable or AX is disabled, as opposed
/// to "this attribute or action is not supported".
fn is_fatal(code: AXError) -> bool {
    matches!(
        code,
        ax::kAXErrorInvalidUIElement | ax::kAXErrorCannotComplete | ax::kAXErrorAPIDisabled
    )
}

fn ax_error(code: AXError, subject: Subject<'_>, what: &str) -> anyhow::Error {
    match code {
        ax::kAXErrorInvalidUIElement | ax::kAXErrorCannotComplete => subject.gone(),
        ax::kAXErrorAPIDisabled => error::permission_denied(PERMISSION_MSG),
        ax::kAXErrorActionUnsupported
        | ax::kAXErrorAttributeUnsupported
        | ax::kAXErrorNotImplemented
        | ax::kAXErrorIllegalArgument => {
            error::unsupported(format!("The {subject} does not support {what}"))
        }
        other => anyhow!("{what} failed on the {subject}: {}", ax_error_name(other)),
    }
}

fn ax_error_name(code: AXError) -> String {
    let name = match code {
        ax::kAXErrorSuccess => "kAXErrorSuccess",
        ax::kAXErrorFailure => "kAXErrorFailure",
        ax::kAXErrorIllegalArgument => "kAXErrorIllegalArgument",
        ax::kAXErrorInvalidUIElement => "kAXErrorInvalidUIElement",
        ax::kAXErrorCannotComplete => "kAXErrorCannotComplete",
        ax::kAXErrorAttributeUnsupported => "kAXErrorAttributeUnsupported",
        ax::kAXErrorActionUnsupported => "kAXErrorActionUnsupported",
        ax::kAXErrorNotImplemented => "kAXErrorNotImplemented",
        ax::kAXErrorAPIDisabled => "kAXErrorAPIDisabled",
        ax::kAXErrorNoValue => "kAXErrorNoValue",
        _ => return format!("AXError {code}"),
    };
    format!("{name} ({code})")
}

// ── Low-level AX helpers ──────────────────────────────────────────────────────

// These call the C API directly: the `accessibility` crate's wrappers panic
// if an app reports success but hands back NULL.

fn get_attr(el: &AXUIElement, name: &'static str) -> Result<CFType, AXError> {
    let attr = CFString::from_static_string(name);
    let mut value: CFTypeRef = ptr::null();
    let err = unsafe {
        ax::AXUIElementCopyAttributeValue(
            el.as_concrete_TypeRef(),
            attr.as_concrete_TypeRef(),
            &mut value,
        )
    };
    if err != ax::kAXErrorSuccess {
        return Err(err);
    }
    if value.is_null() {
        return Err(ax::kAXErrorNoValue);
    }
    // SAFETY: a successful Copy call hands us ownership of `value`.
    Ok(unsafe { CFType::wrap_under_create_rule(value) })
}

fn set_attr(el: &AXUIElement, name: &'static str, value: CFType) -> Result<(), AXError> {
    let attr = CFString::from_static_string(name);
    let err = unsafe {
        ax::AXUIElementSetAttributeValue(
            el.as_concrete_TypeRef(),
            attr.as_concrete_TypeRef(),
            value.as_CFTypeRef(),
        )
    };
    if err == ax::kAXErrorSuccess {
        Ok(())
    } else {
        Err(err)
    }
}

fn cf_bool(value: bool) -> CFType {
    CFBoolean::from(value).into_CFType()
}

/// `Ok(false)` when the attribute is missing or read-only; `Err` only for fatal errors.
fn settable(el: &AXUIElement, name: &'static str) -> Result<bool, AXError> {
    let attr = CFString::from_static_string(name);
    let mut out = false;
    let err = unsafe {
        ax::AXUIElementIsAttributeSettable(
            el.as_concrete_TypeRef(),
            attr.as_concrete_TypeRef(),
            &mut out,
        )
    };
    match err {
        ax::kAXErrorSuccess => Ok(out),
        code if is_fatal(code) => Err(code),
        _ => Ok(false),
    }
}

/// Names of the AX actions the element supports; `Err` only for fatal errors.
fn ax_actions(el: &AXUIElement) -> Result<Vec<String>, AXError> {
    let mut names: CFArrayRef = ptr::null();
    let err = unsafe { ax::AXUIElementCopyActionNames(el.as_concrete_TypeRef(), &mut names) };
    match err {
        ax::kAXErrorSuccess if !names.is_null() => {
            // SAFETY: a successful Copy call hands us ownership of `names`.
            let names: CFArray = unsafe { CFArray::wrap_under_create_rule(names) };
            let string_type = unsafe { CFStringGetTypeID() };
            Ok(names
                .iter()
                .filter_map(|item| {
                    let p: *const c_void = *item;
                    // SAFETY: `p` is owned by `names` and type-checked before wrapping.
                    unsafe {
                        if p.is_null() || CFGetTypeID(p) != string_type {
                            return None;
                        }
                        Some(CFString::wrap_under_get_rule(p as CFStringRef).to_string())
                    }
                })
                .collect())
        }
        code if is_fatal(code) => Err(code),
        _ => Ok(Vec::new()),
    }
}

fn perform(el: &AXUIElement, action: &'static str) -> Result<(), AXError> {
    let action = CFString::from_static_string(action);
    let err = unsafe {
        ax::AXUIElementPerformAction(el.as_concrete_TypeRef(), action.as_concrete_TypeRef())
    };
    if err == ax::kAXErrorSuccess {
        Ok(())
    } else {
        Err(err)
    }
}

/// Perform an action, tolerating the timeout of actions that start a nested
/// run loop in the target app (opening a menu, a modal alert…): the AX reply
/// only comes back once that loop ends, so the call fails with
/// `kAXErrorCannotComplete` although the action ran. If the element still
/// answers, the app is alive and busy with our action.
fn perform_action(el: &AXUIElement, action: &'static str) -> Result<(), AXError> {
    match perform(el, action) {
        Err(ax::kAXErrorCannotComplete) if get_attr(el, "AXRole").is_ok() => {
            tracing::debug!("{action} timed out but the element is alive; assuming it ran");
            Ok(())
        }
        other => other,
    }
}

fn role_of(el: &AXUIElement) -> Option<String> {
    get_attr(el, "AXRole")
        .ok()?
        .downcast::<CFString>()
        .map(|s| s.to_string())
}

fn cf_hash(el: &AXUIElement) -> usize {
    unsafe { CFHash(el.as_CFTypeRef()) }
}

fn cf_to_bool(v: &CFType) -> Option<bool> {
    if let Some(b) = v.downcast::<CFBoolean>() {
        return Some(b.into());
    }
    v.downcast::<CFNumber>()
        .and_then(|n| n.to_i64())
        .map(|n| n != 0)
}

fn cf_to_f64(v: &CFType) -> Option<f64> {
    if let Some(n) = v.downcast::<CFNumber>() {
        return n.to_f64().or_else(|| n.to_i64().map(|i| i as f64));
    }
    v.downcast::<CFBoolean>()
        .map(|b| if bool::from(b) { 1.0 } else { 0.0 })
}

/// The `AXUIElement`s in a CFArray value (other entries are skipped).
fn ax_elements(v: &CFType) -> Vec<AXUIElement> {
    let Some(array) = v.downcast::<CFArray>() else {
        return Vec::new();
    };
    let element_type = unsafe { ax::AXUIElementGetTypeID() };
    array
        .iter()
        .filter_map(|item| {
            let p: *const c_void = *item;
            // SAFETY: `p` is a live CF object owned by `array`; its type is
            // checked before it is retained as an AXUIElement.
            unsafe {
                if p.is_null() || CFGetTypeID(p) != element_type {
                    return None;
                }
                Some(AXUIElement::wrap_under_get_rule(p as ax::AXUIElementRef))
            }
        })
        .collect()
}

/// Wrap one entry of an `AXUIElementCopyMultipleAttributeValues` result.
/// Per-attribute errors (AXValues of type kAXValueAXErrorType) and nulls become `None`.
fn wrap_value(p: *const c_void) -> Option<CFType> {
    if p.is_null() {
        return None;
    }
    // SAFETY: `p` is a live CF object owned by the result array; we retain it.
    unsafe {
        let type_id = CFGetTypeID(p);
        if type_id == CFNullGetTypeID() {
            return None;
        }
        if type_id == ax::AXValueGetTypeID()
            && ax::AXValueGetType(p as ax::AXValueRef) == ax::kAXValueTypeAXError
        {
            return None;
        }
        Some(CFType::wrap_under_get_rule(p))
    }
}

fn cg_point(v: &CFType) -> Option<CGPoint> {
    let mut out = CGPoint::default();
    // SAFETY: `out` is a CGPoint, matching kAXValueTypeCGPoint.
    let ok = unsafe {
        read_ax_value(
            v,
            ax::kAXValueTypeCGPoint,
            &mut out as *mut CGPoint as *mut c_void,
        )
    };
    ok.then_some(out)
}

fn cg_size(v: &CFType) -> Option<CGSize> {
    let mut out = CGSize::default();
    // SAFETY: `out` is a CGSize, matching kAXValueTypeCGSize.
    let ok = unsafe {
        read_ax_value(
            v,
            ax::kAXValueTypeCGSize,
            &mut out as *mut CGSize as *mut c_void,
        )
    };
    ok.then_some(out)
}

/// # Safety
/// `out` must point to a value of the C type described by `kind`.
unsafe fn read_ax_value(v: &CFType, kind: ax::AXValueType, out: *mut c_void) -> bool {
    let r = v.as_CFTypeRef();
    CFGetTypeID(r) == ax::AXValueGetTypeID()
        && ax::AXValueGetType(r as ax::AXValueRef) == kind
        && ax::AXValueGetValue(r as ax::AXValueRef, kind, out)
}

fn to_rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
    Rect {
        x: x.round() as i32,
        y: y.round() as i32,
        width: width.round() as i32,
        height: height.round() as i32,
    }
}

// ── Batched attribute fetch ───────────────────────────────────────────────────

const ROLE: usize = 0;
const SUBROLE: usize = 1;
const TITLE: usize = 2;
const DESCRIPTION: usize = 3;
const IDENTIFIER: usize = 4;
const VALUE: usize = 5;
const PLACEHOLDER: usize = 6;
const CHILDREN: usize = 7;
const ENABLED: usize = 8;
const FOCUSED: usize = 9;
const POSITION: usize = 10;
const SIZE: usize = 11;
const HELP: usize = 12;
const SELECTED: usize = 13;
const EXPANDED: usize = 14;
const DISCLOSING: usize = 15;
const MIN_VALUE: usize = 16;
const MAX_VALUE: usize = 17;
const VALUE_INCREMENT: usize = 18;
const ATTR_COUNT: usize = 19;

const ATTR_NAMES: [&str; ATTR_COUNT] = [
    "AXRole",
    "AXSubrole",
    "AXTitle",
    "AXDescription",
    "AXIdentifier",
    "AXValue",
    "AXPlaceholderValue",
    "AXChildren",
    "AXEnabled",
    "AXFocused",
    "AXPosition",
    "AXSize",
    "AXHelp",
    "AXSelected",
    "AXExpanded",
    "AXDisclosing",
    "AXMinValue",
    "AXMaxValue",
    "AXValueIncrement",
];

/// Enough to decide whether an element matches a search, and to descend.
const LIGHT_ATTRS: [usize; 8] = [
    ROLE,
    SUBROLE,
    TITLE,
    DESCRIPTION,
    IDENTIFIER,
    VALUE,
    PLACEHOLDER,
    CHILDREN,
];
/// Everything not in `LIGHT_ATTRS`.
const EXTRA_ATTRS: [usize; 11] = [
    ENABLED,
    FOCUSED,
    POSITION,
    SIZE,
    HELP,
    SELECTED,
    EXPANDED,
    DISCLOSING,
    MIN_VALUE,
    MAX_VALUE,
    VALUE_INCREMENT,
];
const ALL_ATTRS: [usize; ATTR_COUNT] = [
    ROLE,
    SUBROLE,
    TITLE,
    DESCRIPTION,
    IDENTIFIER,
    VALUE,
    PLACEHOLDER,
    CHILDREN,
    ENABLED,
    FOCUSED,
    POSITION,
    SIZE,
    HELP,
    SELECTED,
    EXPANDED,
    DISCLOSING,
    MIN_VALUE,
    MAX_VALUE,
    VALUE_INCREMENT,
];
const FRAME_ATTRS: [usize; 4] = [ROLE, TITLE, POSITION, SIZE];

#[derive(Clone, Copy)]
enum AttrSet {
    Light = 0,
    Extra = 1,
    All = 2,
    /// Role, title and frame (for window matching and scrolling).
    Frame = 3,
}

impl AttrSet {
    fn indices(self) -> &'static [usize] {
        match self {
            AttrSet::Light => &LIGHT_ATTRS,
            AttrSet::Extra => &EXTRA_ATTRS,
            AttrSet::All => &ALL_ATTRS,
            AttrSet::Frame => &FRAME_ATTRS,
        }
    }
}

fn attr_name_array(set: AttrSet) -> CFArray<CFString> {
    let names: Vec<CFString> = set
        .indices()
        .iter()
        .map(|&i| CFString::from_static_string(ATTR_NAMES[i]))
        .collect();
    CFArray::from_CFTypes(&names)
}

thread_local! {
    /// CFArrays of attribute names per `AttrSet`, built once per thread.
    static ATTR_ARRAYS: [CFArray<CFString>; 4] = [
        attr_name_array(AttrSet::Light),
        attr_name_array(AttrSet::Extra),
        attr_name_array(AttrSet::All),
        attr_name_array(AttrSet::Frame),
    ];
}

/// Attribute values of one element, indexed by the constants above.
struct Attrs(Vec<Option<CFType>>);

impl Attrs {
    fn fetch(el: &AXUIElement, set: AttrSet) -> Result<Self, AXError> {
        let mut attrs = Attrs(vec![None; ATTR_COUNT]);
        attrs.fill(el, set)?;
        Ok(attrs)
    }

    /// Read `set` in one IPC round trip, overwriting those slots.
    fn fill(&mut self, el: &AXUIElement, set: AttrSet) -> Result<(), AXError> {
        let mut out: CFArrayRef = ptr::null();
        let err = ATTR_ARRAYS.with(|arrays| unsafe {
            ax::AXUIElementCopyMultipleAttributeValues(
                el.as_concrete_TypeRef(),
                arrays[set as usize].as_concrete_TypeRef(),
                0,
                &mut out,
            )
        });
        if err != ax::kAXErrorSuccess {
            return Err(err);
        }
        if out.is_null() {
            return Err(ax::kAXErrorFailure);
        }
        // SAFETY: a successful Copy call hands us ownership of `out`.
        let values: CFArray = unsafe { CFArray::wrap_under_create_rule(out) };
        for (pos, &slot) in set.indices().iter().enumerate() {
            self.0[slot] = values.get(pos as CFIndex).and_then(|p| wrap_value(*p));
        }
        Ok(())
    }

    fn get(&self, i: usize) -> Option<&CFType> {
        self.0.get(i).and_then(Option::as_ref)
    }

    fn has(&self, i: usize) -> bool {
        self.get(i).is_some()
    }

    fn string(&self, i: usize) -> Option<String> {
        self.get(i)?.downcast::<CFString>().map(|s| s.to_string())
    }

    /// A string attribute that is not blank.
    fn text(&self, i: usize) -> Option<String> {
        self.string(i).filter(|s| !s.trim().is_empty())
    }

    fn flag(&self, i: usize) -> Option<bool> {
        cf_to_bool(self.get(i)?)
    }

    fn number(&self, i: usize) -> Option<f64> {
        cf_to_f64(self.get(i)?)
    }

    fn point(&self, i: usize) -> Option<CGPoint> {
        cg_point(self.get(i)?)
    }

    fn size(&self, i: usize) -> Option<CGSize> {
        cg_size(self.get(i)?)
    }

    fn rect(&self) -> Option<Rect> {
        let p = self.point(POSITION)?;
        let s = self.size(SIZE)?;
        Some(to_rect(p.x, p.y, s.width, s.height))
    }

    fn children(&self) -> Vec<AXUIElement> {
        self.get(CHILDREN).map(ax_elements).unwrap_or_default()
    }

    fn child_count(&self) -> usize {
        self.get(CHILDREN)
            .and_then(|v| v.downcast::<CFArray>())
            .and_then(|a| usize::try_from(a.len()).ok())
            .unwrap_or(0)
    }
}

// ── Role mapping & element semantics ──────────────────────────────────────────

/// AX role/subrole → [`ElementType`].
///
/// `AXMenuButton` (a button whose press opens a menu) maps to `Button` rather
/// than `SplitButton`: its primary action is a press, it is found with
/// `type=Button`, and it additionally advertises "expand".
/// Unrecognised non-empty roles map to `Custom`; the raw role is kept in
/// `class_name` as `"AXRole"` or `"AXRole:AXSubrole"`.
fn map_role(role: &str, subrole: &str) -> ElementType {
    use ElementType as T;
    match subrole {
        "AXSwitch" | "AXToggle" => return T::CheckBox,
        "AXTabButton" => return T::TabItem,
        "AXOutlineRow" => return T::TreeItem,
        "AXSortButton" => return T::HeaderItem,
        "AXDialog" | "AXSystemDialog" | "AXApplicationDialog" | "AXApplicationAlertDialog" => {
            return T::Dialog
        }
        _ => {}
    }
    match role {
        "AXWindow" => T::Window,
        "AXSheet" | "AXDialog" => T::Dialog,
        "AXApplication" | "AXScrollArea" | "AXDrawer" | "AXPopover" => T::Pane,
        "AXButton" | "AXMenuButton" | "AXDisclosureTriangle" | "AXColorWell" | "AXDockItem" => {
            T::Button
        }
        "AXPopUpButton" | "AXComboBox" => T::ComboBox,
        "AXTextField" | "AXTextArea" | "AXSecureTextField" | "AXSearchField" => T::Edit,
        "AXDateField" | "AXTimeField" => T::Calendar,
        "AXStaticText" | "AXHeading" | "AXListMarker" => T::Text,
        "AXCheckBox" => T::CheckBox,
        "AXRadioButton" => T::RadioButton,
        "AXList" => T::ListBox,
        "AXRow" => T::ListItem,
        "AXCell" => T::DataItem,
        "AXOutline" | "AXBrowser" => T::TreeView,
        "AXTable" => T::Table,
        "AXGrid" => T::DataGrid,
        "AXMenu" => T::Menu,
        "AXMenuBar" => T::MenuBar,
        "AXMenuItem" | "AXMenuBarItem" => T::MenuItem,
        "AXTabGroup" => T::TabControl,
        "AXToolbar" => T::ToolBar,
        "AXScrollBar" => T::ScrollBar,
        "AXValueIndicator" | "AXHandle" | "AXGrowArea" => T::Thumb,
        "AXSlider" => T::Slider,
        "AXIncrementor" | "AXStepper" => T::Spinner,
        "AXProgressIndicator" | "AXBusyIndicator" | "AXLevelIndicator" | "AXRelevanceIndicator" => {
            T::ProgressBar
        }
        "AXImage" => T::Image,
        "AXLink" => T::Link,
        "AXGroup" | "AXRadioGroup" | "AXSplitGroup" | "AXLayoutArea" | "AXLayoutItem"
        | "AXMatte" => T::Group,
        "AXSplitter" => T::Separator,
        "AXWebArea" => T::Document,
        "AXHelpTag" => T::ToolTip,
        "" | "AXUnknown" => T::Unknown,
        _ => T::Custom,
    }
}

fn is_text_role(role: &str, subrole: &str) -> bool {
    matches!(
        role,
        "AXTextField" | "AXTextArea" | "AXSecureTextField" | "AXSearchField" | "AXComboBox"
    ) || matches!(subrole, "AXSearchField" | "AXSecureTextField")
}

fn is_range_role(role: &str) -> bool {
    matches!(
        role,
        "AXSlider" | "AXIncrementor" | "AXScrollBar" | "AXLevelIndicator"
    )
}

/// Elements whose "expand" opens a menu (menu bar items, submenu items, pop-ups).
fn is_menu_host(role: &str, attrs: &Attrs) -> bool {
    match role {
        "AXMenuBarItem" | "AXPopUpButton" | "AXMenuButton" | "AXComboBox" => true,
        // A menu item hosts a menu only when it has a submenu.
        "AXMenuItem" => attrs.child_count() > 0,
        _ => false,
    }
}

/// Accessible name: title, description, the text of a static text, or placeholder.
fn label_of(role: &str, a: &Attrs) -> String {
    a.text(TITLE)
        .or_else(|| a.text(DESCRIPTION))
        .or_else(|| {
            if role == "AXStaticText" {
                a.text(VALUE)
            } else {
                None
            }
        })
        .or_else(|| a.text(PLACEHOLDER))
        .unwrap_or_default()
}

fn toggle_state(value: f64) -> ToggleState {
    match value.round() as i64 {
        0 => ToggleState::Off,
        1 => ToggleState::On,
        _ => ToggleState::Indeterminate,
    }
}

fn expand_state(role: &str, a: &Attrs) -> Option<ExpandState> {
    let open = match a.flag(EXPANDED).or_else(|| a.flag(DISCLOSING)) {
        Some(open) => open,
        None if role == "AXDisclosureTriangle" => a.number(VALUE)? >= 0.5,
        None => return None,
    };
    Some(if open {
        ExpandState::Expanded
    } else {
        ExpandState::Collapsed
    })
}

fn range_info(et: ElementType, a: &Attrs, value_settable: bool) -> Option<RangeInfo> {
    if !matches!(
        et,
        ElementType::Slider
            | ElementType::Spinner
            | ElementType::ProgressBar
            | ElementType::ScrollBar
    ) {
        return None;
    }
    let value = a.number(VALUE)?;
    let (minimum, maximum) = match (a.number(MIN_VALUE), a.number(MAX_VALUE)) {
        (Some(min), Some(max)) => (min, max),
        // Scroll bars report a 0…1 position without explicit bounds.
        _ if et == ElementType::ScrollBar => (0.0, 1.0),
        _ => return None,
    };
    Some(RangeInfo {
        value,
        minimum,
        maximum,
        step: a.number(VALUE_INCREMENT).unwrap_or(0.0),
        read_only: !value_settable,
    })
}

// ── Tree walking state ────────────────────────────────────────────────────────

/// Elements already visited during one walk (guards against cycles in buggy AX trees).
#[derive(Default)]
struct Visited(HashMap<usize, Vec<AXUIElement>>);

impl Visited {
    /// `true` if `el` had not been seen yet.
    fn insert(&mut self, el: &AXUIElement) -> bool {
        let bucket = self.0.entry(cf_hash(el)).or_default();
        if bucket.iter().any(|seen| seen == el) {
            return false;
        }
        bucket.push(el.clone());
        true
    }
}

struct Walk {
    pid: i32,
    visited: Visited,
    timeouts: u32,
}

impl Walk {
    fn new(pid: i32, root: &AXUIElement) -> Self {
        let mut visited = Visited::default();
        visited.insert(root);
        Self {
            pid,
            visited,
            timeouts: 0,
        }
    }

    fn stalled(&self) -> bool {
        self.timeouts >= MAX_WALK_TIMEOUTS
    }

    /// Fetch a descendant's attributes. Elements that vanished are skipped;
    /// repeated timeouts stop the walk.
    fn fetch(&mut self, el: &AXUIElement, set: AttrSet) -> Option<Attrs> {
        if self.stalled() {
            return None;
        }
        match Attrs::fetch(el, set) {
            Ok(attrs) => {
                self.timeouts = 0;
                Some(attrs)
            }
            Err(code) => {
                if code == ax::kAXErrorCannotComplete {
                    self.timeouts += 1;
                    if self.stalled() {
                        tracing::warn!(
                            "PID {} stopped answering accessibility requests; returning a partial result",
                            self.pid
                        );
                    }
                }
                None
            }
        }
    }
}

struct Filter {
    /// Lower-cased query.
    query: Option<String>,
    element_type: Option<ElementType>,
    interactive_only: bool,
}

impl Filter {
    fn new(
        query: Option<&str>,
        element_type: Option<&ElementType>,
        interactive_only: bool,
    ) -> Self {
        Self {
            query: query.filter(|q| !q.is_empty()).map(str::to_lowercase),
            element_type: element_type.copied(),
            interactive_only,
        }
    }

    fn matches(&self, et: ElementType, label: &str, automation_id: Option<&str>) -> bool {
        if self.element_type.is_some_and(|wanted| wanted != et) {
            return false;
        }
        match &self.query {
            None => true,
            Some(q) => {
                label.to_lowercase().contains(q.as_str())
                    || automation_id.is_some_and(|a| a.to_lowercase().contains(q.as_str()))
            }
        }
    }
}

struct Search {
    walk: Walk,
    filter: Filter,
    results: Vec<UiElement>,
}

// ── Window list helpers ───────────────────────────────────────────────────────

/// `kCGWindow*` dictionary keys.
struct WindowKeys {
    number: CFStringRef,
    pid: CFStringRef,
    layer: CFStringRef,
    owner: CFStringRef,
    name: CFStringRef,
    bounds: CFStringRef,
}

impl WindowKeys {
    fn get() -> Self {
        // SAFETY: immutable CFString constants exported by CoreGraphics.
        unsafe {
            Self {
                number: cgw::kCGWindowNumber,
                pid: cgw::kCGWindowOwnerPID,
                layer: cgw::kCGWindowLayer,
                owner: cgw::kCGWindowOwnerName,
                name: cgw::kCGWindowName,
                bounds: cgw::kCGWindowBounds,
            }
        }
    }
}

/// The dictionaries in a `CGWindowListCopyWindowInfo` result.
fn window_dicts(array: &CFArray) -> Vec<CFDictionary> {
    let dict_type = unsafe { CFDictionaryGetTypeID() };
    array
        .iter()
        .filter_map(|item| {
            let p: *const c_void = *item;
            // SAFETY: `p` is owned by `array` and type-checked before wrapping.
            unsafe {
                if p.is_null() || CFGetTypeID(p) != dict_type {
                    return None;
                }
                Some(CFDictionary::wrap_under_get_rule(p as CFDictionaryRef))
            }
        })
        .collect()
}

fn dict_value(dict: &CFDictionary, key: CFStringRef) -> Option<CFType> {
    let item = dict.find(key as *const c_void)?;
    let p: *const c_void = *item;
    if p.is_null() {
        return None;
    }
    // SAFETY: `p` is a live CF object owned by `dict`; we retain it.
    Some(unsafe { CFType::wrap_under_get_rule(p) })
}

fn dict_i64(dict: &CFDictionary, key: CFStringRef) -> Option<i64> {
    dict_value(dict, key)?.downcast::<CFNumber>()?.to_i64()
}

fn dict_string(dict: &CFDictionary, key: CFStringRef) -> Option<String> {
    dict_value(dict, key)?
        .downcast::<CFString>()
        .map(|s| s.to_string())
}

fn dict_rect(dict: &CFDictionary, key: CFStringRef) -> Option<CGRect> {
    let bounds = dict_value(dict, key)?.downcast::<CFDictionary>()?;
    CGRect::from_dict_representation(&bounds)
}

fn frame_matches(pos: CGPoint, size: CGSize, bounds: &CGRect) -> bool {
    let near = |a: f64, b: f64| (a - b).abs() <= FRAME_TOLERANCE;
    near(pos.x, bounds.origin.x)
        && near(pos.y, bounds.origin.y)
        && near(size.width, bounds.size.width)
        && near(size.height, bounds.size.height)
}

/// The app's main window, else its focused window, else its first window.
fn main_window(app: &AXUIElement) -> Result<Option<AXUIElement>, AXError> {
    for name in ["AXMainWindow", "AXFocusedWindow"] {
        match get_attr(app, name) {
            Ok(v) => {
                if let Some(window) = v.downcast::<AXUIElement>() {
                    return Ok(Some(window));
                }
            }
            Err(code) if is_fatal(code) => return Err(code),
            Err(_) => {}
        }
    }
    match get_attr(app, "AXWindows") {
        Ok(v) => Ok(ax_elements(&v).into_iter().next()),
        Err(code) if is_fatal(code) => Err(code),
        Err(_) => Ok(None),
    }
}

// ── Backend ───────────────────────────────────────────────────────────────────

pub struct MacOsUiBackend {
    registry: ElementRegistry<SafeElement>,
    /// PIDs that accepted `AXManualAccessibility` (Electron / Chromium apps).
    manual_ax: DashSet<i32>,
}

impl MacOsUiBackend {
    pub fn new() -> Result<Self> {
        set_global_messaging_timeout();
        if unsafe { ax::AXIsProcessTrusted() } {
            TIMEOUT_APPLIED.store(true, Ordering::Relaxed);
        } else {
            tracing::warn!(
                "Accessibility permission not granted. {} and enable OculOS (or the terminal running it).",
                PERMISSION_MSG
            );
        }
        Ok(Self {
            registry: ElementRegistry::new(),
            manual_ax: DashSet::new(),
        })
    }

    // ── Registry ──────────────────────────────────────────────────────────

    /// Stable id for `el`: `stable_id((pid, CFHash))`, re-salted if that id is
    /// already taken by a different element.
    fn register(&self, pid: i32, el: &AXUIElement) -> String {
        let hash = cf_hash(el);
        let mut salt = 0u32;
        loop {
            let id = if salt == 0 {
                registry::stable_id((pid, hash))
            } else {
                registry::stable_id((pid, hash, salt))
            };
            let free = match self.registry.get(&id) {
                Some(existing) => existing.0 == *el,
                None => true,
            };
            if free || salt >= MAX_ID_PROBES {
                self.registry.insert(id.clone(), SafeElement(el.clone()));
                return id;
            }
            salt += 1;
        }
    }

    fn lookup(&self, id: &str) -> Result<AXUIElement> {
        ensure_trusted()?;
        self.registry
            .get(id)
            .map(|e| e.0)
            .ok_or_else(|| error::element_not_found(id))
    }

    fn app_element(pid: u32) -> Result<(i32, AXUIElement)> {
        ensure_trusted()?;
        let pid = i32::try_from(pid)
            .ok()
            .filter(|p| *p > 0)
            .ok_or_else(|| error::not_found(format!("No application with PID {pid}")))?;
        Ok((pid, AXUIElement::application(pid)))
    }

    /// Ask Electron / Chromium apps to expose their full accessibility tree.
    fn enable_manual_accessibility(&self, pid: i32, app: &AXUIElement) {
        if set_attr(app, "AXManualAccessibility", cf_bool(true)).is_ok()
            && self.manual_ax.insert(pid)
        {
            // First time for this app: give it a moment to build the tree.
            thread::sleep(MANUAL_AX_SETTLE);
        }
    }

    /// Resolve a CG window number (the `hwnd` from `list_windows`) to its AX window.
    fn window_for_hwnd(&self, hwnd: usize) -> Result<(i32, AXUIElement)> {
        ensure_trusted()?;
        let missing = || {
            error::not_found(format!(
                "No window with hwnd {hwnd} — call GET /windows for current values"
            ))
        };
        let window_id = u32::try_from(hwnd).map_err(|_| missing())?;
        let keys = WindowKeys::get();
        let info = cgw::copy_window_info(cgw::kCGWindowListOptionIncludingWindow, window_id)
            .and_then(|array| {
                window_dicts(&array)
                    .into_iter()
                    .find(|d| dict_i64(d, keys.number) == Some(i64::from(window_id)))
            })
            .ok_or_else(missing)?;
        let pid = dict_i64(&info, keys.pid)
            .and_then(|p| i32::try_from(p).ok())
            .filter(|p| *p > 0)
            .ok_or_else(missing)?;
        let bounds = dict_rect(&info, keys.bounds).ok_or_else(missing)?;
        let cg_title = dict_string(&info, keys.name).filter(|t| !t.is_empty());

        let app = AXUIElement::application(pid);
        self.enable_manual_accessibility(pid, &app);
        let windows = get_attr(&app, "AXWindows")
            .map_err(|code| ax_error(code, Subject::App(pid), "listing windows"))?;

        let mut fallback = None;
        for window in ax_elements(&windows) {
            let Ok(a) = Attrs::fetch(&window, AttrSet::Frame) else {
                continue;
            };
            let (Some(pos), Some(size)) = (a.point(POSITION), a.size(SIZE)) else {
                continue;
            };
            if !frame_matches(pos, size, &bounds) {
                continue;
            }
            if cg_title.is_some() && a.string(TITLE) == cg_title {
                return Ok((pid, window));
            }
            fallback.get_or_insert(window);
        }
        fallback.map(|w| (pid, w)).ok_or_else(|| {
            error::not_found(format!(
                "Window {hwnd} (PID {pid}) is not exposed through the Accessibility API"
            ))
        })
    }

    // ── Element description ───────────────────────────────────────────────

    /// Build a [`UiElement`] (without children) from fetched attributes.
    fn describe(&self, pid: i32, el: &AXUIElement, a: &Attrs) -> UiElement {
        use ElementType as T;

        let role = a.string(ROLE).unwrap_or_default();
        let subrole = a.string(SUBROLE).unwrap_or_default();
        let et = map_role(&role, &subrole);

        let mut e = UiElement::new(self.register(pid, el), et);
        e.label = label_of(&role, a);
        e.value = a.string(VALUE);
        e.rect = a.rect().unwrap_or_default();
        e.enabled = a.flag(ENABLED).unwrap_or(true);
        e.focused = a.flag(FOCUSED).unwrap_or(false);
        e.automation_id = a.text(IDENTIFIER);
        e.help_text = a.text(HELP);
        e.class_name = match (role.is_empty(), subrole.is_empty()) {
            (true, _) => None,
            (false, true) => Some(role.clone()),
            (false, false) => Some(format!("{role}:{subrole}")),
        };

        let ax_actions = ax_actions(el).unwrap_or_default();
        let has = |name: &str| ax_actions.iter().any(|a| a == name);
        let text_role = is_text_role(&role, &subrole);
        let range_role = is_range_role(&role);
        let menu_host = is_menu_host(&role, a);
        let triangle = role == "AXDisclosureTriangle";

        // Settable checks cost one IPC each, so only ask where they matter.
        let focusable = a.has(FOCUSED) && settable(el, "AXFocused").unwrap_or(false);
        let value_settable = (text_role || range_role || (et == T::CheckBox && !has("AXPress")))
            && settable(el, "AXValue").unwrap_or(false);
        let selected_settable = a.has(SELECTED) && settable(el, "AXSelected").unwrap_or(false);
        let expand_settable = if a.has(EXPANDED) {
            settable(el, "AXExpanded").unwrap_or(false)
        } else if a.has(DISCLOSING) {
            settable(el, "AXDisclosing").unwrap_or(false)
        } else {
            false
        };

        e.is_keyboard_focusable = focusable;
        if et == T::CheckBox {
            e.toggle_state = a.number(VALUE).map(toggle_state);
        }
        e.is_selected = a.flag(SELECTED).or_else(|| {
            if matches!(et, T::RadioButton | T::TabItem) {
                a.number(VALUE).map(|v| v >= 0.5)
            } else {
                None
            }
        });
        e.expand_state = expand_state(&role, a);
        e.range = range_info(et, a, value_settable);

        let mut actions: Vec<&str> = Vec::new();
        if has("AXPress") {
            actions.push("click");
        }
        if text_role && value_settable {
            actions.push("set-text");
        }
        if text_role || focusable {
            actions.push("send-keys");
        }
        if focusable {
            actions.push("focus");
        }
        if et == T::CheckBox && (has("AXPress") || value_settable) {
            actions.push("toggle");
        }
        if expand_settable
            || (triangle && has("AXPress"))
            || (menu_host && (has("AXShowMenu") || has("AXPress")))
        {
            actions.push("expand");
        }
        if expand_settable || (triangle && has("AXPress")) || (menu_host && has("AXCancel")) {
            actions.push("collapse");
        }
        if selected_settable
            || has("AXPick")
            || (matches!(et, T::RadioButton | T::TabItem) && has("AXPress"))
        {
            actions.push("select");
        }
        if range_role && value_settable && a.number(VALUE).is_some() {
            actions.push("set-range");
        }
        if role == "AXScrollArea" {
            actions.push("scroll");
        }
        if has("AXScrollToVisible") {
            actions.push("scroll-into-view");
        }
        e.actions = actions.into_iter().map(String::from).collect();
        e
    }

    // ── Tree & search ─────────────────────────────────────────────────────

    /// Read the root's attributes, turning "no such app/window" into `not_found`.
    fn fetch_root(root: &AXUIElement, set: AttrSet, subject: Subject<'_>) -> Result<Attrs> {
        let attrs = Attrs::fetch(root, set)
            .map_err(|code| ax_error(code, subject, "reading the UI tree"))?;
        if !attrs.has(ROLE) {
            return Err(subject.gone());
        }
        Ok(attrs)
    }

    fn tree(&self, pid: i32, root: &AXUIElement, subject: Subject<'_>) -> Result<UiElement> {
        let attrs = Self::fetch_root(root, AttrSet::All, subject)?;
        let mut walk = Walk::new(pid, root);
        Ok(self.build_tree(&mut walk, root, &attrs, 0))
    }

    fn build_tree(
        &self,
        walk: &mut Walk,
        el: &AXUIElement,
        attrs: &Attrs,
        depth: u32,
    ) -> UiElement {
        let mut node = self.describe(walk.pid, el, attrs);
        for child in attrs.children() {
            if !walk.visited.insert(&child) {
                continue;
            }
            if depth + 1 > MAX_DEPTH {
                let id = self.register(walk.pid, &child);
                node.children.push(UiElement::depth_limit_placeholder(id));
                continue;
            }
            if let Some(child_attrs) = walk.fetch(&child, AttrSet::All) {
                node.children
                    .push(self.build_tree(walk, &child, &child_attrs, depth + 1));
            }
        }
        node
    }

    fn find(
        &self,
        pid: i32,
        root: &AXUIElement,
        subject: Subject<'_>,
        filter: Filter,
    ) -> Result<Vec<UiElement>> {
        let attrs = Self::fetch_root(root, AttrSet::Light, subject)?;
        let mut search = Search {
            walk: Walk::new(pid, root),
            filter,
            results: Vec::new(),
        };
        self.search(&mut search, root, attrs, 0);
        Ok(search.results)
    }

    fn search(&self, s: &mut Search, el: &AXUIElement, mut attrs: Attrs, depth: u32) {
        if s.results.len() >= MAX_RESULTS {
            return;
        }
        let role = attrs.string(ROLE).unwrap_or_default();
        let subrole = attrs.string(SUBROLE).unwrap_or_default();
        let et = map_role(&role, &subrole);
        let label = label_of(&role, &attrs);
        if s.filter
            .matches(et, &label, attrs.text(IDENTIFIER).as_deref())
        {
            // Only matches pay for the remaining attributes, actions and settable checks.
            if let Err(code) = attrs.fill(el, AttrSet::Extra) {
                tracing::trace!("partial attributes for a match: {}", ax_error_name(code));
            }
            let element = self.describe(s.walk.pid, el, &attrs);
            if !s.filter.interactive_only || !element.actions.is_empty() {
                s.results.push(element);
            }
        }
        if depth >= MAX_DEPTH {
            return;
        }
        for child in attrs.children() {
            if s.results.len() >= MAX_RESULTS || s.walk.stalled() {
                return;
            }
            if !s.walk.visited.insert(&child) {
                continue;
            }
            if let Some(child_attrs) = s.walk.fetch(&child, AttrSet::Light) {
                self.search(s, &child, child_attrs, depth + 1);
            }
        }
    }

    // ── Interaction helpers ───────────────────────────────────────────────

    fn set_expanded(&self, id: &str, want: bool) -> Result<()> {
        let what = if want { "expand" } else { "collapse" };
        let subject = Subject::Element(id);
        let err = |code| ax_error(code, subject, what);
        let el = self.lookup(id)?;
        let a = Attrs::fetch(&el, AttrSet::All).map_err(err)?;
        let role = a.string(ROLE).unwrap_or_default();

        // 1. State attributes: combo boxes (AXExpanded), outline rows (AXDisclosing).
        for (slot, name) in [(EXPANDED, "AXExpanded"), (DISCLOSING, "AXDisclosing")] {
            let Some(current) = a.flag(slot) else {
                continue;
            };
            if current == want {
                return Ok(());
            }
            if settable(&el, name).map_err(err)? {
                return set_attr(&el, name, cf_bool(want)).map_err(err);
            }
            break; // state is known but read-only: fall back to actions
        }

        let actions = ax_actions(&el).map_err(err)?;
        let has = |name: &str| actions.iter().any(|a| a == name);

        // 2. Disclosure triangles report their state as AXValue 0/1.
        if role == "AXDisclosureTriangle" && has("AXPress") {
            if a.number(VALUE).is_some_and(|v| (v >= 0.5) == want) {
                return Ok(());
            }
            return perform_action(&el, "AXPress").map_err(err);
        }

        // 3. Menus: open with AXShowMenu / AXPress, close with AXCancel.
        if is_menu_host(&role, &a) {
            if want {
                for action in ["AXShowMenu", "AXPress"] {
                    if has(action) {
                        return perform_action(&el, action).map_err(err);
                    }
                }
            } else {
                if has("AXCancel") {
                    return perform_action(&el, "AXCancel").map_err(err);
                }
                let open_menu = a
                    .children()
                    .into_iter()
                    .find(|c| role_of(c).as_deref() == Some("AXMenu"));
                return match open_menu {
                    Some(menu) => perform_action(&menu, "AXCancel").map_err(err),
                    None => Err(error::unsupported(format!(
                        "Element '{id}' has no open menu to collapse"
                    ))),
                };
            }
        }

        Err(error::unsupported(format!(
            "Element '{id}' cannot be {}",
            if want { "expanded" } else { "collapsed" }
        )))
    }

    /// Activate the element's app, raise its window and focus it before typing.
    /// Fails (without typing) if the app cannot be brought to the front, since
    /// the keys would otherwise land in whatever app is frontmost.
    fn prepare_for_typing(&self, id: &str, el: &AXUIElement) -> Result<()> {
        let is_frontmost = |app: &AXUIElement| {
            get_attr(app, "AXFrontmost")
                .ok()
                .and_then(|v| cf_to_bool(&v))
                .unwrap_or(false)
        };

        let mut pid: ax::pid_t = 0;
        let pid_ok = unsafe { ax::AXUIElementGetPid(el.as_concrete_TypeRef(), &mut pid) }
            == ax::kAXErrorSuccess;
        if pid_ok && pid > 0 {
            let app = AXUIElement::application(pid);
            if !is_frontmost(&app) {
                if let Err(code) = set_attr(&app, "AXFrontmost", cf_bool(true)) {
                    if is_fatal(code) {
                        return Err(ax_error(code, Subject::Element(id), "send-keys"));
                    }
                    tracing::debug!("AXFrontmost failed for PID {pid}: {}", ax_error_name(code));
                }
                let activated = (0..ACTIVATE_ATTEMPTS).any(|_| {
                    thread::sleep(ACTIVATE_POLL);
                    is_frontmost(&app)
                });
                if !activated {
                    return Err(anyhow!(
                        "Could not bring the application of element '{id}' (PID {pid}) to the front; no keys were sent"
                    ));
                }
            }
        }

        if let Some(window) = get_attr(el, "AXWindow")
            .ok()
            .and_then(|v| v.downcast::<AXUIElement>())
        {
            let _ = perform(&window, "AXRaise");
        }

        match set_attr(el, "AXFocused", cf_bool(true)) {
            Ok(()) => {}
            Err(code) if is_fatal(code) => {
                return Err(ax_error(code, Subject::Element(id), "send-keys"))
            }
            // Not every element accepts focus; keys then go to the app's focused element.
            Err(code) => tracing::debug!(
                "element '{id}' did not take focus ({}); typing into the focused element",
                ax_error_name(code)
            ),
        }
        thread::sleep(FOCUS_SETTLE);
        Ok(())
    }
}

// ── UiBackend implementation ──────────────────────────────────────────────────

impl UiBackend for MacOsUiBackend {
    fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        let options =
            cgw::kCGWindowListOptionOnScreenOnly | cgw::kCGWindowListExcludeDesktopElements;
        let array = cgw::copy_window_info(options, cgw::kCGNullWindowID)
            .ok_or_else(|| anyhow!("CGWindowListCopyWindowInfo returned no data"))?;
        let keys = WindowKeys::get();

        let mut windows = Vec::new();
        for dict in window_dicts(&array) {
            let Some(pid) = dict_i64(&dict, keys.pid).and_then(|p| u32::try_from(p).ok()) else {
                continue;
            };
            // Only normal application windows (layer 0).
            if pid == 0 || dict_i64(&dict, keys.layer).unwrap_or(0) != 0 {
                continue;
            }
            let owner = dict_string(&dict, keys.owner).unwrap_or_default();
            if owner.is_empty() {
                continue;
            }
            // Window titles need the Screen Recording permission; fall back to the app name.
            let title = dict_string(&dict, keys.name)
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| owner.clone());
            let hwnd = dict_i64(&dict, keys.number)
                .and_then(|n| usize::try_from(n).ok())
                .unwrap_or(0);
            let rect = dict_rect(&dict, keys.bounds)
                .map(|r| to_rect(r.origin.x, r.origin.y, r.size.width, r.size.height))
                .unwrap_or_default();
            windows.push(WindowInfo {
                pid,
                hwnd,
                title,
                exe_name: owner,
                rect,
                visible: true,
            });
        }
        Ok(windows)
    }

    fn get_ui_tree(&self, pid: u32) -> Result<UiElement> {
        let (pid, app) = Self::app_element(pid)?;
        self.enable_manual_accessibility(pid, &app);
        self.tree(pid, &app, Subject::App(pid))
    }

    fn get_ui_tree_hwnd(&self, hwnd: usize) -> Result<UiElement> {
        let (pid, window) = self.window_for_hwnd(hwnd)?;
        self.tree(pid, &window, Subject::Window(hwnd))
    }

    fn find_elements(
        &self,
        pid: u32,
        query: Option<&str>,
        element_type: Option<&ElementType>,
        interactive_only: bool,
    ) -> Result<Vec<UiElement>> {
        let (pid, app) = Self::app_element(pid)?;
        self.enable_manual_accessibility(pid, &app);
        let filter = Filter::new(query, element_type, interactive_only);
        self.find(pid, &app, Subject::App(pid), filter)
    }

    fn find_elements_hwnd(
        &self,
        hwnd: usize,
        query: Option<&str>,
        element_type: Option<&ElementType>,
        interactive_only: bool,
    ) -> Result<Vec<UiElement>> {
        let (pid, window) = self.window_for_hwnd(hwnd)?;
        let filter = Filter::new(query, element_type, interactive_only);
        self.find(pid, &window, Subject::Window(hwnd), filter)
    }

    fn click_element(&self, oculos_id: &str) -> Result<()> {
        let el = self.lookup(oculos_id)?;
        perform_action(&el, "AXPress")
            .map_err(|code| ax_error(code, Subject::Element(oculos_id), "click"))
    }

    fn set_text(&self, oculos_id: &str, text: &str) -> Result<()> {
        let el = self.lookup(oculos_id)?;
        set_attr(&el, "AXValue", CFString::new(text).into_CFType())
            .map_err(|code| ax_error(code, Subject::Element(oculos_id), "set-text"))
    }

    fn send_keys(&self, oculos_id: &str, steps: &[KeyStep]) -> Result<()> {
        let el = self.lookup(oculos_id)?;
        // Validate the whole sequence before anything is posted.
        let plan = plan_keys(steps)?;
        if plan.is_empty() {
            return Ok(());
        }
        self.prepare_for_typing(oculos_id, &el)?;
        post_keys(&plan)
    }

    fn focus_element(&self, oculos_id: &str) -> Result<()> {
        let el = self.lookup(oculos_id)?;
        set_attr(&el, "AXFocused", cf_bool(true))
            .map_err(|code| ax_error(code, Subject::Element(oculos_id), "focus"))
    }

    fn toggle_element(&self, oculos_id: &str) -> Result<()> {
        let subject = Subject::Element(oculos_id);
        let err = |code| ax_error(code, subject, "toggle");
        let el = self.lookup(oculos_id)?;
        if ax_actions(&el).map_err(err)?.iter().any(|a| a == "AXPress") {
            return perform_action(&el, "AXPress").map_err(err);
        }
        if settable(&el, "AXValue").map_err(err)? {
            let current = get_attr(&el, "AXValue")
                .ok()
                .and_then(|v| cf_to_f64(&v))
                .unwrap_or(0.0);
            let next = if current >= 0.5 { 0 } else { 1 };
            return set_attr(&el, "AXValue", CFNumber::from(next).into_CFType()).map_err(err);
        }
        Err(error::unsupported(format!(
            "Element '{oculos_id}' cannot be toggled"
        )))
    }

    fn expand_element(&self, oculos_id: &str) -> Result<()> {
        self.set_expanded(oculos_id, true)
    }

    fn collapse_element(&self, oculos_id: &str) -> Result<()> {
        self.set_expanded(oculos_id, false)
    }

    fn select_element(&self, oculos_id: &str) -> Result<()> {
        let subject = Subject::Element(oculos_id);
        let err = |code| ax_error(code, subject, "select");
        let el = self.lookup(oculos_id)?;
        if settable(&el, "AXSelected").map_err(err)? {
            return set_attr(&el, "AXSelected", cf_bool(true)).map_err(err);
        }
        let actions = ax_actions(&el).map_err(err)?;
        for action in ["AXPick", "AXPress"] {
            if actions.iter().any(|a| a == action) {
                return perform_action(&el, action).map_err(err);
            }
        }
        Err(error::unsupported(format!(
            "Element '{oculos_id}' cannot be selected"
        )))
    }

    fn set_range(&self, oculos_id: &str, value: f64) -> Result<()> {
        let subject = Subject::Element(oculos_id);
        let err = |code| ax_error(code, subject, "set-range");
        let el = self.lookup(oculos_id)?;
        let a = Attrs::fetch(&el, AttrSet::Extra).map_err(err)?;
        if let (Some(min), Some(max)) = (a.number(MIN_VALUE), a.number(MAX_VALUE)) {
            if value < min || value > max {
                return Err(error::invalid_input(format!(
                    "Value {value} is outside the range of element '{oculos_id}' ({min} – {max})"
                )));
            }
        }
        set_attr(&el, "AXValue", CFNumber::from(value).into_CFType()).map_err(err)
    }

    fn scroll_element(&self, oculos_id: &str, direction: &str) -> Result<()> {
        let el = self.lookup(oculos_id)?;
        // Wheel deltas in lines: (vertical, horizontal); positive = up / left.
        let (vertical, horizontal) = match direction {
            "up" => (3, 0),
            "down" => (-3, 0),
            "left" => (0, 3),
            "right" => (0, -3),
            "page-up" => (10, 0),
            "page-down" => (-10, 0),
            other => {
                return Err(error::invalid_input(format!(
                    "Unknown scroll direction '{other}'. Use: up, down, left, right, page-up, page-down"
                )))
            }
        };
        let a = Attrs::fetch(&el, AttrSet::Frame)
            .map_err(|code| ax_error(code, Subject::Element(oculos_id), "scroll"))?;
        let frame = match (a.point(POSITION), a.size(SIZE)) {
            (Some(pos), Some(size)) if size.width >= 1.0 && size.height >= 1.0 => (pos, size),
            _ => {
                return Err(error::unsupported(format!(
                    "Element '{oculos_id}' has no on-screen bounds to scroll at"
                )))
            }
        };
        let (pos, size) = frame;
        let centre = CGPoint::new(pos.x + size.width / 2.0, pos.y + size.height / 2.0);
        post_scroll(centre, vertical, horizontal)
    }

    fn scroll_into_view(&self, oculos_id: &str) -> Result<()> {
        let el = self.lookup(oculos_id)?;
        perform_action(&el, "AXScrollToVisible")
            .map_err(|code| ax_error(code, Subject::Element(oculos_id), "scroll-into-view"))
    }

    fn focus_window(&self, pid: u32) -> Result<()> {
        let (pid, app) = Self::app_element(pid)?;
        let err = |code| ax_error(code, Subject::App(pid), "focusing a window");
        let window = main_window(&app)
            .map_err(err)?
            .ok_or_else(|| error::not_found(format!("Application with PID {pid} has no window")))?;

        if get_attr(&window, "AXMinimized")
            .ok()
            .and_then(|v| cf_to_bool(&v))
            == Some(true)
        {
            if let Err(code) = set_attr(&window, "AXMinimized", cf_bool(false)) {
                tracing::debug!("could not un-minimize window: {}", ax_error_name(code));
            }
        }
        set_attr(&app, "AXFrontmost", cf_bool(true)).map_err(err)?;
        let _ = set_attr(&window, "AXMain", cf_bool(true));
        if let Err(code) = perform(&window, "AXRaise") {
            tracing::debug!("AXRaise failed for PID {pid}: {}", ax_error_name(code));
        }
        Ok(())
    }

    fn close_window(&self, pid: u32) -> Result<()> {
        let (pid, app) = Self::app_element(pid)?;
        let err = |code| ax_error(code, Subject::App(pid), "closing a window");
        let window = main_window(&app)
            .map_err(err)?
            .ok_or_else(|| error::not_found(format!("No window found for PID {pid}")))?;
        let button = match get_attr(&window, "AXCloseButton") {
            Ok(v) => v.downcast::<AXUIElement>(),
            Err(code) if is_fatal(code) => return Err(err(code)),
            Err(_) => None,
        }
        .ok_or_else(|| {
            error::unsupported(format!("The window of PID {pid} has no close button"))
        })?;
        if get_attr(&button, "AXEnabled")
            .ok()
            .and_then(|v| cf_to_bool(&v))
            == Some(false)
        {
            return Err(error::unsupported(format!(
                "The close button of PID {pid}'s window is disabled"
            )));
        }
        perform_action(&button, "AXPress").map_err(err)
    }
}

// ── Scrolling ─────────────────────────────────────────────────────────────────

fn cursor_position() -> Option<CGPoint> {
    let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok()?;
    CGEvent::new(source).ok().map(|e| e.location())
}

/// Post a line-based scroll-wheel event at `at` (global display coordinates).
///
/// The event location is set explicitly and the pointer is also warped there,
/// because the window server routes wheel events to the window under the
/// pointer; the pointer is restored afterwards.
fn post_scroll(at: CGPoint, vertical: i32, horizontal: i32) -> Result<()> {
    let saved = cursor_position();
    CGDisplay::warp_mouse_cursor_position(at)
        .map_err(|e| anyhow!("Could not move the pointer over the element (CGError {e})"))?;

    // SAFETY: plain CoreGraphics calls on an event we create, own and release.
    let posted = unsafe {
        let event = CGEventCreateScrollWheelEvent2(
            ptr::null_mut(),
            SCROLL_UNIT_LINE,
            2,
            vertical,
            horizontal,
            0,
        );
        if event.is_null() {
            false
        } else {
            CGEventSetLocation(event, at);
            CGEventPost(HID_EVENT_TAP, event);
            CFRelease(event as CFTypeRef);
            true
        }
    };

    thread::sleep(SCROLL_SETTLE);
    if let Some(original) = saved {
        let _ = CGDisplay::warp_mouse_cursor_position(original);
    }
    if posted {
        Ok(())
    } else {
        Err(anyhow!("Failed to create a scroll-wheel event"))
    }
}

// ── Keyboard ──────────────────────────────────────────────────────────────────

/// A validated keyboard step, ready to be posted.
enum KeyAction {
    /// Unicode text (layout independent).
    Text(String),
    /// Modifier keys pressed in order, then an optional key with its extra flags.
    Chord {
        modifiers: Vec<(CGKeyCode, CGEventFlags)>,
        key: Option<(CGKeyCode, CGEventFlags)>,
    },
}

/// Translate parsed key steps into postable actions, rejecting anything that
/// cannot be typed on macOS before a single event is sent.
fn plan_keys(steps: &[KeyStep]) -> Result<Vec<KeyAction>> {
    let mut plan = Vec::with_capacity(steps.len());
    for step in steps {
        match step {
            KeyStep::Text(text) => {
                if !text.is_empty() {
                    plan.push(KeyAction::Text(text.clone()));
                }
            }
            KeyStep::Chord(chord) => {
                let mut modifiers = chord.modifiers.clone();
                let key = match chord.key {
                    // A bare character is typed as text, whatever the keyboard layout.
                    Some(Key::Char(c)) if modifiers.is_empty() => {
                        plan.push(KeyAction::Text(c.to_string()));
                        continue;
                    }
                    Some(key) => {
                        let (code, flags, needs_shift) = chord_key(key)?;
                        if needs_shift && !modifiers.contains(&Modifier::Shift) {
                            modifiers.push(Modifier::Shift);
                        }
                        Some((code, flags))
                    }
                    None => None,
                };
                plan.push(KeyAction::Chord {
                    modifiers: modifiers.into_iter().map(modifier_key).collect(),
                    key,
                });
            }
        }
    }
    Ok(plan)
}

fn modifier_key(m: Modifier) -> (CGKeyCode, CGEventFlags) {
    match m {
        Modifier::Meta => (0x37, CGEventFlags::CGEventFlagCommand),
        Modifier::Shift => (0x38, CGEventFlags::CGEventFlagShift),
        Modifier::Alt => (0x3A, CGEventFlags::CGEventFlagAlternate),
        Modifier::Ctrl => (0x3B, CGEventFlags::CGEventFlagControl),
    }
}

/// Virtual key code, extra flags (fn / keypad, as real keyboards send them) and
/// whether Shift is needed (for shifted US-ANSI punctuation).
fn chord_key(key: Key) -> Result<(CGKeyCode, CGEventFlags, bool)> {
    let none = CGEventFlags::CGEventFlagNull;
    let func = CGEventFlags::CGEventFlagSecondaryFn;
    let arrow = func | CGEventFlags::CGEventFlagNumericPad;
    let (code, flags) = match key {
        Key::Enter => (0x24, none),
        Key::Tab => (0x30, none),
        Key::Space => (0x31, none),
        Key::Backspace => (0x33, none),
        Key::Escape => (0x35, none),
        Key::CapsLock => (0x39, none),
        Key::Delete => (0x75, func),
        // There is no Insert key on Mac keyboards; Help sits in its place.
        Key::Insert => (0x72, func),
        Key::Home => (0x73, func),
        Key::End => (0x77, func),
        Key::PageUp => (0x74, func),
        Key::PageDown => (0x79, func),
        Key::Left => (0x7B, arrow),
        Key::Right => (0x7C, arrow),
        Key::Down => (0x7D, arrow),
        Key::Up => (0x7E, arrow),
        Key::F(n) => (function_keycode(n)?, func),
        Key::PrintScreen => {
            return Err(error::unsupported(
                "PRINTSCREEN has no equivalent key on macOS",
            ))
        }
        Key::Menu => return Err(error::unsupported("MENU has no equivalent key on macOS")),
        Key::Char(c) => {
            let (code, shift) = ansi_keycode(c).ok_or_else(|| {
                error::invalid_input(format!(
                    "'{c}' cannot be used in a key combination on macOS \
                     (use a US-keyboard letter, digit or punctuation key)"
                ))
            })?;
            return Ok((code, none, shift));
        }
    };
    Ok((code, flags, false))
}

fn function_keycode(n: u8) -> Result<CGKeyCode> {
    const F_KEYS: [CGKeyCode; 20] = [
        0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, // F1–F10
        0x67, 0x6F, 0x69, 0x6B, 0x71, 0x6A, 0x40, 0x4F, 0x50, 0x5A, // F11–F20
    ];
    match n {
        1..=20 => Ok(F_KEYS[usize::from(n) - 1]),
        _ => Err(error::unsupported(format!(
            "F{n} is not available on macOS (F1–F20 only)"
        ))),
    }
}

/// US-ANSI virtual key code for a character, plus whether Shift is required.
fn ansi_keycode(c: char) -> Option<(CGKeyCode, bool)> {
    fn base(c: char) -> Option<CGKeyCode> {
        Some(match c {
            'a' => 0x00,
            's' => 0x01,
            'd' => 0x02,
            'f' => 0x03,
            'h' => 0x04,
            'g' => 0x05,
            'z' => 0x06,
            'x' => 0x07,
            'c' => 0x08,
            'v' => 0x09,
            'b' => 0x0B,
            'q' => 0x0C,
            'w' => 0x0D,
            'e' => 0x0E,
            'r' => 0x0F,
            'y' => 0x10,
            't' => 0x11,
            '1' => 0x12,
            '2' => 0x13,
            '3' => 0x14,
            '4' => 0x15,
            '6' => 0x16,
            '5' => 0x17,
            '=' => 0x18,
            '9' => 0x19,
            '7' => 0x1A,
            '-' => 0x1B,
            '8' => 0x1C,
            '0' => 0x1D,
            ']' => 0x1E,
            'o' => 0x1F,
            'u' => 0x20,
            '[' => 0x21,
            'i' => 0x22,
            'p' => 0x23,
            'l' => 0x25,
            'j' => 0x26,
            '\'' => 0x27,
            'k' => 0x28,
            ';' => 0x29,
            '\\' => 0x2A,
            ',' => 0x2B,
            '/' => 0x2C,
            'n' => 0x2D,
            'm' => 0x2E,
            '.' => 0x2F,
            ' ' => 0x31,
            '`' => 0x32,
            _ => return None,
        })
    }

    if let Some(code) = base(c.to_ascii_lowercase()) {
        return Some((code, c.is_ascii_uppercase()));
    }
    let unshifted = match c {
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        '_' => '-',
        '+' => '=',
        '{' => '[',
        '}' => ']',
        '|' => '\\',
        ':' => ';',
        '"' => '\'',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        '~' => '`',
        _ => return None,
    };
    base(unshifted).map(|code| (code, true))
}

fn post_keys(plan: &[KeyAction]) -> Result<()> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow!("Failed to create a CGEventSource"))?;
    for action in plan {
        match action {
            KeyAction::Text(text) => type_text(&source, text)?,
            KeyAction::Chord { modifiers, key } => press_chord(&source, modifiers, *key)?,
        }
    }
    Ok(())
}

fn post_key(
    source: &CGEventSource,
    code: CGKeyCode,
    down: bool,
    flags: CGEventFlags,
) -> Result<()> {
    let event = CGEvent::new_keyboard_event(source.clone(), code, down)
        .map_err(|_| anyhow!("Failed to create a keyboard event"))?;
    event.set_flags(flags);
    event.post(CGEventTapLocation::HID);
    thread::sleep(KEY_EVENT_DELAY);
    Ok(())
}

/// Press the modifiers in order, press and release the key with all modifier
/// flags set, then release the modifiers in reverse order (ending with no
/// flags). Pressed modifiers are always released, even after an error.
fn press_chord(
    source: &CGEventSource,
    modifiers: &[(CGKeyCode, CGEventFlags)],
    key: Option<(CGKeyCode, CGEventFlags)>,
) -> Result<()> {
    let mut flags = CGEventFlags::CGEventFlagNull;
    let mut pressed = 0;
    let mut result = Ok(());
    for &(code, flag) in modifiers {
        flags |= flag;
        if let Err(e) = post_key(source, code, true, flags) {
            result = Err(e);
            break;
        }
        pressed += 1;
    }
    if result.is_ok() {
        if let Some((code, extra)) = key {
            result = post_key(source, code, true, flags | extra)
                .and_then(|()| post_key(source, code, false, flags | extra));
        }
    }
    for &(code, flag) in modifiers[..pressed].iter().rev() {
        flags.remove(flag);
        let released = post_key(source, code, false, flags);
        if result.is_ok() {
            result = released;
        }
    }
    result
}

/// Type text as Unicode keyboard events, at most 20 UTF-16 units per event
/// (surrogate pairs are never split).
fn type_text(source: &CGEventSource, text: &str) -> Result<()> {
    let mut chunk: Vec<u16> = Vec::with_capacity(MAX_UNICODE_CHUNK);
    let mut buf = [0u16; 2];
    for ch in text.chars() {
        let units = ch.encode_utf16(&mut buf);
        if chunk.len() + units.len() > MAX_UNICODE_CHUNK {
            post_unicode(source, &chunk)?;
            chunk.clear();
        }
        chunk.extend_from_slice(units);
    }
    if !chunk.is_empty() {
        post_unicode(source, &chunk)?;
    }
    Ok(())
}

fn post_unicode(source: &CGEventSource, units: &[u16]) -> Result<()> {
    for down in [true, false] {
        let event = CGEvent::new_keyboard_event(source.clone(), 0, down)
            .map_err(|_| anyhow!("Failed to create a keyboard event"))?;
        event.set_flags(CGEventFlags::CGEventFlagNull);
        event.set_string_from_utf16_unchecked(units);
        event.post(CGEventTapLocation::HID);
        thread::sleep(KEY_EVENT_DELAY);
    }
    Ok(())
}
