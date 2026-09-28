//! Windows backend built on Microsoft UI Automation (UIA).
//!
//! * **Speed** — tree and find requests fetch every property and pattern we
//!   need in one cross-process round trip with an `IUIAutomationCacheRequest`
//!   (`BuildUpdatedCache` / `FindAllBuildCache`) and then read the `Cached*`
//!   getters locally. Interactions use the live (`Current*`) API.
//! * **Stable ids** — `oculos_id`s are derived from the UIA RuntimeId, so the
//!   same element found twice keeps its id.
//! * **Coordinates** — the process opts into per-monitor DPI awareness (v2), so
//!   UIA rectangles, window rectangles, screenshots, mouse input and the
//!   highlight overlay all use physical screen pixels.
//! * **COM** — API calls run on tokio's blocking pool; every public entry point
//!   calls [`ensure_com`] so the current thread is in the multithreaded
//!   apartment.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use windows::core::{Interface, BSTR, VARIANT};
use windows::Win32::Foundation::{
    CloseHandle, BOOL, COLORREF, HANDLE, HWND, LPARAM, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreatePen, DeleteDC, DeleteObject, GetDC,
    GetDIBits, GetStockObject, Rectangle, ReleaseDC, SelectObject, SetROP2, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, NULL_BRUSH, PS_SOLID,
    R2_NOTXORPEN, SRCCOPY,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::Com::{
    CoCreateInstance, CoIncrementMTAUsage, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_MULTITHREADED, SAFEARRAY,
};
use windows::Win32::System::Ole::{
    SafeArrayDestroy, SafeArrayGetDim, SafeArrayGetElement, SafeArrayGetLBound, SafeArrayGetUBound,
};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Variant::VariantToInt32Array;
use windows::Win32::UI::Accessibility::{
    AutomationElementMode_Full, CUIAutomation, CUIAutomation8, ExpandCollapseState,
    ExpandCollapseState_Collapsed, ExpandCollapseState_Expanded,
    ExpandCollapseState_PartiallyExpanded, IUIAutomation, IUIAutomationCacheRequest,
    IUIAutomationCondition, IUIAutomationElement, IUIAutomationExpandCollapsePattern,
    IUIAutomationInvokePattern, IUIAutomationLegacyIAccessiblePattern,
    IUIAutomationRangeValuePattern, IUIAutomationScrollItemPattern, IUIAutomationScrollPattern,
    IUIAutomationSelectionItemPattern, IUIAutomationTextPattern, IUIAutomationTogglePattern,
    IUIAutomationValuePattern, IUIAutomationWindowPattern, PropertyConditionFlags,
    PropertyConditionFlags_IgnoreCase, PropertyConditionFlags_MatchSubstring,
    ScrollAmount_LargeDecrement, ScrollAmount_LargeIncrement, ScrollAmount_NoAmount,
    ScrollAmount_SmallDecrement, ScrollAmount_SmallIncrement, ToggleState_Off, ToggleState_On,
    TreeScope, TreeScope_Element, TreeScope_Subtree, UIA_AcceleratorKeyPropertyId,
    UIA_AccessKeyPropertyId, UIA_AutomationIdPropertyId, UIA_BoundingRectanglePropertyId,
    UIA_ClassNamePropertyId, UIA_ControlTypePropertyId,
    UIA_ExpandCollapseExpandCollapseStatePropertyId, UIA_ExpandCollapsePatternId,
    UIA_HasKeyboardFocusPropertyId, UIA_HelpTextPropertyId, UIA_InvokePatternId,
    UIA_IsEnabledPropertyId, UIA_IsKeyboardFocusablePropertyId, UIA_LegacyIAccessiblePatternId,
    UIA_NamePropertyId, UIA_RangeValueIsReadOnlyPropertyId, UIA_RangeValueMaximumPropertyId,
    UIA_RangeValueMinimumPropertyId, UIA_RangeValuePatternId, UIA_RangeValueSmallChangePropertyId,
    UIA_RangeValueValuePropertyId, UIA_RuntimeIdPropertyId, UIA_ScrollItemPatternId,
    UIA_ScrollPatternId, UIA_SelectionItemIsSelectedPropertyId, UIA_SelectionItemPatternId,
    UIA_TextPatternId, UIA_TogglePatternId, UIA_ToggleToggleStatePropertyId,
    UIA_ValueIsReadOnlyPropertyId, UIA_ValuePatternId, UIA_ValueValuePropertyId,
    UIA_WindowPatternId, UIA_E_ELEMENTNOTAVAILABLE, UIA_E_ELEMENTNOTENABLED,
    UIA_E_INVALIDOPERATION, UIA_E_NOTSUPPORTED, UIA_E_TIMEOUT, UIA_PATTERN_ID, UIA_PROPERTY_ID,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyW, SendInput, VkKeyScanW, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY, VK_APPS, VK_CAPITAL, VK_CONTROL,
    VK_LWIN, VK_MENU, VK_SHIFT, VK_SNAPSHOT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GetCursorPos, GetForegroundWindow, GetSystemMetrics, GetWindow,
    GetWindowLongW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
    IsWindowVisible, PostMessageW, SetCursorPos, SetForegroundWindow, ShowWindow, WindowFromPoint,
    GWL_EXSTYLE, GW_OWNER, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_SWAPBUTTON,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_RESTORE, WHEEL_DELTA, WM_CLOSE, WS_EX_TOOLWINDOW,
};

use crate::{
    error,
    keys::{Chord, Key, KeyStep, Modifier},
    platform::UiBackend,
    registry::{self, ElementRegistry},
    types::{ElementType, ExpandState, RangeInfo, Rect, ToggleState, UiElement, WindowInfo},
};

// ── Tunables ──────────────────────────────────────────────────────────────────

/// Deeper nodes are replaced by [`UiElement::depth_limit_placeholder`].
const MAX_TREE_DEPTH: usize = 48;
/// Maximum number of characters read through the TextPattern per element.
const TEXT_CONTENT_LIMIT: i32 = 4096;
/// Highlight duration cap.
const MAX_HIGHLIGHT_MS: u64 = 5000;
/// Pause between focusing an element and typing into it.
const FOCUS_SETTLE: Duration = Duration::from_millis(50);
/// Pause before the cursor is moved back after synthetic mouse input.
const CURSOR_RESTORE_DELAY: Duration = Duration::from_millis(50);
/// `PW_RENDERFULLCONTENT` — captures DirectComposition / GPU-rendered content.
const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(2);
/// Refuse absurd capture sizes instead of allocating gigabytes.
const MAX_CAPTURE_DIMENSION: i32 = 32_768;

// HRESULTs meaning "the element (or its process/window) is gone".
const RPC_E_DISCONNECTED: u32 = 0x8001_0108;
const RPC_S_SERVER_UNAVAILABLE: u32 = 0x8007_06BA;
const RPC_S_CALL_FAILED_DNE: u32 = 0x8007_06BF;
const HRESULT_INVALID_WINDOW_HANDLE: u32 = 0x8007_0578;
const GONE_HRESULTS: [u32; 5] = [
    UIA_E_ELEMENTNOTAVAILABLE,
    RPC_E_DISCONNECTED,
    RPC_S_SERVER_UNAVAILABLE,
    RPC_S_CALL_FAILED_DNE,
    HRESULT_INVALID_WINDOW_HANDLE,
];

/// Everything read from an element while building trees / find results.
const CACHED_PROPERTIES: &[UIA_PROPERTY_ID] = &[
    UIA_RuntimeIdPropertyId,
    UIA_NamePropertyId,
    UIA_ControlTypePropertyId,
    UIA_AutomationIdPropertyId,
    UIA_ClassNamePropertyId,
    UIA_HelpTextPropertyId,
    UIA_AcceleratorKeyPropertyId,
    UIA_AccessKeyPropertyId,
    UIA_BoundingRectanglePropertyId,
    UIA_IsEnabledPropertyId,
    UIA_HasKeyboardFocusPropertyId,
    UIA_IsKeyboardFocusablePropertyId,
    UIA_ValueValuePropertyId,
    UIA_ValueIsReadOnlyPropertyId,
    UIA_ToggleToggleStatePropertyId,
    UIA_ExpandCollapseExpandCollapseStatePropertyId,
    UIA_SelectionItemIsSelectedPropertyId,
    UIA_RangeValueValuePropertyId,
    UIA_RangeValueMinimumPropertyId,
    UIA_RangeValueMaximumPropertyId,
    UIA_RangeValueSmallChangePropertyId,
    UIA_RangeValueIsReadOnlyPropertyId,
];

const CACHED_PATTERNS: &[UIA_PATTERN_ID] = &[
    UIA_InvokePatternId,
    UIA_ValuePatternId,
    UIA_TogglePatternId,
    UIA_ExpandCollapsePatternId,
    UIA_SelectionItemPatternId,
    UIA_RangeValuePatternId,
    UIA_ScrollPatternId,
    UIA_ScrollItemPatternId,
    UIA_TextPatternId,
];

// ── COM apartment (one per thread) ────────────────────────────────────────────

/// Joins the current thread to the multithreaded apartment for its lifetime.
struct ComApartment {
    initialized: bool,
}

impl ComApartment {
    fn enter() -> Self {
        // S_OK and S_FALSE both succeed and must be balanced by CoUninitialize;
        // RPC_E_CHANGED_MODE (thread is already an STA) fails and must not be.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self {
            initialized: hr.is_ok(),
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { CoUninitialize() };
        }
    }
}

thread_local! {
    static COM_APARTMENT: ComApartment = ComApartment::enter();
}

/// Make sure COM is initialised on the calling (tokio blocking-pool) thread.
fn ensure_com() {
    COM_APARTMENT.with(|_| {});
}

// ── Element registry ──────────────────────────────────────────────────────────

#[derive(Clone)]
struct SafeElement(IUIAutomationElement);

// SAFETY: UIA client objects are free-threaded; they live in the MTA, which is
// kept alive for the whole process (`CoIncrementMTAUsage` in `new`).
unsafe impl Send for SafeElement {}
unsafe impl Sync for SafeElement {}

/// Source of unique ids for the rare element that has no RuntimeId.
static FALLBACK_IDS: AtomicU64 = AtomicU64::new(0);

// ── Backend ───────────────────────────────────────────────────────────────────

pub struct WindowsUiBackend {
    automation: IUIAutomation,
    registry: ElementRegistry<SafeElement>,
}

// SAFETY: see `SafeElement`.
unsafe impl Send for WindowsUiBackend {}
unsafe impl Sync for WindowsUiBackend {}

impl WindowsUiBackend {
    pub fn new() -> Result<Self> {
        unsafe {
            // Physical pixels everywhere. Fails harmlessly when the awareness
            // was already set (e.g. by an application manifest).
            if let Err(e) =
                SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
            {
                tracing::debug!("SetProcessDpiAwarenessContext: {e}");
            }
            // Keep the MTA alive for the process lifetime so UIA objects stay
            // valid even when the thread that created them exits. The cookie
            // is intentionally never released.
            if let Err(e) = CoIncrementMTAUsage() {
                tracing::debug!("CoIncrementMTAUsage: {e}");
            }
        }
        ensure_com();

        // CUIAutomation8 (Windows 8+) adds connection/transaction timeouts, so
        // a hung application cannot block a request forever.
        let automation: IUIAutomation = unsafe {
            match CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) {
                Ok(a) => a,
                Err(_) => CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                    .context("Failed to create the UI Automation client")?,
            }
        };

        Ok(Self {
            automation,
            registry: ElementRegistry::new(),
        })
    }

    // ── Registry ──────────────────────────────────────────────────────────

    fn lookup(&self, oculos_id: &str) -> Result<IUIAutomationElement> {
        self.registry
            .get(oculos_id)
            .map(|e| e.0)
            .ok_or_else(|| error::element_not_found(oculos_id))
    }

    fn register(&self, id: &str, element: &IUIAutomationElement) {
        self.registry
            .insert(id.to_string(), SafeElement(element.clone()));
    }

    // ── Cache requests & conditions ───────────────────────────────────────

    /// A cache request for everything [`cached_node`] reads. `scope` is
    /// `Subtree` for tree walks and `Element` for find results.
    unsafe fn cache_request(&self, scope: TreeScope) -> Result<IUIAutomationCacheRequest> {
        let build = || -> windows::core::Result<IUIAutomationCacheRequest> {
            let request = self.automation.CreateCacheRequest()?;
            request.SetAutomationElementMode(AutomationElementMode_Full)?;
            request.SetTreeFilter(&self.automation.ControlViewCondition()?)?;
            request.SetTreeScope(scope)?;
            for &property in CACHED_PROPERTIES {
                request.AddProperty(property)?;
            }
            for &pattern in CACHED_PATTERNS {
                request.AddPattern(pattern)?;
            }
            Ok(request)
        };
        build().context("Failed to create a UI Automation cache request")
    }

    /// `ControlType == id1 OR ControlType == id2 …`
    unsafe fn control_type_condition(&self, ids: &[i32]) -> Result<IUIAutomationCondition> {
        let mut combined: Option<IUIAutomationCondition> = None;
        for &id in ids {
            let condition = self
                .automation
                .CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(id))?;
            combined = Some(match combined {
                Some(previous) => self.automation.CreateOrCondition(&previous, &condition)?,
                None => condition,
            });
        }
        combined.ok_or_else(|| anyhow!("empty control type list"))
    }

    /// Case-insensitive substring match on Name OR AutomationId, evaluated by
    /// UIA itself. `None` when this Windows version rejects the flags.
    unsafe fn query_condition(&self, query: &str) -> Option<IUIAutomationCondition> {
        let flags = PropertyConditionFlags(
            PropertyConditionFlags_IgnoreCase.0 | PropertyConditionFlags_MatchSubstring.0,
        );
        let value = VARIANT::from(BSTR::from(query));
        let build = || -> windows::core::Result<IUIAutomationCondition> {
            let by_name =
                self.automation
                    .CreatePropertyConditionEx(UIA_NamePropertyId, &value, flags)?;
            let by_id = self.automation.CreatePropertyConditionEx(
                UIA_AutomationIdPropertyId,
                &value,
                flags,
            )?;
            self.automation.CreateOrCondition(&by_name, &by_id)
        };
        match build() {
            Ok(condition) => Some(condition),
            Err(e) => {
                tracing::debug!("substring condition unavailable ({e}); filtering client-side");
                None
            }
        }
    }

    unsafe fn element_from_hwnd(&self, hwnd: HWND) -> Result<IUIAutomationElement> {
        self.automation
            .ElementFromHandle(hwnd)
            .map_err(|e| window_error(hwnd, "access", e))
    }

    // ── Tree / find ───────────────────────────────────────────────────────

    fn tree_for_hwnd(&self, hwnd: HWND) -> Result<UiElement> {
        unsafe {
            let root = self.element_from_hwnd(hwnd)?;
            let request = self.cache_request(TreeScope_Subtree)?;
            let cached = root
                .BuildUpdatedCache(&request)
                .map_err(|e| window_error(hwnd, "read the UI tree of", e))?;
            Ok(self.cached_subtree(&cached, 0))
        }
    }

    /// Convert a cached element and its cached descendants, registering each.
    unsafe fn cached_subtree(&self, element: &IUIAutomationElement, depth: usize) -> UiElement {
        if depth > MAX_TREE_DEPTH {
            let id = element_id(element);
            self.register(&id, element);
            return UiElement::depth_limit_placeholder(id);
        }

        let mut node = cached_node(element);
        self.register(&node.oculos_id, element);

        // No cached children ⇒ error (NULL array) ⇒ leaf.
        if let Ok(children) = element.GetCachedChildren() {
            let count = children.Length().unwrap_or(0);
            node.children.reserve(usize::try_from(count).unwrap_or(0));
            for i in 0..count {
                if let Ok(child) = children.GetElement(i) {
                    node.children.push(self.cached_subtree(&child, depth + 1));
                }
            }
        }
        node
    }

    fn find_in_hwnd(
        &self,
        hwnd: HWND,
        query: Option<&str>,
        element_type: Option<&ElementType>,
        interactive_only: bool,
    ) -> Result<Vec<UiElement>> {
        let query = query.filter(|q| !q.is_empty());
        let wanted = element_type.copied();

        unsafe {
            let root = self.element_from_hwnd(hwnd)?;
            let request = self.cache_request(TreeScope_Element)?;
            let mut condition = self
                .automation
                .ControlViewCondition()
                .context("ControlViewCondition failed")?;

            if let Some(t) = wanted {
                let ids = control_type_ids(t);
                if !ids.is_empty() {
                    let by_type = self.control_type_condition(&ids)?;
                    condition = self.automation.CreateAndCondition(&condition, &by_type)?;
                } else if t != ElementType::Unknown {
                    // UIA has no control type that maps to this ElementType.
                    return Ok(Vec::new());
                }
                // Unknown: filtered client-side below.
            }

            // FindAll* may answer "nothing found" with S_OK and a NULL array,
            // which windows-rs reports as an error carrying a success code.
            let find_all = |condition: &IUIAutomationCondition| match root.FindAllBuildCache(
                TreeScope_Subtree,
                condition,
                &request,
            ) {
                Ok(found) => Ok(Some(found)),
                Err(e) if e.code().is_ok() => Ok(None),
                Err(e) => Err(e),
            };

            let found = match query.and_then(|q| self.query_condition(q)) {
                Some(by_query) => {
                    let full = self.automation.CreateAndCondition(&condition, &by_query)?;
                    match find_all(&full) {
                        Ok(found) => found,
                        Err(e) if is_gone(&e) => return Err(window_error(hwnd, "search", e)),
                        Err(e) => {
                            tracing::debug!("substring search failed ({e}); retrying without it");
                            find_all(&condition).map_err(|e| window_error(hwnd, "search", e))?
                        }
                    }
                }
                None => find_all(&condition).map_err(|e| window_error(hwnd, "search", e))?,
            };
            let Some(found) = found else {
                return Ok(Vec::new());
            };

            let query_lower = query.map(str::to_lowercase);
            let count = found.Length().unwrap_or(0);
            let mut results = Vec::new();
            for i in 0..count {
                let Ok(element) = found.GetElement(i) else {
                    continue;
                };
                // Client-side re-check (also covers the no-substring fallback).
                if let Some(t) = wanted {
                    let ctrl = element.CachedControlType().map(|c| c.0).unwrap_or(0);
                    if control_type(ctrl) != t {
                        continue;
                    }
                }
                if let Some(q) = &query_lower {
                    let name = bstr_opt(element.CachedName()).unwrap_or_default();
                    let aid = bstr_opt(element.CachedAutomationId()).unwrap_or_default();
                    if !name.to_lowercase().contains(q.as_str())
                        && !aid.to_lowercase().contains(q.as_str())
                    {
                        continue;
                    }
                }
                let node = cached_node(&element);
                if interactive_only && node.actions.is_empty() {
                    continue;
                }
                self.register(&node.oculos_id, &element);
                results.push(node);
            }
            Ok(results)
        }
    }
}

// ── Cached element → UiElement ────────────────────────────────────────────────

/// Stable id from the UIA RuntimeId (unique id if the element has none).
unsafe fn element_id(element: &IUIAutomationElement) -> String {
    match runtime_id(element) {
        Some(ints) => registry::stable_id(ints),
        None => registry::stable_id((
            "uia-no-runtime-id",
            FALLBACK_IDS.fetch_add(1, Ordering::Relaxed),
        )),
    }
}

unsafe fn runtime_id(element: &IUIAutomationElement) -> Option<Vec<i32>> {
    // Cached value first (no cross-process call), then the live getter.
    if let Ok(value) = element.GetCachedPropertyValue(UIA_RuntimeIdPropertyId) {
        let mut buf = [0i32; 32];
        let mut count = 0u32;
        if VariantToInt32Array(&value, &mut buf, &mut count).is_ok() {
            let count = (count as usize).min(buf.len());
            if count > 0 {
                return Some(buf[..count].to_vec());
            }
        }
    }
    let array = element.GetRuntimeId().ok()?;
    safearray_to_i32s(array)
}

/// Destroys a SAFEARRAY on drop.
struct SafeArrayGuard(*mut SAFEARRAY);

impl Drop for SafeArrayGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = SafeArrayDestroy(self.0);
            }
        }
    }
}

/// Read a 1-D SAFEARRAY of 4-byte integers and destroy it.
unsafe fn safearray_to_i32s(array: *mut SAFEARRAY) -> Option<Vec<i32>> {
    if array.is_null() {
        return None;
    }
    let _guard = SafeArrayGuard(array);
    if SafeArrayGetDim(array) != 1 || (*array).cbElements != 4 {
        return None;
    }
    let lower = SafeArrayGetLBound(array, 1).ok()?;
    let upper = SafeArrayGetUBound(array, 1).ok()?;
    if upper < lower || upper - lower >= 64 {
        return None;
    }
    let mut out = Vec::with_capacity((upper - lower + 1) as usize);
    for index in lower..=upper {
        let mut value = 0i32;
        SafeArrayGetElement(array, &index, &mut value as *mut i32 as *mut c_void).ok()?;
        out.push(value);
    }
    Some(out)
}

/// Build a `UiElement` (without children) from an element's cached data.
/// Does not register the element.
unsafe fn cached_node(element: &IUIAutomationElement) -> UiElement {
    let ctrl = element.CachedControlType().map(|c| c.0).unwrap_or(0);
    let element_type = control_type(ctrl);
    let mut node = UiElement::new(element_id(element), element_type);

    node.label = bstr_opt(element.CachedName()).unwrap_or_default();
    node.automation_id = bstr_opt(element.CachedAutomationId());
    node.class_name = bstr_opt(element.CachedClassName());
    node.help_text = bstr_opt(element.CachedHelpText());
    node.keyboard_shortcut =
        bstr_opt(element.CachedAcceleratorKey()).or_else(|| bstr_opt(element.CachedAccessKey()));
    node.rect = element
        .CachedBoundingRectangle()
        .map(to_rect)
        .unwrap_or_default();
    node.enabled = element
        .CachedIsEnabled()
        .map(|b| b.as_bool())
        .unwrap_or(true);
    node.focused = element
        .CachedHasKeyboardFocus()
        .map(|b| b.as_bool())
        .unwrap_or(false);
    node.is_keyboard_focusable = element
        .CachedIsKeyboardFocusable()
        .map(|b| b.as_bool())
        .unwrap_or(false);

    let mut actions: Vec<&'static str> = Vec::new();

    // Unsupported pattern ⇒ NULL ⇒ error ⇒ absent.
    if element
        .GetCachedPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
        .is_ok()
    {
        actions.push("click");
    }

    if let Ok(vp) = element.GetCachedPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId) {
        let read_only = vp.CachedIsReadOnly().map(|b| b.as_bool()).unwrap_or(true);
        if !read_only {
            actions.push("set-text");
        }
        node.value = bstr_opt(vp.CachedValue());
    }

    if let Ok(tp) = element.GetCachedPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId) {
        actions.push("toggle");
        node.toggle_state = tp.CachedToggleState().ok().map(|s| {
            if s == ToggleState_On {
                ToggleState::On
            } else if s == ToggleState_Off {
                ToggleState::Off
            } else {
                ToggleState::Indeterminate
            }
        });
    }

    if let Ok(ep) = element
        .GetCachedPatternAs::<IUIAutomationExpandCollapsePattern>(UIA_ExpandCollapsePatternId)
    {
        node.expand_state = ep.CachedExpandCollapseState().ok().map(expand_state);
        match node.expand_state {
            Some(ExpandState::Collapsed) | Some(ExpandState::PartiallyExpanded) => {
                actions.push("expand")
            }
            Some(ExpandState::Expanded) => actions.push("collapse"),
            _ => {}
        }
    }

    if let Ok(sp) =
        element.GetCachedPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
    {
        actions.push("select");
        node.is_selected = sp.CachedIsSelected().ok().map(|b| b.as_bool());
    }

    if let Ok(rp) =
        element.GetCachedPatternAs::<IUIAutomationRangeValuePattern>(UIA_RangeValuePatternId)
    {
        let read_only = rp.CachedIsReadOnly().map(|b| b.as_bool()).unwrap_or(true);
        if !read_only {
            actions.push("set-range");
        }
        node.range = Some(RangeInfo {
            value: rp.CachedValue().unwrap_or(0.0),
            minimum: rp.CachedMinimum().unwrap_or(0.0),
            maximum: rp.CachedMaximum().unwrap_or(100.0),
            step: rp.CachedSmallChange().unwrap_or(1.0),
            read_only,
        });
    }

    if element
        .GetCachedPatternAs::<IUIAutomationScrollPattern>(UIA_ScrollPatternId)
        .is_ok()
    {
        actions.push("scroll");
    }

    if element
        .GetCachedPatternAs::<IUIAutomationScrollItemPattern>(UIA_ScrollItemPatternId)
        .is_ok()
    {
        actions.push("scroll-into-view");
    }

    // Text content: live call, only for elements that expose the TextPattern.
    if let Ok(tp) = element.GetCachedPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId) {
        node.text_content = tp
            .DocumentRange()
            .and_then(|range| range.GetText(TEXT_CONTENT_LIMIT))
            .ok()
            .and_then(|b| non_empty(b.to_string()));
    }

    if node.is_keyboard_focusable {
        actions.push("focus");
        if matches!(
            element_type,
            ElementType::Edit | ElementType::Document | ElementType::Custom
        ) {
            actions.push("send-keys");
        }
    }

    node.actions = actions.into_iter().map(String::from).collect();
    node
}

fn expand_state(s: ExpandCollapseState) -> ExpandState {
    if s == ExpandCollapseState_Collapsed {
        ExpandState::Collapsed
    } else if s == ExpandCollapseState_Expanded {
        ExpandState::Expanded
    } else if s == ExpandCollapseState_PartiallyExpanded {
        ExpandState::PartiallyExpanded
    } else {
        ExpandState::LeafNode
    }
}

fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

fn bstr_opt(value: windows::core::Result<BSTR>) -> Option<String> {
    value.ok().and_then(|b| non_empty(b.to_string()))
}

fn to_rect(r: RECT) -> Rect {
    Rect {
        x: r.left,
        y: r.top,
        width: r.right - r.left,
        height: r.bottom - r.top,
    }
}

// ── Control type mapping ──────────────────────────────────────────────────────

const CONTROL_TYPE_BASE: i32 = 50000;

/// UIA control type ids 50000..=50040, in order.
const CONTROL_TYPES: [ElementType; 41] = [
    ElementType::Button,      // 50000 Button
    ElementType::Calendar,    // 50001 Calendar
    ElementType::CheckBox,    // 50002 CheckBox
    ElementType::ComboBox,    // 50003 ComboBox
    ElementType::Edit,        // 50004 Edit
    ElementType::Link,        // 50005 Hyperlink
    ElementType::Image,       // 50006 Image
    ElementType::ListItem,    // 50007 ListItem
    ElementType::ListBox,     // 50008 List
    ElementType::Menu,        // 50009 Menu
    ElementType::MenuBar,     // 50010 MenuBar
    ElementType::MenuItem,    // 50011 MenuItem
    ElementType::ProgressBar, // 50012 ProgressBar
    ElementType::RadioButton, // 50013 RadioButton
    ElementType::ScrollBar,   // 50014 ScrollBar
    ElementType::Slider,      // 50015 Slider
    ElementType::Spinner,     // 50016 Spinner
    ElementType::StatusBar,   // 50017 StatusBar
    ElementType::TabControl,  // 50018 Tab
    ElementType::TabItem,     // 50019 TabItem
    ElementType::Text,        // 50020 Text
    ElementType::ToolBar,     // 50021 ToolBar
    ElementType::ToolTip,     // 50022 ToolTip
    ElementType::TreeView,    // 50023 Tree
    ElementType::TreeItem,    // 50024 TreeItem
    ElementType::Custom,      // 50025 Custom
    ElementType::Group,       // 50026 Group
    ElementType::Thumb,       // 50027 Thumb
    ElementType::DataGrid,    // 50028 DataGrid
    ElementType::DataItem,    // 50029 DataItem
    ElementType::Document,    // 50030 Document
    ElementType::SplitButton, // 50031 SplitButton
    ElementType::Window,      // 50032 Window
    ElementType::Pane,        // 50033 Pane
    ElementType::Header,      // 50034 Header
    ElementType::HeaderItem,  // 50035 HeaderItem
    ElementType::Table,       // 50036 Table
    ElementType::TitleBar,    // 50037 TitleBar
    ElementType::Separator,   // 50038 Separator
    ElementType::Custom,      // 50039 SemanticZoom
    ElementType::ToolBar,     // 50040 AppBar
];

fn control_type(id: i32) -> ElementType {
    id.checked_sub(CONTROL_TYPE_BASE)
        .and_then(|offset| usize::try_from(offset).ok())
        .and_then(|offset| CONTROL_TYPES.get(offset))
        .copied()
        .unwrap_or(ElementType::Unknown)
}

/// Every UIA control type id that maps to `t` (empty for Unknown / Dialog).
fn control_type_ids(t: ElementType) -> Vec<i32> {
    CONTROL_TYPES
        .iter()
        .zip(CONTROL_TYPE_BASE..)
        .filter(|(mapped, _)| **mapped == t)
        .map(|(_, id)| id)
        .collect()
}

// ── Error mapping ─────────────────────────────────────────────────────────────

fn hresult(e: &windows::core::Error) -> u32 {
    e.code().0 as u32
}

fn is_gone(e: &windows::core::Error) -> bool {
    GONE_HRESULTS.contains(&hresult(e))
}

/// Map a failed UIA call on a registered element to a typed error.
fn uia_error(oculos_id: &str, action: &str, e: windows::core::Error) -> anyhow::Error {
    match hresult(&e) {
        code if GONE_HRESULTS.contains(&code) => error::element_not_found(oculos_id),
        UIA_E_ELEMENTNOTENABLED => error::unsupported(format!(
            "Cannot {action} element '{oculos_id}': it is disabled"
        )),
        UIA_E_NOTSUPPORTED | UIA_E_INVALIDOPERATION => error::unsupported(format!(
            "Cannot {action} element '{oculos_id}': the application rejected the operation ({e})"
        )),
        UIA_E_TIMEOUT => error::timeout(format!(
            "Timed out trying to {action} element '{oculos_id}' — the application may be busy or hung"
        )),
        _ => anyhow!("Failed to {action} element '{oculos_id}': {e}"),
    }
}

/// Map a failed UIA call on a window root to a typed error.
fn window_error(hwnd: HWND, action: &str, e: windows::core::Error) -> anyhow::Error {
    let handle = hwnd.0 as usize;
    if is_gone(&e) || !unsafe { IsWindow(hwnd) }.as_bool() {
        return error::not_found(format!("Window 0x{handle:X} no longer exists"));
    }
    if hresult(&e) == UIA_E_TIMEOUT {
        return error::timeout(format!(
            "Timed out trying to {action} window 0x{handle:X} — the application may be busy or hung"
        ));
    }
    anyhow!("Failed to {action} window 0x{handle:X}: {e}")
}

/// Live pattern lookup: `Ok(None)` when the element does not support it.
unsafe fn optional_pattern<T: Interface>(
    element: &IUIAutomationElement,
    oculos_id: &str,
    pattern: UIA_PATTERN_ID,
) -> Result<Option<T>> {
    match element.GetCurrentPatternAs::<T>(pattern) {
        Ok(p) => Ok(Some(p)),
        Err(e) if is_gone(&e) || hresult(&e) == UIA_E_TIMEOUT => {
            Err(uia_error(oculos_id, "access", e))
        }
        // NULL pattern (S_OK) or any other refusal ⇒ not supported.
        Err(_) => Ok(None),
    }
}

/// Live pattern lookup: `unsupported` error when the element lacks it.
unsafe fn required_pattern<T: Interface>(
    element: &IUIAutomationElement,
    oculos_id: &str,
    pattern: UIA_PATTERN_ID,
    pattern_name: &str,
    action: &str,
) -> Result<T> {
    optional_pattern(element, oculos_id, pattern)?.ok_or_else(|| {
        error::unsupported(format!(
            "Cannot {action} element '{oculos_id}': it does not support the {pattern_name} pattern"
        ))
    })
}

// ── UiBackend implementation ──────────────────────────────────────────────────

impl UiBackend for WindowsUiBackend {
    // ── Discovery ─────────────────────────────────────────────────────────

    fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        ensure_com();
        let mut exe_names: HashMap<u32, String> = HashMap::new();
        let mut result = Vec::new();
        for hwnd in top_level_windows() {
            unsafe {
                if !IsWindowVisible(hwnd).as_bool() {
                    continue;
                }
                let title = window_title(hwnd);
                if title.is_empty() {
                    continue;
                }
                let pid = window_pid(hwnd);
                let exe_name = exe_names
                    .entry(pid)
                    .or_insert_with(|| get_exe_name(pid))
                    .clone();
                result.push(WindowInfo {
                    pid,
                    hwnd: hwnd.0 as usize,
                    title,
                    exe_name,
                    rect: to_rect(window_bounds(hwnd)),
                    visible: true,
                });
            }
        }
        Ok(result)
    }

    fn get_ui_tree(&self, pid: u32) -> Result<UiElement> {
        ensure_com();
        self.tree_for_hwnd(main_window(pid)?)
    }

    fn get_ui_tree_hwnd(&self, hwnd: usize) -> Result<UiElement> {
        ensure_com();
        self.tree_for_hwnd(checked_hwnd(hwnd)?)
    }

    fn find_elements(
        &self,
        pid: u32,
        query: Option<&str>,
        element_type: Option<&ElementType>,
        interactive_only: bool,
    ) -> Result<Vec<UiElement>> {
        ensure_com();
        self.find_in_hwnd(main_window(pid)?, query, element_type, interactive_only)
    }

    fn find_elements_hwnd(
        &self,
        hwnd: usize,
        query: Option<&str>,
        element_type: Option<&ElementType>,
        interactive_only: bool,
    ) -> Result<Vec<UiElement>> {
        ensure_com();
        self.find_in_hwnd(checked_hwnd(hwnd)?, query, element_type, interactive_only)
    }

    // ── Basic interactions ─────────────────────────────────────────────────

    fn click_element(&self, oculos_id: &str) -> Result<()> {
        ensure_com();
        let element = self.lookup(oculos_id)?;
        let id = oculos_id;
        unsafe {
            if let Some(p) =
                optional_pattern::<IUIAutomationInvokePattern>(&element, id, UIA_InvokePatternId)?
            {
                p.Invoke().map_err(|e| uia_error(id, "click", e))?;
                tracing::debug!(element = id, "click via InvokePattern");
                return Ok(());
            }

            if let Some(p) =
                optional_pattern::<IUIAutomationTogglePattern>(&element, id, UIA_TogglePatternId)?
            {
                p.Toggle().map_err(|e| uia_error(id, "click", e))?;
                tracing::debug!(element = id, "click via TogglePattern");
                return Ok(());
            }

            if let Some(p) = optional_pattern::<IUIAutomationSelectionItemPattern>(
                &element,
                id,
                UIA_SelectionItemPatternId,
            )? {
                p.Select().map_err(|e| uia_error(id, "click", e))?;
                tracing::debug!(element = id, "click via SelectionItemPattern");
                return Ok(());
            }

            if let Some(p) = optional_pattern::<IUIAutomationLegacyIAccessiblePattern>(
                &element,
                id,
                UIA_LegacyIAccessiblePatternId,
            )? {
                let has_default_action = p
                    .CurrentDefaultAction()
                    .map(|a| !a.is_empty())
                    .unwrap_or(false);
                if has_default_action {
                    match p.DoDefaultAction() {
                        Ok(()) => {
                            tracing::debug!(element = id, "click via LegacyIAccessible");
                            return Ok(());
                        }
                        Err(e) if is_gone(&e) => return Err(error::element_not_found(id)),
                        Err(e) => {
                            tracing::debug!(element = id, "DoDefaultAction failed: {e}")
                        }
                    }
                }
            }

            // Last resort: a real mouse click at the element's centre.
            let point = mouse_target(&element, id, "click")?;
            mouse_click(point)?;
            tracing::debug!(
                element = id,
                x = point.x,
                y = point.y,
                "click via synthetic mouse input"
            );
        }
        Ok(())
    }

    fn set_text(&self, oculos_id: &str, text: &str) -> Result<()> {
        ensure_com();
        let element = self.lookup(oculos_id)?;
        unsafe {
            let vp: IUIAutomationValuePattern = required_pattern(
                &element,
                oculos_id,
                UIA_ValuePatternId,
                "Value",
                "set text on",
            )?;
            if vp.CurrentIsReadOnly().map(|b| b.as_bool()).unwrap_or(false) {
                return Err(error::unsupported(format!(
                    "Cannot set text on element '{oculos_id}': its value is read-only"
                )));
            }
            vp.SetValue(&BSTR::from(text))
                .map_err(|e| uia_error(oculos_id, "set text on", e))?;
        }
        Ok(())
    }

    fn send_keys(&self, oculos_id: &str, steps: &[KeyStep]) -> Result<()> {
        ensure_com();
        let element = self.lookup(oculos_id)?;
        // Build every event first, so an untypeable key aborts before anything is sent.
        let batches = steps
            .iter()
            .map(key_batch)
            .collect::<Result<Vec<KeyBatch>>>()?;
        unsafe { focus_for_typing(&element, oculos_id)? };
        std::thread::sleep(FOCUS_SETTLE);
        for batch in &batches {
            batch.send()?;
        }
        Ok(())
    }

    fn focus_element(&self, oculos_id: &str) -> Result<()> {
        ensure_com();
        let element = self.lookup(oculos_id)?;
        unsafe { element.SetFocus() }.map_err(|e| uia_error(oculos_id, "focus", e))
    }

    // ── Pattern-specific interactions ──────────────────────────────────────

    fn toggle_element(&self, oculos_id: &str) -> Result<()> {
        ensure_com();
        let element = self.lookup(oculos_id)?;
        unsafe {
            let p: IUIAutomationTogglePattern =
                required_pattern(&element, oculos_id, UIA_TogglePatternId, "Toggle", "toggle")?;
            p.Toggle().map_err(|e| uia_error(oculos_id, "toggle", e))
        }
    }

    fn expand_element(&self, oculos_id: &str) -> Result<()> {
        ensure_com();
        let element = self.lookup(oculos_id)?;
        unsafe {
            let p: IUIAutomationExpandCollapsePattern = required_pattern(
                &element,
                oculos_id,
                UIA_ExpandCollapsePatternId,
                "ExpandCollapse",
                "expand",
            )?;
            p.Expand().map_err(|e| uia_error(oculos_id, "expand", e))
        }
    }

    fn collapse_element(&self, oculos_id: &str) -> Result<()> {
        ensure_com();
        let element = self.lookup(oculos_id)?;
        unsafe {
            let p: IUIAutomationExpandCollapsePattern = required_pattern(
                &element,
                oculos_id,
                UIA_ExpandCollapsePatternId,
                "ExpandCollapse",
                "collapse",
            )?;
            p.Collapse()
                .map_err(|e| uia_error(oculos_id, "collapse", e))
        }
    }

    fn select_element(&self, oculos_id: &str) -> Result<()> {
        ensure_com();
        let element = self.lookup(oculos_id)?;
        unsafe {
            let p: IUIAutomationSelectionItemPattern = required_pattern(
                &element,
                oculos_id,
                UIA_SelectionItemPatternId,
                "SelectionItem",
                "select",
            )?;
            p.Select().map_err(|e| uia_error(oculos_id, "select", e))
        }
    }

    fn set_range(&self, oculos_id: &str, value: f64) -> Result<()> {
        ensure_com();
        let element = self.lookup(oculos_id)?;
        unsafe {
            let p: IUIAutomationRangeValuePattern = required_pattern(
                &element,
                oculos_id,
                UIA_RangeValuePatternId,
                "RangeValue",
                "set the range of",
            )?;
            p.SetValue(value)
                .map_err(|e| uia_error(oculos_id, "set the range of", e))
        }
    }

    fn scroll_element(&self, oculos_id: &str, direction: &str) -> Result<()> {
        ensure_com();
        let element = self.lookup(oculos_id)?;
        let id = oculos_id;
        // (horizontal amount, vertical amount, wheel is horizontal, wheel notches)
        let (h, v, horizontal, notches) = match direction {
            "up" => (ScrollAmount_NoAmount, ScrollAmount_SmallDecrement, false, 1),
            "down" => (ScrollAmount_NoAmount, ScrollAmount_SmallIncrement, false, -1),
            "left" => (ScrollAmount_SmallDecrement, ScrollAmount_NoAmount, true, -1),
            "right" => (ScrollAmount_SmallIncrement, ScrollAmount_NoAmount, true, 1),
            "page-up" => (ScrollAmount_NoAmount, ScrollAmount_LargeDecrement, false, 3),
            "page-down" => (ScrollAmount_NoAmount, ScrollAmount_LargeIncrement, false, -3),
            other => {
                return Err(error::invalid_input(format!(
                    "Unknown scroll direction '{other}'. Use: up, down, left, right, page-up, page-down"
                )))
            }
        };
        unsafe {
            if let Some(sp) =
                optional_pattern::<IUIAutomationScrollPattern>(&element, id, UIA_ScrollPatternId)?
            {
                sp.Scroll(h, v).map_err(|e| uia_error(id, "scroll", e))?;
                return Ok(());
            }
            // No ScrollPattern: mouse wheel over the element.
            let point = mouse_target(&element, id, "scroll")?;
            mouse_wheel(point, horizontal, notches)?;
            tracing::debug!(element = id, direction, "scroll via mouse wheel");
        }
        Ok(())
    }

    fn scroll_into_view(&self, oculos_id: &str) -> Result<()> {
        ensure_com();
        let element = self.lookup(oculos_id)?;
        unsafe {
            let p: IUIAutomationScrollItemPattern = required_pattern(
                &element,
                oculos_id,
                UIA_ScrollItemPatternId,
                "ScrollItem",
                "scroll into view",
            )?;
            p.ScrollIntoView()
                .map_err(|e| uia_error(oculos_id, "scroll into view", e))
        }
    }

    // ── Window operations ──────────────────────────────────────────────────

    fn focus_window(&self, pid: u32) -> Result<()> {
        ensure_com();
        let hwnd = main_window(pid)?;
        unsafe {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            if bring_to_foreground(hwnd, pid) {
                return Ok(());
            }
        }
        Err(error::permission_denied(format!(
            "Windows refused to bring the window of PID {pid} to the foreground \
             (focus-stealing prevention); click the window once or retry"
        )))
    }

    fn close_window(&self, pid: u32) -> Result<()> {
        ensure_com();
        let hwnd = main_window(pid)?;
        unsafe {
            let element = self.element_from_hwnd(hwnd)?;
            match element.GetCurrentPatternAs::<IUIAutomationWindowPattern>(UIA_WindowPatternId) {
                Ok(wp) => return wp.Close().map_err(|e| window_error(hwnd, "close", e)),
                Err(e) if is_gone(&e) => return Err(window_error(hwnd, "close", e)),
                Err(_) => {}
            }
            // No WindowPattern: the classic graceful close.
            PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0))
                .map_err(|e| window_error(hwnd, "close", e))
        }
    }

    // ── Highlight ────────────────────────────────────────────────────────

    fn highlight_element(&self, oculos_id: &str, duration_ms: u64) -> Result<Rect> {
        ensure_com();
        let element = self.lookup(oculos_id)?;
        let bounds = unsafe { element.CurrentBoundingRectangle() }
            .map_err(|e| uia_error(oculos_id, "highlight", e))?;
        let rect = to_rect(bounds);
        if rect.width <= 0 || rect.height <= 0 {
            return Err(error::unsupported(format!(
                "Cannot highlight element '{oculos_id}': it has no on-screen bounds"
            )));
        }
        let duration = duration_ms.min(MAX_HIGHLIGHT_MS);
        // Draw on a background thread so the API response is not blocked.
        std::thread::spawn(move || unsafe { draw_highlight_rect(rect, duration) });
        Ok(rect)
    }

    // ── Screenshots ──────────────────────────────────────────────────────

    fn screenshot_window(&self, pid: u32) -> Result<Vec<u8>> {
        ensure_com();
        let hwnd = main_window(pid)?;
        unsafe { capture_window(hwnd) }
    }

    fn screenshot_element(&self, oculos_id: &str) -> Result<Vec<u8>> {
        ensure_com();
        let element = self.lookup(oculos_id)?;
        unsafe {
            let bounds = element
                .CurrentBoundingRectangle()
                .map_err(|e| uia_error(oculos_id, "screenshot", e))?;
            if bounds.right <= bounds.left || bounds.bottom <= bounds.top {
                return Err(error::unsupported(format!(
                    "Cannot screenshot element '{oculos_id}': it has no on-screen bounds"
                )));
            }
            if element
                .CurrentIsOffscreen()
                .map(|b| b.as_bool())
                .unwrap_or(false)
            {
                return Err(error::unsupported(format!(
                    "Cannot screenshot element '{oculos_id}': it is off-screen (use scroll-into-view first)"
                )));
            }
            let visible = intersect(bounds, virtual_screen_rect()).ok_or_else(|| {
                error::unsupported(format!(
                    "Cannot screenshot element '{oculos_id}': it is outside the visible screen area"
                ))
            })?;
            capture_screen_rect(visible)
        }
    }
}

// ── Windows ───────────────────────────────────────────────────────────────────

unsafe extern "system" fn enum_windows_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let list = &mut *(lparam.0 as *mut Vec<HWND>);
    list.push(hwnd);
    BOOL(1)
}

/// All top-level windows in Z order (topmost first).
fn top_level_windows() -> Vec<HWND> {
    let mut list: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(enum_windows_cb),
            LPARAM(&mut list as *mut Vec<HWND> as isize),
        );
    }
    list
}

fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

/// Window caption. `GetWindowTextW` reads the caption of other processes'
/// windows without sending them a message, so a hung application cannot block
/// us (unlike `GetWindowTextLengthW`).
fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let copied = unsafe { GetWindowTextW(hwnd, &mut buf) };
    let copied = usize::try_from(copied).unwrap_or(0).min(buf.len());
    String::from_utf16_lossy(&buf[..copied])
}

/// Visible window bounds (DWM extended frame, i.e. without the invisible
/// resize borders), falling back to `GetWindowRect`.
unsafe fn window_bounds(hwnd: HWND) -> RECT {
    let mut frame = RECT::default();
    if DwmGetWindowAttribute(
        hwnd,
        DWMWA_EXTENDED_FRAME_BOUNDS,
        &mut frame as *mut RECT as *mut c_void,
        size_of::<RECT>() as u32,
    )
    .is_ok()
        && frame.right > frame.left
        && frame.bottom > frame.top
    {
        return frame;
    }
    let mut rect = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rect);
    rect
}

/// The main window of a process: a visible, titled, un-owned, non-tool window,
/// falling back to the first visible titled one.
fn find_main_window(pid: u32) -> Option<HWND> {
    let mut fallback = None;
    for hwnd in top_level_windows() {
        unsafe {
            if window_pid(hwnd) != pid
                || !IsWindowVisible(hwnd).as_bool()
                || window_title(hwnd).is_empty()
            {
                continue;
            }
            let owned = matches!(GetWindow(hwnd, GW_OWNER), Ok(owner) if !owner.is_invalid());
            let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            let tool_window = ex_style & WS_EX_TOOLWINDOW.0 != 0;
            if !owned && !tool_window {
                return Some(hwnd);
            }
            fallback.get_or_insert(hwnd);
        }
    }
    fallback
}

fn main_window(pid: u32) -> Result<HWND> {
    find_main_window(pid)
        .ok_or_else(|| error::not_found(format!("No visible window found for PID {pid}")))
}

fn checked_hwnd(hwnd: usize) -> Result<HWND> {
    let handle = HWND(hwnd as *mut c_void);
    if hwnd == 0 || !unsafe { IsWindow(handle) }.as_bool() {
        return Err(error::not_found(format!(
            "No window with handle {hwnd} (0x{hwnd:X})"
        )));
    }
    Ok(handle)
}

fn is_foreground(pid: u32) -> bool {
    let foreground = unsafe { GetForegroundWindow() };
    !foreground.is_invalid() && window_pid(foreground) == pid
}

/// Foreground changes are applied asynchronously; poll briefly.
fn wait_foreground(pid: u32) -> bool {
    for _ in 0..5 {
        if is_foreground(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

unsafe fn bring_to_foreground(hwnd: HWND, pid: u32) -> bool {
    let _ = SetForegroundWindow(hwnd);
    if wait_foreground(pid) {
        tracing::debug!(pid, "focus_window via SetForegroundWindow");
        return true;
    }

    // A no-op input event from this process lifts the foreground lock.
    let dummy = mouse_input(0, 0, 0, MOUSE_EVENT_FLAGS(0));
    let _ = SendInput(&[dummy], size_of::<INPUT>() as i32);
    let _ = SetForegroundWindow(hwnd);
    if wait_foreground(pid) {
        tracing::debug!(pid, "focus_window via dummy input + SetForegroundWindow");
        return true;
    }

    // Share the input state of the current foreground thread.
    let foreground = GetForegroundWindow();
    let foreground_thread = if foreground.is_invalid() {
        0
    } else {
        GetWindowThreadProcessId(foreground, None)
    };
    let current_thread = GetCurrentThreadId();
    let attached = foreground_thread != 0
        && foreground_thread != current_thread
        && AttachThreadInput(current_thread, foreground_thread, true).as_bool();
    let _ = BringWindowToTop(hwnd);
    let _ = SetForegroundWindow(hwnd);
    if attached {
        let _ = AttachThreadInput(current_thread, foreground_thread, false);
    }
    let ok = wait_foreground(pid);
    if ok {
        tracing::debug!(pid, "focus_window via AttachThreadInput");
    }
    ok
}

/// Closes a kernel handle on drop.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn get_exe_name(pid: u32) -> String {
    const UNKNOWN: &str = "unknown.exe";
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return UNKNOWN.to_string();
        };
        let handle = OwnedHandle(handle);
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        if QueryFullProcessImageNameW(
            handle.0,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut size,
        )
        .is_err()
        {
            return UNKNOWN.to_string();
        }
        let len = (size as usize).min(buf.len());
        let path = String::from_utf16_lossy(&buf[..len]);
        path.rsplit(['/', '\\'])
            .next()
            .filter(|name| !name.is_empty())
            .unwrap_or(UNKNOWN)
            .to_string()
    }
}

// ── Keyboard input ────────────────────────────────────────────────────────────

/// The events for one `KeyStep`, sent with a single `SendInput` call.
struct KeyBatch {
    inputs: Vec<INPUT>,
    /// Modifiers pressed by this batch (released again if sending fails).
    modifiers: Vec<(u16, bool)>,
}

impl KeyBatch {
    fn send(&self) -> Result<()> {
        let result = send_inputs(&self.inputs);
        if result.is_err() && !self.modifiers.is_empty() {
            // Never leave a modifier stuck down.
            let release: Vec<INPUT> = self
                .modifiers
                .iter()
                .rev()
                .map(|&(vk, extended)| vk_input(vk, extended, true))
                .collect();
            let _ = send_inputs(&release);
        }
        result
    }
}

fn key_batch(step: &KeyStep) -> Result<KeyBatch> {
    match step {
        KeyStep::Text(text) => Ok(KeyBatch {
            inputs: unicode_inputs(text),
            modifiers: Vec::new(),
        }),
        KeyStep::Chord(chord) => chord_batch(chord),
    }
}

fn chord_batch(chord: &Chord) -> Result<KeyBatch> {
    // A bare character key ({PLUS}, {LBRACE}, {a 3}…) means "type this
    // character": send it as Unicode so the layout cannot change it.
    if chord.modifiers.is_empty() {
        if let Some(Key::Char(c)) = chord.key {
            return Ok(KeyBatch {
                inputs: unicode_inputs(&c.to_string()),
                modifiers: Vec::new(),
            });
        }
    }

    let modifiers: Vec<(u16, bool)> = chord.modifiers.iter().map(|&m| modifier_vk(m)).collect();
    let key = chord.key.map(key_vk).transpose()?;

    let mut inputs = Vec::with_capacity(modifiers.len() * 2 + 2);
    for &(vk, extended) in &modifiers {
        inputs.push(vk_input(vk, extended, false));
    }
    if let Some((vk, extended)) = key {
        inputs.push(vk_input(vk, extended, false));
        inputs.push(vk_input(vk, extended, true));
    }
    for &(vk, extended) in modifiers.iter().rev() {
        inputs.push(vk_input(vk, extended, true));
    }
    Ok(KeyBatch { inputs, modifiers })
}

/// (virtual key, needs KEYEVENTF_EXTENDEDKEY)
fn modifier_vk(m: Modifier) -> (u16, bool) {
    match m {
        Modifier::Ctrl => (VK_CONTROL.0, false),
        Modifier::Alt => (VK_MENU.0, false),
        Modifier::Shift => (VK_SHIFT.0, false),
        Modifier::Meta => (VK_LWIN.0, true),
    }
}

/// (virtual key, needs KEYEVENTF_EXTENDEDKEY)
fn key_vk(key: Key) -> Result<(u16, bool)> {
    Ok(match key {
        Key::Enter => (0x0D, false),
        Key::Tab => (0x09, false),
        Key::Escape => (0x1B, false),
        Key::Space => (0x20, false),
        Key::Backspace => (0x08, false),
        Key::Delete => (0x2E, true),
        Key::Insert => (0x2D, true),
        Key::Home => (0x24, true),
        Key::End => (0x23, true),
        Key::PageUp => (0x21, true),
        Key::PageDown => (0x22, true),
        Key::Left => (0x25, true),
        Key::Up => (0x26, true),
        Key::Right => (0x27, true),
        Key::Down => (0x28, true),
        Key::CapsLock => (VK_CAPITAL.0, false),
        Key::PrintScreen => (VK_SNAPSHOT.0, false),
        Key::Menu => (VK_APPS.0, true),
        Key::F(n) if (1..=24).contains(&n) => (0x70 + u16::from(n) - 1, false),
        Key::F(n) => {
            return Err(error::invalid_input(format!(
                "F{n} is not a valid function key (F1–F24)"
            )))
        }
        Key::Char(c) => (char_vk(c)?, false),
    })
}

/// Virtual key for a character on the current keyboard layout.
fn char_vk(c: char) -> Result<u16> {
    let not_on_layout = || {
        error::invalid_input(format!(
            "character {c:?} is not on the current keyboard layout"
        ))
    };
    let mut buf = [0u16; 2];
    let units = c.encode_utf16(&mut buf);
    if units.len() != 1 {
        return Err(not_on_layout());
    }
    let scan = unsafe { VkKeyScanW(units[0]) };
    let vk = (scan as u16) & 0xFF;
    if scan == -1 || vk == 0xFF {
        return Err(not_on_layout());
    }
    Ok(vk)
}

fn vk_input(vk: u16, extended: bool, key_up: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(0);
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if key_up {
        flags |= KEYEVENTF_KEYUP;
    }
    // Hardware scan code as well, for apps that read it (games, remote tools).
    let scan = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC) } as u16;
    keyboard_input(VIRTUAL_KEY(vk), scan, flags)
}

/// KEYEVENTF_UNICODE down/up pairs, one per UTF-16 unit (surrogate pairs
/// are sent as two consecutive units, which Windows recombines).
fn unicode_inputs(text: &str) -> Vec<INPUT> {
    let mut inputs = Vec::with_capacity(text.len() * 2);
    for unit in text.encode_utf16() {
        inputs.push(keyboard_input(VIRTUAL_KEY(0), unit, KEYEVENTF_UNICODE));
        inputs.push(keyboard_input(
            VIRTUAL_KEY(0),
            unit,
            KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
        ));
    }
    inputs
}

fn keyboard_input(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// One `SendInput` call; fails when Windows injected fewer events than given.
fn send_inputs(inputs: &[INPUT]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if (sent as usize) < inputs.len() {
        return Err(error::permission_denied(format!(
            "Input was blocked ({sent} of {} events injected) — the target window may be running \
             elevated (UIPI) or the desktop is locked",
            inputs.len()
        )));
    }
    Ok(())
}

/// Focus the element before typing. Proceeds if it already has focus.
unsafe fn focus_for_typing(element: &IUIAutomationElement, oculos_id: &str) -> Result<()> {
    match element.SetFocus() {
        Ok(()) => Ok(()),
        Err(e) if is_gone(&e) => Err(error::element_not_found(oculos_id)),
        Err(e) => {
            if element
                .CurrentHasKeyboardFocus()
                .map(|b| b.as_bool())
                .unwrap_or(false)
            {
                Ok(())
            } else {
                Err(error::unsupported(format!(
                    "Element '{oculos_id}' cannot receive keyboard focus ({e}); no keys were sent"
                )))
            }
        }
    }
}

// ── Mouse input ───────────────────────────────────────────────────────────────

fn mouse_input(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// The virtual screen (all monitors) in physical pixels.
fn virtual_screen_rect() -> RECT {
    unsafe {
        let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
        RECT {
            left,
            top,
            right: left + GetSystemMetrics(SM_CXVIRTUALSCREEN),
            bottom: top + GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

/// Map a pixel offset to SendInput's 0..=65535 absolute range, aiming at the
/// pixel centre so rounding in either direction lands on the right pixel.
fn normalize_coordinate(offset: i32, extent: i32) -> i32 {
    if extent <= 0 {
        return 0;
    }
    let value = (2 * i64::from(offset) + 1) * 32_768 / i64::from(extent);
    value.clamp(0, 65_535) as i32
}

fn absolute_mouse_position(point: POINT) -> (i32, i32) {
    let screen = virtual_screen_rect();
    (
        normalize_coordinate(point.x - screen.left, screen.right - screen.left),
        normalize_coordinate(point.y - screen.top, screen.bottom - screen.top),
    )
}

/// Centre of the element, provided it is on screen and not covered by another
/// process's window (so synthetic mouse input cannot hit the wrong app).
unsafe fn mouse_target(
    element: &IUIAutomationElement,
    oculos_id: &str,
    action: &str,
) -> Result<POINT> {
    let bounds = element
        .CurrentBoundingRectangle()
        .map_err(|e| uia_error(oculos_id, action, e))?;
    if bounds.right <= bounds.left || bounds.bottom <= bounds.top {
        return Err(error::unsupported(format!(
            "Cannot {action} element '{oculos_id}': it supports no suitable UI Automation pattern \
             and has no on-screen bounds for mouse input"
        )));
    }
    if element
        .CurrentIsOffscreen()
        .map(|b| b.as_bool())
        .unwrap_or(false)
    {
        return Err(error::unsupported(format!(
            "Cannot {action} element '{oculos_id}': it is off-screen (use scroll-into-view first)"
        )));
    }
    let point = POINT {
        x: bounds.left + (bounds.right - bounds.left) / 2,
        y: bounds.top + (bounds.bottom - bounds.top) / 2,
    };
    let pid = element
        .CurrentProcessId()
        .map_err(|e| uia_error(oculos_id, action, e))?;
    let hit = WindowFromPoint(point);
    if hit.is_invalid() || i64::from(window_pid(hit)) != i64::from(pid) {
        return Err(error::unsupported(format!(
            "Cannot {action} element '{oculos_id}' with the mouse: another window covers it at \
             ({}, {}); bring its window to the foreground (POST /windows/{pid}/focus) and retry",
            point.x, point.y
        )));
    }
    Ok(point)
}

/// Run `f`, then put the mouse cursor back where it was.
fn with_cursor_restored(f: impl FnOnce() -> Result<()>) -> Result<()> {
    let mut previous = POINT::default();
    let saved = unsafe { GetCursorPos(&mut previous) }.is_ok();
    let result = f();
    if saved {
        std::thread::sleep(CURSOR_RESTORE_DELAY);
        let _ = unsafe { SetCursorPos(previous.x, previous.y) };
    }
    result
}

fn mouse_click(point: POINT) -> Result<()> {
    // SendInput's "left" is the physical left button; honour swapped buttons.
    let swapped = unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0;
    let (down, up) = if swapped {
        (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP)
    } else {
        (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP)
    };
    let (x, y) = absolute_mouse_position(point);
    let at = MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
    let inputs = [
        mouse_input(x, y, 0, at),
        mouse_input(x, y, 0, at | down),
        mouse_input(x, y, 0, at | up),
    ];
    with_cursor_restored(|| send_inputs(&inputs))
}

/// `notches` wheel clicks (positive = up / right) over `point`.
fn mouse_wheel(point: POINT, horizontal: bool, notches: i32) -> Result<()> {
    let (x, y) = absolute_mouse_position(point);
    let flags = if horizontal {
        MOUSEEVENTF_HWHEEL
    } else {
        MOUSEEVENTF_WHEEL
    };
    let delta = WHEEL_DELTA as i32 * notches.signum();
    let mut inputs = vec![mouse_input(
        x,
        y,
        0,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    )];
    for _ in 0..notches.unsigned_abs() {
        // mouseData carries a signed delta in a DWORD.
        inputs.push(mouse_input(0, 0, delta as u32, flags));
    }
    with_cursor_restored(|| send_inputs(&inputs))
}

// ── GDI helpers (RAII) ────────────────────────────────────────────────────────

/// The whole-screen DC, released on drop.
struct ScreenDc(HDC);

impl ScreenDc {
    fn get() -> Result<Self> {
        let hdc = unsafe { GetDC(HWND::default()) };
        if hdc.is_invalid() {
            return Err(anyhow!("GetDC failed for the screen"));
        }
        Ok(Self(hdc))
    }
}

impl Drop for ScreenDc {
    fn drop(&mut self) {
        unsafe { ReleaseDC(HWND::default(), self.0) };
    }
}

/// A memory DC, deleted on drop.
struct MemoryDc(HDC);

impl MemoryDc {
    fn compatible_with(hdc: HDC) -> Result<Self> {
        let mem = unsafe { CreateCompatibleDC(hdc) };
        if mem.is_invalid() {
            return Err(anyhow!("CreateCompatibleDC failed"));
        }
        Ok(Self(mem))
    }
}

impl Drop for MemoryDc {
    fn drop(&mut self) {
        let _ = unsafe { DeleteDC(self.0) };
    }
}

/// A GDI object (bitmap, pen…), deleted on drop.
struct GdiObject(HGDIOBJ);

impl Drop for GdiObject {
    fn drop(&mut self) {
        let _ = unsafe { DeleteObject(self.0) };
    }
}

/// Selects an object into a DC and restores the previous one on drop.
struct Selection {
    hdc: HDC,
    previous: HGDIOBJ,
}

impl Selection {
    fn new(hdc: HDC, object: HGDIOBJ) -> Self {
        let previous = unsafe { SelectObject(hdc, object) };
        Self { hdc, previous }
    }
}

impl Drop for Selection {
    fn drop(&mut self) {
        if !self.previous.is_invalid() {
            unsafe { SelectObject(self.hdc, self.previous) };
        }
    }
}

// ── Screenshots ───────────────────────────────────────────────────────────────

fn intersect(a: RECT, b: RECT) -> Option<RECT> {
    let r = RECT {
        left: a.left.max(b.left),
        top: a.top.max(b.top),
        right: a.right.min(b.right),
        bottom: a.bottom.min(b.bottom),
    };
    (r.right > r.left && r.bottom > r.top).then_some(r)
}

fn check_capture_size(width: i32, height: i32) -> Result<()> {
    if width <= 0 || height <= 0 {
        return Err(error::unsupported("Nothing to capture: the area is empty"));
    }
    if width > MAX_CAPTURE_DIMENSION || height > MAX_CAPTURE_DIMENSION {
        return Err(error::unsupported(format!(
            "Capture area {width}x{height} is too large"
        )));
    }
    Ok(())
}

fn new_bitmap(hdc: HDC, width: i32, height: i32) -> Result<(HBITMAP, GdiObject)> {
    let bitmap = unsafe { CreateCompatibleBitmap(hdc, width, height) };
    if bitmap.is_invalid() {
        return Err(anyhow!("CreateCompatibleBitmap({width}x{height}) failed"));
    }
    Ok((bitmap, GdiObject(bitmap.into())))
}

/// Read a bitmap (not selected into any DC) as top-down 32-bit BGRA.
unsafe fn read_bitmap(hdc: HDC, bitmap: HBITMAP, width: i32, height: i32) -> Result<Vec<u8>> {
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height, // negative = top-down rows
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pixels = vec![0u8; width as usize * height as usize * 4];
    let lines = GetDIBits(
        hdc,
        bitmap,
        0,
        height as u32,
        Some(pixels.as_mut_ptr() as *mut c_void),
        &mut info,
        DIB_RGB_COLORS,
    );
    if lines != height {
        return Err(anyhow!("GetDIBits failed ({lines} of {height} rows)"));
    }
    Ok(pixels)
}

/// BGRA (alpha ignored) → opaque RGBA PNG.
fn encode_png(width: u32, height: u32, mut bgra: Vec<u8>) -> Result<Vec<u8>> {
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
        // GDI leaves alpha at 0, which would make the PNG fully transparent.
        px[3] = 255;
    }
    let image = image::RgbaImage::from_raw(width, height, bgra)
        .ok_or_else(|| anyhow!("Pixel buffer does not match {width}x{height}"))?;
    let mut png = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut png, image::ImageFormat::Png)
        .context("Failed to encode PNG")?;
    Ok(png.into_inner())
}

/// Copy a screen region (physical pixels) into a PNG.
unsafe fn capture_screen_rect(area: RECT) -> Result<Vec<u8>> {
    let width = area.right - area.left;
    let height = area.bottom - area.top;
    check_capture_size(width, height)?;

    let screen = ScreenDc::get()?;
    let memory = MemoryDc::compatible_with(screen.0)?;
    let (bitmap, _bitmap_guard) = new_bitmap(screen.0, width, height)?;
    {
        let _selected = Selection::new(memory.0, bitmap.into());
        BitBlt(
            memory.0, 0, 0, width, height, screen.0, area.left, area.top, SRCCOPY,
        )
        .context("BitBlt from the screen failed")?;
    }
    let pixels = read_bitmap(memory.0, bitmap, width, height)?;
    encode_png(width as u32, height as u32, pixels)
}

/// Render a whole window (including covered parts) with `PrintWindow`.
/// Returns BGRA pixels of the full `GetWindowRect` area, or `None` if the
/// window refused to render.
unsafe fn print_window(hwnd: HWND, width: i32, height: i32) -> Result<Option<Vec<u8>>> {
    let screen = ScreenDc::get()?;
    let memory = MemoryDc::compatible_with(screen.0)?;
    let (bitmap, _bitmap_guard) = new_bitmap(screen.0, width, height)?;
    let printed = {
        let _selected = Selection::new(memory.0, bitmap.into());
        PrintWindow(hwnd, memory.0, PW_RENDERFULLCONTENT).as_bool()
    };
    if !printed {
        return Ok(None);
    }
    read_bitmap(memory.0, bitmap, width, height).map(Some)
}

unsafe fn capture_window(hwnd: HWND) -> Result<Vec<u8>> {
    if IsIconic(hwnd).as_bool() {
        return Err(error::unsupported(
            "Cannot screenshot a minimised window; focus it first (POST /windows/{pid}/focus)",
        ));
    }
    let mut window = RECT::default();
    GetWindowRect(hwnd, &mut window).map_err(|e| window_error(hwnd, "measure", e))?;
    // The visible frame, without the invisible resize borders.
    let frame = intersect(window_bounds(hwnd), window)
        .ok_or_else(|| error::unsupported("The window has no visible area"))?;

    let full_width = window.right - window.left;
    let full_height = window.bottom - window.top;
    check_capture_size(full_width, full_height)?;

    if let Some(full) = print_window(hwnd, full_width, full_height)? {
        let (x0, y0) = (frame.left - window.left, frame.top - window.top);
        let (width, height) = (frame.right - frame.left, frame.bottom - frame.top);
        let row_bytes = width as usize * 4;
        let mut cropped = Vec::with_capacity(row_bytes * height as usize);
        for row in y0..y0 + height {
            let start = (row as usize * full_width as usize + x0 as usize) * 4;
            let line = full
                .get(start..start + row_bytes)
                .ok_or_else(|| anyhow!("window frame lies outside the rendered bitmap"))?;
            cropped.extend_from_slice(line);
        }
        tracing::debug!(hwnd = hwnd.0 as usize, "window screenshot via PrintWindow");
        return encode_png(width as u32, height as u32, cropped);
    }

    tracing::debug!(
        hwnd = hwnd.0 as usize,
        "PrintWindow failed; capturing the screen area instead"
    );
    let visible = intersect(frame, virtual_screen_rect())
        .ok_or_else(|| error::unsupported("The window is outside the visible screen area"))?;
    capture_screen_rect(visible)
}

// ── Highlight overlay ─────────────────────────────────────────────────────────

const HIGHLIGHT_THICKNESS: i32 = 3;

/// XOR a rectangle outline onto the screen (drawing it twice erases it).
unsafe fn xor_rectangle(pen: HGDIOBJ, rect: Rect) {
    let Ok(screen) = ScreenDc::get() else {
        return;
    };
    let _pen = Selection::new(screen.0, pen);
    let _brush = Selection::new(screen.0, GetStockObject(NULL_BRUSH));
    SetROP2(screen.0, R2_NOTXORPEN);
    let t = HIGHLIGHT_THICKNESS;
    let _ = Rectangle(
        screen.0,
        rect.x - t,
        rect.y - t,
        rect.x + rect.width + t,
        rect.y + rect.height + t,
    );
}

unsafe fn draw_highlight_rect(rect: Rect, duration_ms: u64) {
    let pen = CreatePen(PS_SOLID, HIGHLIGHT_THICKNESS, COLORREF(0x00FF_8D4C)); // BGR blue
    if pen.is_invalid() {
        return;
    }
    let pen = GdiObject(pen.into());
    xor_rectangle(pen.0, rect);
    std::thread::sleep(Duration::from_millis(duration_ms));
    xor_rectangle(pen.0, rect);
}

// ── Tests (pure helpers only) ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_type_table_round_trips() {
        for (offset, t) in CONTROL_TYPES.iter().enumerate() {
            let id = CONTROL_TYPE_BASE + offset as i32;
            assert_eq!(control_type(id), *t);
            assert!(
                control_type_ids(*t).contains(&id),
                "{t:?} must map back to {id}"
            );
        }
        assert_eq!(control_type(0), ElementType::Unknown);
        assert_eq!(control_type(50041), ElementType::Unknown);
        assert_eq!(control_type_ids(ElementType::Custom), vec![50025, 50039]);
        assert_eq!(control_type_ids(ElementType::ToolBar), vec![50021, 50040]);
        assert!(control_type_ids(ElementType::Dialog).is_empty());
    }

    #[test]
    fn absolute_coordinates_hit_the_pixel() {
        for extent in [1, 800, 1920, 3840, 7680] {
            for offset in [0, extent / 2, extent - 1] {
                let n = normalize_coordinate(offset, extent);
                assert!((0..=65_535).contains(&n));
                assert_eq!(i64::from(n) * i64::from(extent) / 65_536, i64::from(offset));
            }
        }
    }

    #[test]
    fn keys_map_to_virtual_keys() {
        assert_eq!(key_vk(Key::F(12)).ok(), Some((0x7B, false)));
        assert_eq!(key_vk(Key::F(24)).ok(), Some((0x87, false)));
        assert_eq!(key_vk(Key::Delete).ok(), Some((0x2E, true)));
        assert!(key_vk(Key::F(25)).is_err());
    }

    #[test]
    fn unicode_text_uses_one_event_pair_per_utf16_unit() {
        assert_eq!(unicode_inputs("a🙂").len(), 6);
    }
}
