//! Linux backend: AT-SPI2 (the accessibility D-Bus) for discovery and native
//! actions, `xdotool` (X11 / XWayland) for keyboard input and window management.
//!
//! Design notes:
//! - All AT-SPI traffic goes over the *accessibility bus* (its address comes from
//!   `org.a11y.Bus` on the session bus). That is where `org.a11y.atspi.Registry`
//!   and the applications live — not on the session bus itself.
//! - A dedicated OS thread owns a current-thread Tokio runtime and keeps driving
//!   it (timers). The synchronous [`UiBackend`] methods run their futures with
//!   [`tokio::runtime::Handle::block_on`] on the caller's (blocking) thread.
//! - Proxies are built with `CacheProperties::No`: zbus otherwise subscribes to
//!   `PropertiesChanged` and calls `GetAll` on the first property read. Proxy
//!   construction is lazy and never fails for a missing interface, so
//!   capabilities always come from `GetInterfaces` and `GetState`.
//! - Independent calls are issued concurrently (zbus pipelines them over one
//!   connection); a semaphore bounds the number of calls in flight and every
//!   call has a reply timeout, so a hung application cannot hang OculOS.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use atspi::connection::{set_session_accessibility, AccessibilityConnection};
use atspi::proxy::accessible::AccessibleProxy;
use atspi::proxy::action::ActionProxy;
use atspi::proxy::component::ComponentProxy;
use atspi::proxy::editable_text::EditableTextProxy;
use atspi::proxy::selection::SelectionProxy;
use atspi::proxy::text::TextProxy;
use atspi::proxy::value::ValueProxy;
use atspi::{CoordType, Interface, InterfaceSet, Role, ScrollType, State, StateSet};
use dashmap::DashMap;
use futures_util::future::{join, join3, join4, join5, join_all, BoxFuture, FutureExt};
use tokio::sync::{OnceCell, Semaphore};
use zbus::{fdo, CacheProperties, Connection, ProxyBuilder};

use crate::error::{self, ErrorKind};
use crate::keys::{Chord, Key, KeyStep, Modifier};
use crate::platform::UiBackend;
use crate::registry::{self, ElementRegistry};
use crate::types::{ElementType, ExpandState, RangeInfo, Rect, ToggleState, UiElement, WindowInfo};

// ── Constants ─────────────────────────────────────────────────────────────────

const REGISTRY_BUS: &str = "org.a11y.atspi.Registry";
const ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";
const NULL_PATH: &str = "/org/a11y/atspi/accessible/null";

/// Nodes deeper than this become a placeholder (tree) or are skipped (search).
const MAX_DEPTH: u32 = 48;
/// Maximum number of elements returned by one search.
const MAX_RESULTS: usize = 500;
/// D-Bus calls in flight at once on the shared connection. Applications answer
/// serially on their main loop, so deeper pipelining only grows their queues.
const MAX_IN_FLIGHT: usize = 128;
/// Reply timeout for read-only calls.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Reply timeout for calls that make the application do something.
const ACTION_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest text read into `UiElement.value`.
const MAX_TEXT_CHARS: i32 = 10_000;
/// `ATSPI_ROLE_SWITCH` — added to at-spi2-core after `PUSH_BUTTON_MENU` (129),
/// so atspi-common 0.3's `Role` cannot decode it.
const ROLE_SWITCH: u32 = 130;
/// Delay between focusing an element and typing into it.
const FOCUS_SETTLE: Duration = Duration::from_millis(60);

const A11Y_BUS_HINT: &str = "Make sure at-spi2-core is installed (e.g. `sudo apt install \
    at-spi2-core`) and that OculOS runs inside a graphical desktop session \
    (DBUS_SESSION_BUS_ADDRESS must point to the session bus, which provides org.a11y.Bus).";

// ── Element identity ──────────────────────────────────────────────────────────

/// Identity of an accessible object: the application's unique bus name plus the
/// object path. This is what the registry stores behind an `oculos_id`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StoredElement {
    bus: String,
    path: String,
}

impl StoredElement {
    fn new(bus: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            bus: bus.into(),
            path: path.into(),
        }
    }

    /// `None` for the AT-SPI "null" object (no parent, no child…).
    fn from_accessible(a: atspi::Accessible) -> Option<Self> {
        let path = a.path.as_str().to_owned();
        if a.name.is_empty() || path == NULL_PATH {
            None
        } else {
            Some(Self::new(a.name, path))
        }
    }

    fn oculos_id(&self) -> String {
        registry::stable_id((&self.bus, &self.path))
    }
}

// ── Small data carriers ───────────────────────────────────────────────────────

/// What a search needs to decide whether a node matches (cheap to fetch).
struct Probe {
    role: u32,
    name: String,
    /// `None` = not fetched yet; `Some(None)` = fetched, empty or unsupported.
    accessible_id: Option<Option<String>>,
}

/// Whether the parent of a node implements the Selection interface
/// (which makes the node selectable with `select_child`).
#[derive(Clone)]
enum ParentSel {
    Known(bool),
    /// Looked up on demand (only for search matches), once per parent.
    Lazy(Arc<LazyParent>),
}

struct LazyParent {
    el: StoredElement,
    has_selection: OnceCell<bool>,
}

impl ParentSel {
    fn lazy(el: StoredElement) -> Self {
        ParentSel::Lazy(Arc::new(LazyParent {
            el,
            has_selection: OnceCell::new(),
        }))
    }
}

/// Programmatic action names (lower-cased) plus the first key binding.
struct ActionList {
    names: Vec<String>,
    shortcut: Option<String>,
}

/// A live view of a stored element, fetched right before an interaction.
struct Live {
    id: String,
    el: StoredElement,
    acc: AccessibleProxy<'static>,
    role: u32,
    element_type: ElementType,
    states: StateSet,
    ifaces: InterfaceSet,
}

// ── Backend ───────────────────────────────────────────────────────────────────

pub struct LinuxUiBackend {
    rt: tokio::runtime::Handle,
    conn: Connection,
    /// `org.freedesktop.DBus` on the accessibility bus (PID lookup).
    dbus: fdo::DBusProxy<'static>,
    limiter: Semaphore,
    registry: ElementRegistry<StoredElement>,
    /// Unique bus name → PID (unique names are never reused).
    pids: DashMap<String, u32>,
    /// Dropping this stops the runtime thread.
    _stop: tokio::sync::oneshot::Sender<()>,
}

impl LinuxUiBackend {
    pub fn new() -> Result<Self> {
        type Ready = Result<(tokio::runtime::Handle, Connection, fdo::DBusProxy<'static>)>;
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Ready>();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();

        // The runtime lives on its own OS thread, so we never hit "cannot start
        // a runtime from within a runtime" when the caller is inside
        // #[tokio::main], and its timers keep running between calls.
        std::thread::Builder::new()
            .name("oculos-atspi".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(anyhow::Error::new(e)
                            .context("Failed to create the Tokio runtime for AT-SPI2")));
                        return;
                    }
                };
                let connected = rt.block_on(connect_accessibility_bus());
                let ok = connected.is_ok();
                let _ =
                    ready_tx.send(connected.map(|(conn, dbus)| (rt.handle().clone(), conn, dbus)));
                if ok {
                    // Drive the runtime until the backend is dropped.
                    rt.block_on(async {
                        let _ = stop_rx.await;
                    });
                }
            })
            .context("Failed to spawn the AT-SPI2 runtime thread")?;

        let (rt, conn, dbus) = ready_rx
            .recv()
            .map_err(|_| anyhow!("AT-SPI2 init thread panicked"))??;

        tracing::info!("Connected to the AT-SPI2 accessibility bus");
        if find_in_path("xdotool").is_none() {
            tracing::warn!(
                "xdotool not found: send-keys, keyboard scrolling and window focus/close need it"
            );
        }
        if is_wayland() {
            tracing::warn!(
                "Wayland session: xdotool (send-keys, window focus/close) only reaches XWayland \
                 apps"
            );
        }

        Ok(Self {
            rt,
            conn,
            dbus,
            limiter: Semaphore::new(MAX_IN_FLIGHT),
            registry: ElementRegistry::new(),
            pids: DashMap::new(),
            _stop: stop_tx,
        })
    }

    fn block_on<F: Future>(&self, fut: F) -> F::Output {
        self.rt.block_on(fut)
    }

    // ── D-Bus plumbing ────────────────────────────────────────────────────

    /// Run one D-Bus call under the in-flight limit and a reply timeout.
    async fn limited<T>(
        &self,
        timeout: Duration,
        call: impl Future<Output = zbus::Result<T>>,
    ) -> zbus::Result<T> {
        let _permit = self
            .limiter
            .acquire()
            .await
            .map_err(|_| zbus::Error::Failure("AT-SPI2 call limiter closed".into()))?;
        match tokio::time::timeout(timeout, call).await {
            Ok(result) => result,
            Err(_) => Err(zbus::Error::FDO(Box::new(fdo::Error::Timeout(format!(
                "no reply from the application within {} s",
                timeout.as_secs()
            ))))),
        }
    }

    async fn read<T>(&self, call: impl Future<Output = zbus::Result<T>>) -> zbus::Result<T> {
        self.limited(READ_TIMEOUT, call).await
    }

    /// Build a non-caching proxy of type `P` for `el`.
    async fn proxy<P>(
        &self,
        builder: ProxyBuilder<'static, P>,
        el: &StoredElement,
    ) -> zbus::Result<P>
    where
        P: From<zbus::Proxy<'static>>,
    {
        builder
            .destination(el.bus.clone())?
            .path(el.path.clone())?
            .cache_properties(CacheProperties::No)
            .build()
            .await
    }

    async fn accessible(&self, el: &StoredElement) -> zbus::Result<AccessibleProxy<'static>> {
        self.proxy(AccessibleProxy::builder(&self.conn), el).await
    }

    /// Raw role number (decoded leniently, see [`element_type_of`]).
    async fn role(&self, acc: &AccessibleProxy<'_>) -> zbus::Result<u32> {
        self.read(acc.inner().call("GetRole", &())).await
    }

    async fn states(&self, acc: &AccessibleProxy<'_>) -> zbus::Result<StateSet> {
        let raw: Vec<u32> = self.read(acc.inner().call("GetState", &())).await?;
        Ok(parse_states(&raw))
    }

    /// `GetInterfaces`, decoded leniently: `AccessibleProxy::get_interfaces()`
    /// fails as a whole when an application reports an interface name that
    /// atspi-common 0.3 does not know.
    async fn interfaces(&self, acc: &AccessibleProxy<'_>) -> zbus::Result<InterfaceSet> {
        let names: Vec<String> = self.read(acc.inner().call("GetInterfaces", &())).await?;
        Ok(parse_interfaces(&names))
    }

    async fn interfaces_of(&self, el: &StoredElement) -> InterfaceSet {
        match self.accessible(el).await {
            Ok(acc) => self
                .interfaces(&acc)
                .await
                .unwrap_or_else(|_| InterfaceSet::empty()),
            Err(_) => InterfaceSet::empty(),
        }
    }

    async fn accessible_id(&self, acc: &AccessibleProxy<'_>) -> Option<String> {
        self.read(acc.accessible_id())
            .await
            .ok()
            .filter(|s| !s.trim().is_empty())
    }

    /// Children with one `GetChildren` call (falls back to `GetChildAtIndex`
    /// for bridges that lack it).
    async fn children(&self, acc: &AccessibleProxy<'_>) -> zbus::Result<Vec<StoredElement>> {
        match self.read(acc.get_children()).await {
            Ok(list) => Ok(list
                .into_iter()
                .filter_map(StoredElement::from_accessible)
                .collect()),
            Err(e) if is_missing_method(&e) => {
                let count = self.read(acc.child_count()).await?;
                let kids =
                    join_all((0..count.max(0)).map(|i| self.read(acc.get_child_at_index(i)))).await;
                Ok(kids
                    .into_iter()
                    .filter_map(|k| k.ok().and_then(StoredElement::from_accessible))
                    .collect())
            }
            Err(e) => Err(e),
        }
    }

    async fn probe(&self, acc: &AccessibleProxy<'_>, want_id: bool) -> zbus::Result<Probe> {
        let id = async {
            if want_id {
                Some(self.accessible_id(acc).await)
            } else {
                None
            }
        };
        let (role, name, accessible_id) = join3(self.role(acc), self.read(acc.name()), id).await;
        Ok(Probe {
            role: role?,
            name: name.unwrap_or_default(),
            accessible_id,
        })
    }

    async fn extents(&self, el: &StoredElement) -> Option<Rect> {
        let cp: ComponentProxy = self
            .proxy(ComponentProxy::builder(&self.conn), el)
            .await
            .ok()?;
        let (x, y, width, height) = self.read(cp.get_extents(CoordType::Screen)).await.ok()?;
        // Hidden widgets report (INT_MIN, INT_MIN, 1, 1) or negative sizes.
        if x == i32::MIN || y == i32::MIN || width < 0 || height < 0 {
            return None;
        }
        Some(Rect {
            x,
            y,
            width,
            height,
        })
    }

    /// Action names. `GetActions` may return localized names (e.g. GTK's
    /// "Click"), so when any name is not recognised the programmatic names are
    /// fetched with `GetName`.
    async fn action_list(&self, el: &StoredElement) -> zbus::Result<ActionList> {
        let ap: ActionProxy = self.proxy(ActionProxy::builder(&self.conn), el).await?;
        let listed = self.read(ap.get_actions()).await?;
        let shortcut = listed
            .iter()
            .map(|(_, _, kb)| kb.trim())
            .find(|kb| kb.chars().any(|c| c != ';' && !c.is_whitespace()))
            .map(str::to_owned);
        let mut names: Vec<String> = listed
            .into_iter()
            .map(|(name, _, _)| name.trim().to_lowercase())
            .collect();
        if names.iter().any(|n| classify_action(n).is_none()) {
            let programmatic = join_all(
                (0..names.len())
                    .map(|i| self.read(ap.get_name(i32::try_from(i).unwrap_or(i32::MAX)))),
            )
            .await;
            for (slot, fetched) in names.iter_mut().zip(programmatic) {
                if let Ok(name) = fetched {
                    let name = name.trim().to_lowercase();
                    if !name.is_empty() {
                        *slot = name;
                    }
                }
            }
        }
        Ok(ActionList { names, shortcut })
    }

    async fn text(&self, el: &StoredElement) -> zbus::Result<String> {
        let tp: TextProxy = self.proxy(TextProxy::builder(&self.conn), el).await?;
        let count = self.read(tp.character_count()).await?;
        if count <= 0 {
            return Ok(String::new());
        }
        self.read(tp.get_text(0, count.min(MAX_TEXT_CHARS))).await
    }

    async fn range(&self, el: &StoredElement, read_only: bool) -> Option<RangeInfo> {
        let vp: ValueProxy = self.proxy(ValueProxy::builder(&self.conn), el).await.ok()?;
        let (value, minimum, maximum, step) = join4(
            self.read(vp.current_value()),
            self.read(vp.minimum_value()),
            self.read(vp.maximum_value()),
            self.read(vp.minimum_increment()),
        )
        .await;
        Some(RangeInfo {
            value: value.ok()?,
            minimum: minimum.unwrap_or(0.0),
            maximum: maximum.unwrap_or(100.0),
            step: step.unwrap_or(1.0),
            read_only,
        })
    }

    async fn parent_has_selection(&self, parent: &ParentSel) -> bool {
        match parent {
            ParentSel::Known(known) => *known,
            ParentSel::Lazy(lazy) => {
                *lazy
                    .has_selection
                    .get_or_init(|| async {
                        self.interfaces_of(&lazy.el)
                            .await
                            .contains(Interface::Selection)
                    })
                    .await
            }
        }
    }

    async fn pid_of(&self, bus: &str) -> zbus::Result<u32> {
        if let Some(pid) = self.pids.get(bus) {
            return Ok(*pid);
        }
        let name = zbus::names::BusName::try_from(bus.to_owned())?;
        let pid = self
            .read(async {
                self.dbus
                    .get_connection_unix_process_id(name)
                    .await
                    .map_err(zbus::Error::from)
            })
            .await?;
        if self.pids.len() > 4096 {
            self.pids.clear();
        }
        self.pids.insert(bus.to_owned(), pid);
        Ok(pid)
    }

    // ── Building elements ─────────────────────────────────────────────────

    /// Build the full `UiElement` for one node (without children) and register
    /// its id. Also returns the node's interfaces (children need to know about
    /// Selection).
    async fn element(
        &self,
        el: &StoredElement,
        acc: &AccessibleProxy<'_>,
        probe: Option<Probe>,
        parent: &ParentSel,
    ) -> zbus::Result<(UiElement, InterfaceSet)> {
        let probe = async {
            match probe {
                Some(p) if p.accessible_id.is_some() => Ok(p),
                Some(mut p) => {
                    p.accessible_id = Some(self.accessible_id(acc).await);
                    Ok(p)
                }
                None => self.probe(acc, true).await,
            }
        };
        let (probe, states, ifaces, description) = join4(
            probe,
            self.states(acc),
            self.interfaces(acc),
            self.read(acc.description()),
        )
        .await;
        let probe = probe?;
        let known_states = states.is_ok();
        let states = states.unwrap_or_else(|_| StateSet::empty());
        let ifaces = ifaces.unwrap_or_else(|_| InterfaceSet::empty());

        let role = probe.role;
        let element_type = refine_type(role, ifaces, states);
        let password = role == Role::PasswordText as u32;
        let wants_text = !password
            && ifaces.contains(Interface::Text)
            && (ifaces.contains(Interface::EditableText)
                || is_value_type(element_type)
                || (element_type == ElementType::Text && probe.name.is_empty()));
        let range_read_only =
            states.contains(State::ReadOnly) || element_type == ElementType::ProgressBar;

        let rect = async {
            if ifaces.contains(Interface::Component) {
                self.extents(el).await
            } else {
                None
            }
        };
        let actions = async {
            if ifaces.contains(Interface::Action) {
                self.action_list(el).await.ok()
            } else {
                None
            }
        };
        let text = async {
            if wants_text {
                self.text(el).await.ok()
            } else {
                None
            }
        };
        let range = async {
            if ifaces.contains(Interface::Value) {
                self.range(el, range_read_only).await
            } else {
                None
            }
        };
        let parent_selection = async {
            // Only item-like nodes are selected through their parent (a Selectable
            // node gets "select" anyway).
            !states.contains(State::Selectable)
                && is_item_like(role, element_type)
                && self.parent_has_selection(parent).await
        };
        let (rect, actions, text, range, parent_selection) =
            join5(rect, actions, text, range, parent_selection).await;

        let oculos_id = el.oculos_id();
        self.registry.insert(oculos_id.clone(), el.clone());

        let action_names: &[String] = actions.as_ref().map_or(&[], |a| a.names.as_slice());
        let mut e = UiElement::new(oculos_id, element_type);
        e.actions = available_actions(&Caps {
            element_type,
            role,
            states,
            ifaces,
            action_names,
            parent_selection,
            range_read_only,
        });
        e.keyboard_shortcut = actions.as_ref().and_then(|a| a.shortcut.clone());
        e.label = probe.name;
        e.automation_id = probe.accessible_id.flatten();
        e.help_text = description.ok().filter(|d| !d.trim().is_empty());
        e.rect = rect.unwrap_or_default();
        e.enabled = !known_states || is_enabled(states);
        e.focused = states.contains(State::Focused);
        e.is_keyboard_focusable = states.contains(State::Focusable);
        e.toggle_state = toggle_state(role, element_type, states);
        e.is_selected = selected_state(role, states);
        e.expand_state = expand_state(element_type, states);
        e.range = range;
        e.value = text
            .map(|t| t.replace('\u{fffc}', ""))
            .filter(|t| !t.is_empty() || is_value_type(element_type));
        Ok((e, ifaces))
    }

    /// Full subtree, siblings fetched concurrently.
    fn tree_node(
        &self,
        el: StoredElement,
        depth: u32,
        parent: ParentSel,
    ) -> BoxFuture<'_, zbus::Result<UiElement>> {
        async move {
            if depth > MAX_DEPTH {
                let id = el.oculos_id();
                self.registry.insert(id.clone(), el);
                return Ok(UiElement::depth_limit_placeholder(id));
            }
            let acc = self.accessible(&el).await?;
            let (built, children) =
                join(self.element(&el, &acc, None, &parent), self.children(&acc)).await;
            let (mut elem, ifaces) = built?;
            let child_parent = ParentSel::Known(ifaces.contains(Interface::Selection));
            let kids = join_all(
                children
                    .unwrap_or_default()
                    .into_iter()
                    .map(|child| self.tree_node(child, depth + 1, child_parent.clone())),
            )
            .await;
            elem.children = kids
                .into_iter()
                .filter_map(|kid| match kid {
                    Ok(kid) => Some(kid),
                    Err(e) => {
                        tracing::debug!("Skipping unreadable AT-SPI node: {e}");
                        None
                    }
                })
                .collect();
            Ok(elem)
        }
        .boxed()
    }

    // ── Applications & windows ────────────────────────────────────────────

    /// Application roots registered with the AT-SPI registry.
    async fn applications(&self) -> Result<Vec<StoredElement>> {
        let root = StoredElement::new(REGISTRY_BUS, ROOT_PATH);
        let fetch = async {
            let acc = self.accessible(&root).await?;
            self.children(&acc).await
        };
        fetch.await.map_err(|e| {
            anyhow::Error::new(e).context(
                "Failed to list applications from the AT-SPI2 registry (org.a11y.atspi.Registry) \
                 — is at-spi2-registryd running?",
            )
        })
    }

    async fn find_app_root(&self, pid: u32) -> Result<StoredElement> {
        let apps = self.applications().await?;
        let pids = join_all(apps.iter().map(|app| self.pid_of(&app.bus))).await;
        if let Some((app, _)) = apps
            .into_iter()
            .zip(pids)
            .find(|(_, p)| matches!(p, Ok(p) if *p == pid))
        {
            return Ok(app);
        }
        Err(error::not_found(if process_exists(pid) {
            format!(
                "Process {pid} is running but has no accessible application on the AT-SPI2 bus. \
                 GTK apps register automatically; Chromium/Electron apps must be restarted after \
                 accessibility was enabled (or started with --force-renderer-accessibility); Qt \
                 apps may need QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1."
            )
        } else {
            format!("No process with PID {pid}")
        }))
    }

    async fn app_windows(&self, app: &StoredElement) -> Vec<WindowInfo> {
        let Ok(acc) = self.accessible(app).await else {
            return Vec::new();
        };
        let (pid, name, children) = join3(
            self.pid_of(&app.bus),
            self.read(acc.name()),
            self.children(&acc),
        )
        .await;
        let pid = pid.unwrap_or(0);
        let app_name = name.unwrap_or_default();
        let exe = exe_name(pid).unwrap_or_else(|| app_name.clone());

        let children = children.unwrap_or_default();
        let windows = join_all(children.iter().map(|w| self.window_info(w))).await;
        let mut out: Vec<WindowInfo> = windows
            .into_iter()
            .flatten()
            .map(|(title, rect)| WindowInfo {
                pid,
                hwnd: 0,
                title: if title.is_empty() {
                    app_name.clone()
                } else {
                    title
                },
                exe_name: exe.clone(),
                rect,
                visible: true,
            })
            .collect();

        // Keep applications without a visible top-level window discoverable
        // (their PID still works with /windows/{pid}/tree and find).
        if out.is_empty() && pid != 0 {
            out.push(WindowInfo {
                pid,
                hwnd: 0,
                title: if app_name.is_empty() {
                    exe.clone()
                } else {
                    app_name
                },
                exe_name: exe,
                rect: Rect::default(),
                visible: false,
            });
        }
        out
    }

    /// Title and bounds of a visible top-level window, `None` for anything else.
    async fn window_info(&self, w: &StoredElement) -> Option<(String, Rect)> {
        let acc = self.accessible(w).await.ok()?;
        let (role, name, states, rect) = join4(
            self.role(&acc),
            self.read(acc.name()),
            self.states(&acc),
            self.extents(w),
        )
        .await;
        if !is_window_role(role.ok()?) {
            return None;
        }
        if let Ok(states) = states {
            if !states.contains(State::Showing) && !states.contains(State::Visible) {
                return None;
            }
        }
        Some((name.unwrap_or_default(), rect.unwrap_or_default()))
    }

    /// Ask the toolkit to focus the application's first top-level window.
    async fn focus_app_frame(&self, pid: u32) -> Result<()> {
        let app = self.find_app_root(pid).await?;
        let acc = self
            .accessible(&app)
            .await
            .map_err(|e| anyhow::Error::new(e).context("AT-SPI2 proxy error"))?;
        let windows = self.children(&acc).await.map_err(|e| {
            anyhow::Error::new(e).context("Failed to list the application's windows")
        })?;
        let roles = join_all(windows.iter().map(|w| async {
            let acc = self.accessible(w).await.ok()?;
            self.role(&acc).await.ok()
        }))
        .await;
        let frame = windows
            .iter()
            .zip(roles)
            .find(|(_, role)| role.is_some_and(is_window_role))
            .map(|(w, _)| w.clone())
            .ok_or_else(|| {
                error::not_found(format!(
                    "The application with PID {pid} has no top-level window on the AT-SPI2 bus"
                ))
            })?;
        let cp: ComponentProxy = self
            .proxy(ComponentProxy::builder(&self.conn), &frame)
            .await
            .map_err(|e| anyhow::Error::new(e).context("AT-SPI2 proxy error"))?;
        match self.limited(ACTION_TIMEOUT, cp.grab_focus()).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(anyhow!("the toolkit refused to focus its window")),
            Err(e) => Err(anyhow::Error::new(e).context("Component.GrabFocus failed")),
        }
    }

    // ── Interaction helpers ───────────────────────────────────────────────

    fn stored(&self, id: &str) -> Result<StoredElement> {
        self.registry
            .get(id)
            .ok_or_else(|| error::element_not_found(id))
    }

    async fn live_node(&self, id: &str, el: StoredElement) -> Result<Live> {
        let acc = self
            .accessible(&el)
            .await
            .map_err(|e| element_error(id, "resolving it", e))?;
        let (role, states, ifaces) =
            join3(self.role(&acc), self.states(&acc), self.interfaces(&acc)).await;
        let ifaces = ifaces.map_err(|e| element_error(id, "reading its interfaces", e))?;
        let states = states.unwrap_or_else(|_| StateSet::empty());
        if states.contains(State::Defunct) {
            return Err(error::element_not_found(id));
        }
        let role = role.unwrap_or(Role::Invalid as u32);
        Ok(Live {
            id: id.to_owned(),
            el,
            acc,
            role,
            element_type: refine_type(role, ifaces, states),
            states,
            ifaces,
        })
    }

    /// Resolve an `oculos_id` and fetch its current role, states and interfaces.
    async fn live(&self, id: &str) -> Result<Live> {
        let el = self.stored(id)?;
        self.live_node(id, el).await
    }

    async fn actions_of(&self, live: &Live, verb: &str) -> Result<Vec<String>> {
        if !live.ifaces.contains(Interface::Action) {
            return Err(error::unsupported(format!(
                "Element '{}' exposes no AT-SPI actions, so it cannot be {verb} natively. \
                 Try focus + send-keys {{ENTER}} or {{SPACE}}.",
                live.id
            )));
        }
        self.action_list(&live.el)
            .await
            .map(|a| a.names)
            .map_err(|e| element_error(&live.id, "listing its actions", e))
    }

    async fn do_action(&self, live: &Live, index: usize, name: &str) -> Result<()> {
        let what = format!("performing action '{name}' (it may still have happened)");
        let ap: ActionProxy = self
            .proxy(ActionProxy::builder(&self.conn), &live.el)
            .await
            .map_err(|e| element_error(&live.id, &what, e))?;
        let index = i32::try_from(index).map_err(|_| anyhow!("action index out of range"))?;
        match self.limited(ACTION_TIMEOUT, ap.do_action(index)).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(anyhow!(
                "The application reported that action '{name}' could not be performed on \
                 element '{}' (hidden or insensitive?)",
                live.id
            )),
            Err(e) => Err(element_error(&live.id, &what, e)),
        }
    }

    async fn grab_focus(&self, live: &Live) -> Result<()> {
        if live.states.contains(State::Focused) {
            return Ok(());
        }
        if !live.ifaces.contains(Interface::Component) {
            return Err(error::unsupported(format!(
                "Element '{}' has no AT-SPI Component interface, so it cannot be focused",
                live.id
            )));
        }
        if !live.states.is_empty() && !live.states.contains(State::Focusable) {
            return Err(error::unsupported(format!(
                "Element '{}' is not keyboard-focusable",
                live.id
            )));
        }
        let cp: ComponentProxy = self
            .proxy(ComponentProxy::builder(&self.conn), &live.el)
            .await
            .map_err(|e| element_error(&live.id, "focusing it", e))?;
        match self.limited(ACTION_TIMEOUT, cp.grab_focus()).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(anyhow!(
                "The application refused to move keyboard focus to element '{}'",
                live.id
            )),
            Err(e) => Err(element_error(&live.id, "focusing it", e)),
        }
    }

    /// First showing, focusable descendant (breadth-first, bounded) — scroll
    /// containers such as GtkScrolledWindow are usually not focusable themselves.
    async fn focusable_descendant(&self, root: &Live) -> Option<Live> {
        const MAX_LEVELS: usize = 4;
        const MAX_NODES: usize = 200;
        let mut frontier = vec![root.el.clone()];
        let mut seen = 0;
        for _ in 0..MAX_LEVELS {
            let lists = join_all(frontier.iter().map(|el| async {
                let acc = self.accessible(el).await.ok()?;
                self.children(&acc).await.ok()
            }))
            .await;
            let kids: Vec<StoredElement> = lists
                .into_iter()
                .flatten()
                .flatten()
                .take(MAX_NODES.saturating_sub(seen))
                .collect();
            seen += kids.len();
            let lives = join_all(kids.into_iter().map(|k| self.live_node(&root.id, k))).await;
            let mut next = Vec::new();
            for live in lives.into_iter().flatten() {
                if live.ifaces.contains(Interface::Component)
                    && live.states.contains(State::Focusable)
                    && live.states.contains(State::Showing)
                {
                    return Some(live);
                }
                next.push(live.el);
            }
            if next.is_empty() || seen >= MAX_NODES {
                break;
            }
            frontier = next;
        }
        None
    }

    /// Scroll natively through a ScrollBar child's Value interface. `Ok(false)`
    /// when the element has no suitable scroll bar.
    async fn scroll_with_scrollbar(&self, live: &Live, direction: &str) -> Result<bool> {
        let vertical = !matches!(direction, "left" | "right");
        let Ok(children) = self.children(&live.acc).await else {
            return Ok(false);
        };
        let bars = join_all(children.iter().map(|child| async move {
            let acc = self.accessible(child).await.ok()?;
            if self.role(&acc).await.ok()? != Role::ScrollBar as u32 {
                return None;
            }
            let (states, ifaces) = join(self.states(&acc), self.interfaces(&acc)).await;
            let (states, ifaces) = (states.ok()?, ifaces.ok()?);
            let orientation = if vertical {
                State::Vertical
            } else {
                State::Horizontal
            };
            (ifaces.contains(Interface::Value) && states.contains(orientation))
                .then(|| child.clone())
        }))
        .await;
        let Some(bar) = bars.into_iter().flatten().next() else {
            return Ok(false);
        };

        let what = "scrolling it";
        let vp: ValueProxy = self
            .proxy(ValueProxy::builder(&self.conn), &bar)
            .await
            .map_err(|e| element_error(&live.id, what, e))?;
        let (current, minimum, maximum, increment) = join4(
            self.read(vp.current_value()),
            self.read(vp.minimum_value()),
            self.read(vp.maximum_value()),
            self.read(vp.minimum_increment()),
        )
        .await;
        let (Ok(current), Ok(minimum), Ok(maximum)) = (current, minimum, maximum) else {
            return Ok(false);
        };
        if maximum <= minimum {
            // Everything fits: there is nothing to scroll (like being at the end).
            return Ok(true);
        }
        let line = increment
            .ok()
            .filter(|i| *i > 0.0)
            .unwrap_or((maximum - minimum) / 20.0);
        let page = (line * 9.0).min(maximum - minimum);
        let delta = match direction {
            "up" | "left" => -line,
            "down" | "right" => line,
            "page-up" => -page,
            _ => page,
        };
        let target = (current + delta).clamp(minimum, maximum);
        if (target - current).abs() > f64::EPSILON {
            self.limited(ACTION_TIMEOUT, vp.set_current_value(target))
                .await
                .map_err(|e| element_error(&live.id, what, e))?;
        }
        Ok(true)
    }

    /// Current text, `None` when it cannot be read back (no Text interface,
    /// password field…).
    async fn readable_text(&self, live: &Live) -> Option<String> {
        if !live.ifaces.contains(Interface::Text) || live.role == Role::PasswordText as u32 {
            return None;
        }
        self.text(&live.el).await.ok()
    }

    /// Did the text change after a write? Toolkits such as atk-bridge report
    /// success for `SetTextContents` even on read-only fields, and Chromium
    /// applies it asynchronously, so poll briefly.
    async fn text_changed(&self, live: &Live, before: Option<&str>, wanted: &str) -> bool {
        let Some(before) = before else {
            return true; // cannot verify
        };
        if before == wanted {
            return true;
        }
        for attempt in 0..6 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            match self.text(&live.el).await {
                Ok(now) if now != before => return true,
                Ok(_) => {}
                Err(_) => return true,
            }
        }
        false
    }

    async fn set_text_async(&self, id: &str, text: &str) -> Result<()> {
        let live = self.live(id).await?;
        if !live.ifaces.contains(Interface::EditableText) {
            return Err(error::unsupported(format!(
                "Element '{id}' does not implement the AT-SPI EditableText interface (not an \
                 editable text field); use send-keys to type into it"
            )));
        }
        if live.states.contains(State::ReadOnly) {
            return Err(error::unsupported(format!("Element '{id}' is read-only")));
        }
        ensure_enabled(&live)?;

        let before = self.readable_text(&live).await;
        let what = "setting its text";
        let et: EditableTextProxy = self
            .proxy(EditableTextProxy::builder(&self.conn), &live.el)
            .await
            .map_err(|e| element_error(id, what, e))?;

        // 1) SetTextContents.
        let mut problems = Vec::new();
        match self
            .limited(ACTION_TIMEOUT, et.set_text_contents(text))
            .await
        {
            Ok(true) => {
                if self.text_changed(&live, before.as_deref(), text).await {
                    return Ok(());
                }
                problems.push("SetTextContents had no effect".to_owned());
            }
            Ok(false) => problems.push("SetTextContents was refused".to_owned()),
            Err(e) if is_gone(&e) && !is_missing_method(&e) => {
                return Err(error::element_not_found(id))
            }
            Err(e) => problems.push(format!("SetTextContents failed: {e}")),
        }

        // 2) Fallback: delete everything, then insert.
        let count = match before.as_deref() {
            Some(t) => i32::try_from(t.chars().count()).unwrap_or(i32::MAX),
            None => -1, // "to the end" for GTK and Qt
        };
        let deleted = self.limited(ACTION_TIMEOUT, et.delete_text(0, count)).await;
        let byte_len = i32::try_from(text.len()).unwrap_or(i32::MAX);
        let inserted = self
            .limited(ACTION_TIMEOUT, et.insert_text(0, text, byte_len))
            .await;
        match (deleted, inserted) {
            (_, Ok(true)) => {
                if self.text_changed(&live, before.as_deref(), text).await {
                    return Ok(());
                }
                problems.push("DeleteText/InsertText had no effect".to_owned());
            }
            (_, Ok(false)) => problems.push("InsertText was refused".to_owned()),
            (_, Err(e)) => return Err(element_error(id, what, e)),
        }
        Err(error::unsupported(format!(
            "The application did not accept new text for element '{id}' ({}); the field may be \
             read-only — try send-keys",
            problems.join("; ")
        )))
    }

    async fn select_async(&self, id: &str) -> Result<()> {
        let live = self.live(id).await?;
        ensure_enabled(&live)?;
        if selected_state(live.role, live.states) == Some(true) {
            return Ok(()); // already selected
        }
        let names = if live.ifaces.contains(Interface::Action) {
            self.actions_of(&live, "selected").await?
        } else {
            Vec::new()
        };
        let item_like = is_item_like(live.role, live.element_type);
        if !live.states.contains(State::Selectable)
            && !item_like
            && find_action(&names, &[ActionKind::Select]).is_none()
        {
            return Err(error::unsupported(format!(
                "Element '{id}' is not selectable (a {} — use click instead)",
                live.element_type.name()
            )));
        }

        // 1) The parent's Selection interface.
        let (parent, index) = join(
            self.read(live.acc.parent()),
            self.read(live.acc.get_index_in_parent()),
        )
        .await;
        let mut problems = Vec::new();
        if let (Ok(parent), Ok(index)) = (parent, index) {
            if let Some(parent) = StoredElement::from_accessible(parent).filter(|_| index >= 0) {
                if self
                    .interfaces_of(&parent)
                    .await
                    .contains(Interface::Selection)
                {
                    let sp: SelectionProxy = self
                        .proxy(SelectionProxy::builder(&self.conn), &parent)
                        .await
                        .map_err(|e| element_error(id, "selecting it", e))?;
                    match self.limited(ACTION_TIMEOUT, sp.select_child(index)).await {
                        Ok(true) => return Ok(()),
                        Ok(false) => problems.push("the parent refused SelectChild".to_owned()),
                        Err(e) if is_gone(&e) && !is_missing_method(&e) => {
                            return Err(error::element_not_found(id))
                        }
                        Err(e) => problems.push(format!("SelectChild failed: {e}")),
                    }
                }
            }
        }

        // 2) A select/click action on the element itself.
        if let Some(i) = select_action_index(&names) {
            return self.do_action(&live, i, &names[i]).await;
        }
        problems.push(format!("available actions: {}", describe_actions(&names)));
        Err(error::unsupported(format!(
            "Element '{id}' cannot be selected: its parent has no AT-SPI Selection interface and \
             it has no select/click action{}",
            if problems.is_empty() {
                String::new()
            } else {
                format!(" ({})", problems.join("; "))
            }
        )))
    }

    async fn expand_collapse_async(&self, id: &str, expand: bool) -> Result<()> {
        let live = self.live(id).await?;
        let expanded = live.states.contains(State::Expanded);
        if is_expandable(live.states) && expanded == expand {
            return Ok(()); // already in the requested state
        }
        ensure_enabled(&live)?;
        let verb = if expand { "expanded" } else { "collapsed" };
        let names = self.actions_of(&live, verb).await?;
        match expand_action_index(&names, live.element_type, live.states, expand) {
            Some(i) => self.do_action(&live, i, &names[i]).await,
            None => Err(error::unsupported(format!(
                "Element '{id}' cannot be {verb}: no matching action (available actions: {})",
                describe_actions(&names)
            ))),
        }
    }

    async fn set_range_async(&self, id: &str, value: f64) -> Result<()> {
        if !value.is_finite() {
            return Err(error::invalid_input(format!(
                "Value must be a finite number, got {value}"
            )));
        }
        let live = self.live(id).await?;
        if !live.ifaces.contains(Interface::Value) {
            return Err(error::unsupported(format!(
                "Element '{id}' does not implement the AT-SPI Value interface (not a slider or \
                 spinner)"
            )));
        }
        if live.states.contains(State::ReadOnly) || live.element_type == ElementType::ProgressBar {
            return Err(error::unsupported(format!(
                "The value of element '{id}' is read-only"
            )));
        }
        ensure_enabled(&live)?;
        let what = "setting its value";
        let vp: ValueProxy = self
            .proxy(ValueProxy::builder(&self.conn), &live.el)
            .await
            .map_err(|e| element_error(id, what, e))?;
        let (minimum, maximum) =
            join(self.read(vp.minimum_value()), self.read(vp.maximum_value())).await;
        if let (Ok(minimum), Ok(maximum)) = (minimum, maximum) {
            if maximum > minimum && !(minimum..=maximum).contains(&value) {
                return Err(error::invalid_input(format!(
                    "Value {value} is outside the allowed range [{minimum}, {maximum}]"
                )));
            }
        }
        self.limited(ACTION_TIMEOUT, vp.set_current_value(value))
            .await
            .map_err(|e| element_error(id, what, e))
    }

    async fn scroll_into_view_async(&self, id: &str) -> Result<()> {
        let live = self.live(id).await?;
        if !live.ifaces.contains(Interface::Component) {
            return Err(error::unsupported(format!(
                "Element '{id}' has no AT-SPI Component interface, so it cannot be scrolled \
                 into view"
            )));
        }
        let what = "scrolling it into view";
        let cp: ComponentProxy = self
            .proxy(ComponentProxy::builder(&self.conn), &live.el)
            .await
            .map_err(|e| element_error(id, what, e))?;
        match self
            .limited(ACTION_TIMEOUT, cp.scroll_to(ScrollType::Anywhere))
            .await
        {
            Ok(true) => Ok(()),
            Ok(false) => Err(error::unsupported(format!(
                "The application does not implement ScrollTo for element '{id}'"
            ))),
            Err(e) if is_missing_method(&e) => Err(error::unsupported(format!(
                "The application's AT-SPI bridge does not support Component.ScrollTo (element \
                 '{id}')"
            ))),
            Err(e) => Err(element_error(id, what, e)),
        }
    }
}

/// Connect to the accessibility bus (after asking toolkits to enable accessibility).
async fn connect_accessibility_bus() -> Result<(Connection, fdo::DBusProxy<'static>)> {
    // Chromium/Electron, Firefox and Qt only expose their trees when
    // org.a11y.Status.IsEnabled is set; GTK always does. Failure is not fatal.
    match tokio::time::timeout(Duration::from_secs(3), set_session_accessibility(true)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::debug!("Could not set org.a11y.Status.IsEnabled: {e}"),
        Err(_) => tracing::debug!("Timed out setting org.a11y.Status.IsEnabled"),
    }

    let open = async {
        match std::env::var("AT_SPI_BUS_ADDRESS") {
            Ok(addr) if !addr.trim().is_empty() => {
                AccessibilityConnection::connect(addr.trim().parse()?).await
            }
            _ => AccessibilityConnection::open().await,
        }
    };
    let a11y = match tokio::time::timeout(Duration::from_secs(10), open).await {
        Ok(Ok(a11y)) => a11y,
        Ok(Err(e)) => {
            return Err(anyhow!(
                "Could not connect to the AT-SPI2 accessibility bus: {e}. {A11Y_BUS_HINT}"
            ))
        }
        Err(_) => {
            return Err(anyhow!(
                "Timed out connecting to the AT-SPI2 accessibility bus. {A11Y_BUS_HINT}"
            ))
        }
    };
    let conn = a11y.connection().clone();
    let dbus = fdo::DBusProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .context("Failed to create an org.freedesktop.DBus proxy on the accessibility bus")?;
    Ok((conn, dbus))
}

// ── Search ────────────────────────────────────────────────────────────────────

/// One `find_elements` run. Nodes are first probed cheaply (role, name, id);
/// the full element (states, extents, value, actions) is only built for
/// candidates.
struct Search<'a> {
    backend: &'a LinuxUiBackend,
    /// Lower-cased query.
    query: Option<String>,
    element_type: Option<ElementType>,
    interactive_only: bool,
    found: AtomicUsize,
}

impl Search<'_> {
    fn is_full(&self) -> bool {
        self.found.load(Ordering::Relaxed) >= MAX_RESULTS
    }

    fn is_candidate(&self, probe: &Probe) -> bool {
        if let Some(wanted) = self.element_type {
            // Role::Text is refined to Edit or Text once its interfaces are known.
            let text_role = probe.role == Role::Text as u32 && wanted == ElementType::Text;
            if element_type_of(probe.role) != wanted && !text_role {
                return false;
            }
        }
        match &self.query {
            None => true,
            Some(q) => {
                probe.name.to_lowercase().contains(q.as_str())
                    || probe
                        .accessible_id
                        .as_ref()
                        .and_then(Option::as_ref)
                        .is_some_and(|id| id.to_lowercase().contains(q.as_str()))
            }
        }
    }

    /// `interactive` ignores "scroll-into-view": nearly every node implements
    /// Component, so it would make labels and fillers "interactive".
    fn accepts(&self, elem: &UiElement) -> bool {
        self.element_type.is_none_or(|t| elem.element_type == t)
            && (!self.interactive_only || elem.actions.iter().any(|a| a != "scroll-into-view"))
    }

    fn visit(
        &self,
        el: StoredElement,
        depth: u32,
        parent: ParentSel,
    ) -> BoxFuture<'_, Vec<UiElement>> {
        async move {
            let mut out = Vec::new();
            if depth > MAX_DEPTH || self.is_full() {
                return out;
            }
            let b = self.backend;
            let Ok(acc) = b.accessible(&el).await else {
                return out;
            };
            let (probe, children) =
                join(b.probe(&acc, self.query.is_some()), b.children(&acc)).await;
            let Ok(probe) = probe else {
                return out; // node vanished
            };

            let mut own_selection = None;
            if self.is_candidate(&probe) {
                if let Ok((elem, ifaces)) = b.element(&el, &acc, Some(probe), &parent).await {
                    own_selection = Some(ifaces.contains(Interface::Selection));
                    if self.accepts(&elem) {
                        self.found.fetch_add(1, Ordering::Relaxed);
                        out.push(elem);
                    }
                }
            }

            let children = children.unwrap_or_default();
            if children.is_empty() || self.is_full() {
                return out;
            }
            let child_parent = match own_selection {
                Some(known) => ParentSel::Known(known),
                None => ParentSel::lazy(el.clone()),
            };
            let nested = join_all(
                children
                    .into_iter()
                    .map(|child| self.visit(child, depth + 1, child_parent.clone())),
            )
            .await;
            out.extend(nested.into_iter().flatten());
            out
        }
        .boxed()
    }
}

// ── UiBackend implementation ──────────────────────────────────────────────────

impl UiBackend for LinuxUiBackend {
    fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        self.block_on(async {
            let apps = self.applications().await?;
            let per_app = join_all(apps.iter().map(|app| self.app_windows(app))).await;
            Ok(per_app.into_iter().flatten().collect())
        })
    }

    fn get_ui_tree(&self, pid: u32) -> Result<UiElement> {
        self.block_on(async {
            let root = self.find_app_root(pid).await?;
            self.tree_node(root, 0, ParentSel::Known(false))
                .await
                .map_err(|e| {
                    if is_gone(&e) {
                        error::not_found(format!(
                            "The application with PID {pid} disappeared from the AT-SPI2 bus"
                        ))
                    } else {
                        anyhow::Error::new(e).context("Failed to read the accessibility tree")
                    }
                })
        })
    }

    fn get_ui_tree_hwnd(&self, _hwnd: usize) -> Result<UiElement> {
        Err(hwnd_unsupported())
    }

    fn find_elements(
        &self,
        pid: u32,
        query: Option<&str>,
        element_type: Option<&ElementType>,
        interactive_only: bool,
    ) -> Result<Vec<UiElement>> {
        self.block_on(async {
            let root = self.find_app_root(pid).await?;
            let search = Search {
                backend: self,
                query: query.filter(|q| !q.is_empty()).map(str::to_lowercase),
                element_type: element_type.copied(),
                interactive_only,
                found: AtomicUsize::new(0),
            };
            let mut results = search.visit(root, 0, ParentSel::Known(false)).await;
            results.truncate(MAX_RESULTS);
            Ok(results)
        })
    }

    fn find_elements_hwnd(
        &self,
        _hwnd: usize,
        _query: Option<&str>,
        _element_type: Option<&ElementType>,
        _interactive_only: bool,
    ) -> Result<Vec<UiElement>> {
        Err(hwnd_unsupported())
    }

    fn click_element(&self, oculos_id: &str) -> Result<()> {
        self.block_on(async {
            let live = self.live(oculos_id).await?;
            ensure_enabled(&live)?;
            let names = self.actions_of(&live, "clicked").await?;
            match click_action_index(&names) {
                Some(i) => self.do_action(&live, i, &names[i]).await,
                None => Err(error::unsupported(format!(
                    "Element '{oculos_id}' has no click/press/activate action (available \
                     actions: {})",
                    describe_actions(&names)
                ))),
            }
        })
    }

    fn set_text(&self, oculos_id: &str, text: &str) -> Result<()> {
        self.block_on(self.set_text_async(oculos_id, text))
    }

    fn send_keys(&self, oculos_id: &str, steps: &[KeyStep]) -> Result<()> {
        // Check the keyboard path first so focus is not moved for nothing.
        xdotool_ready()?;
        self.block_on(async {
            let live = self.live(oculos_id).await?;
            self.grab_focus(&live).await
        })?;
        std::thread::sleep(FOCUS_SETTLE);
        send_key_steps(steps)
    }

    fn focus_element(&self, oculos_id: &str) -> Result<()> {
        self.block_on(async {
            let live = self.live(oculos_id).await?;
            self.grab_focus(&live).await
        })
    }

    fn toggle_element(&self, oculos_id: &str) -> Result<()> {
        self.block_on(async {
            let live = self.live(oculos_id).await?;
            ensure_enabled(&live)?;
            let names = self.actions_of(&live, "toggled").await?;
            let checkable = is_checkable(live.role, live.element_type, live.states);
            match toggle_action_index(&names, checkable) {
                Some(i) => self.do_action(&live, i, &names[i]).await,
                None => Err(error::unsupported(format!(
                    "Element '{oculos_id}' is not a toggle (no toggle/check action and not a \
                     check box or toggle button; available actions: {})",
                    describe_actions(&names)
                ))),
            }
        })
    }

    fn expand_element(&self, oculos_id: &str) -> Result<()> {
        self.block_on(self.expand_collapse_async(oculos_id, true))
    }

    fn collapse_element(&self, oculos_id: &str) -> Result<()> {
        self.block_on(self.expand_collapse_async(oculos_id, false))
    }

    fn select_element(&self, oculos_id: &str) -> Result<()> {
        self.block_on(self.select_async(oculos_id))
    }

    fn set_range(&self, oculos_id: &str, value: f64) -> Result<()> {
        self.block_on(self.set_range_async(oculos_id, value))
    }

    fn scroll_element(&self, oculos_id: &str, direction: &str) -> Result<()> {
        let key = match direction {
            "up" => Key::Up,
            "down" => Key::Down,
            "left" => Key::Left,
            "right" => Key::Right,
            "page-up" => Key::PageUp,
            "page-down" => Key::PageDown,
            other => {
                return Err(error::invalid_input(format!(
                    "Unknown scroll direction '{other}' (use up, down, left, right, page-up or \
                     page-down)"
                )))
            }
        };

        // Native first: a scroll bar child with a Value interface.
        let live = self.block_on(async {
            let live = self.live(oculos_id).await?;
            let scrolled = self.scroll_with_scrollbar(&live, direction).await?;
            Ok::<_, anyhow::Error>((!scrolled).then_some(live))
        })?;
        let Some(live) = live else {
            return Ok(());
        };

        // Keyboard fallback: focus the element (or a focusable descendant) and
        // press the arrow / page key.
        xdotool_ready()?;
        self.block_on(async {
            if live.states.contains(State::Focused)
                || (live.ifaces.contains(Interface::Component)
                    && live.states.contains(State::Focusable))
            {
                return self.grab_focus(&live).await;
            }
            match self.focusable_descendant(&live).await {
                Some(inner) => self.grab_focus(&inner).await,
                None => Err(error::unsupported(format!(
                    "Element '{oculos_id}' has no scroll bar with a Value interface and neither \
                     it nor its descendants can take keyboard focus, so it cannot be scrolled"
                ))),
            }
        })?;
        std::thread::sleep(FOCUS_SETTLE);
        send_key_steps(&[KeyStep::Chord(Chord {
            modifiers: Vec::new(),
            key: Some(key),
        })])
    }

    fn scroll_into_view(&self, oculos_id: &str) -> Result<()> {
        self.block_on(self.scroll_into_view_async(oculos_id))
    }

    fn focus_window(&self, pid: u32) -> Result<()> {
        if !process_exists(pid) {
            return Err(error::not_found(format!("No process with PID {pid}")));
        }
        let mut problems = Vec::new();
        let mut window_seen = false;
        let mut xdotool_unusable = None;

        match x11_windows(pid) {
            Ok(ids) => match ids.first() {
                Some(win) => {
                    window_seen = true;
                    match xdotool(&["windowactivate", "--sync", win], Duration::from_secs(3)) {
                        Ok(_) => return Ok(()),
                        Err(e) => problems.push(format!("xdotool windowactivate: {e:#}")),
                    }
                    // Without an EWMH window manager (_NET_ACTIVE_WINDOW) set the
                    // input focus directly.
                    let _ = xdotool(&["windowraise", win], Duration::from_secs(3));
                    match xdotool(&["windowfocus", "--sync", win], Duration::from_secs(3)) {
                        Ok(_) => return Ok(()),
                        Err(e) => problems.push(format!("xdotool windowfocus: {e:#}")),
                    }
                }
                None => problems.push("xdotool found no visible X11 window for this PID".into()),
            },
            Err(e) => {
                problems.push(format!("{e:#}"));
                xdotool_unusable = Some(e);
            }
        }

        // Fallback: ask the toolkit to focus its first top-level window.
        match self.block_on(self.focus_app_frame(pid)) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if error::kind_of(&e) != Some(ErrorKind::NotFound) {
                    window_seen = true;
                }
                problems.push(format!("AT-SPI2: {e:#}"));
            }
        }

        let details = problems.join("; ");
        if window_seen {
            Err(anyhow!(
                "Could not focus the window of PID {pid}: {details}"
            ))
        } else if let Some(e) = xdotool_unusable {
            Err(e.context(format!("Could not focus the window of PID {pid}")))
        } else {
            Err(error::not_found(format!(
                "No window found for PID {pid}: {details}"
            )))
        }
    }

    fn close_window(&self, pid: u32) -> Result<()> {
        if !process_exists(pid) {
            return Err(error::not_found(format!("No process with PID {pid}")));
        }
        let ids = x11_windows(pid)?;
        let Some(win) = ids.first() else {
            return Err(error::not_found(format!(
                "No visible X11 window found for PID {pid}{}",
                wayland_note()
            )));
        };

        // Graceful first: let the window manager send WM_DELETE_WINDOW.
        let mut problems = Vec::new();
        if find_in_path("wmctrl").is_some() {
            let hex = format!("0x{:x}", win.parse::<u64>().unwrap_or(0));
            match run_command("wmctrl", &["-i", "-c", &hex], Duration::from_secs(5)) {
                Ok(out) if out.status.success() => return Ok(()),
                Ok(out) => problems.push(format!(
                    "wmctrl -c: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )),
                Err(e) => problems.push(format!("wmctrl: {e}")),
            }
        }
        match xdotool(&["windowclose", win], Duration::from_secs(5)) {
            Ok(_) => Ok(()),
            Err(e) => {
                problems.push(format!("xdotool windowclose: {e:#}"));
                Err(anyhow!(
                    "Could not close the window of PID {pid}: {}",
                    problems.join("; ")
                ))
            }
        }
    }
}

fn hwnd_unsupported() -> anyhow::Error {
    error::unsupported(
        "Linux has no window handles (HWND); use the PID-based endpoints (/windows/{pid}/...) \
         instead",
    )
}

// ── D-Bus error classification ────────────────────────────────────────────────

const DBUS_ERR: &str = "org.freedesktop.DBus.Error.";

fn dbus_error_name(e: &zbus::Error) -> Option<String> {
    match e {
        zbus::Error::MethodError(name, _, _) => Some(name.as_str().to_owned()),
        zbus::Error::FDO(f) => {
            let short = match **f {
                fdo::Error::UnknownObject(_) => "UnknownObject",
                fdo::Error::UnknownMethod(_) => "UnknownMethod",
                fdo::Error::UnknownInterface(_) => "UnknownInterface",
                fdo::Error::ServiceUnknown(_) => "ServiceUnknown",
                fdo::Error::NameHasNoOwner(_) => "NameHasNoOwner",
                fdo::Error::NotSupported(_) => "NotSupported",
                fdo::Error::NoReply(_) => "NoReply",
                fdo::Error::Timeout(_) => "Timeout",
                fdo::Error::TimedOut(_) => "TimedOut",
                _ => return None,
            };
            Some(format!("{DBUS_ERR}{short}"))
        }
        _ => None,
    }
}

fn dbus_error_is(e: &zbus::Error, names: &[&str]) -> bool {
    dbus_error_name(e)
        .and_then(|n| n.strip_prefix(DBUS_ERR).map(str::to_owned))
        .is_some_and(|short| names.contains(&short.as_str()))
}

/// The object (or its application) no longer exists. atk-bridge answers
/// `UnknownMethod` for paths of destroyed objects.
fn is_gone(e: &zbus::Error) -> bool {
    dbus_error_is(
        e,
        &[
            "UnknownObject",
            "UnknownMethod",
            "UnknownInterface",
            "ServiceUnknown",
            "NameHasNoOwner",
        ],
    )
}

fn is_missing_method(e: &zbus::Error) -> bool {
    dbus_error_is(e, &["UnknownMethod", "UnknownInterface", "NotSupported"])
}

fn is_timeout(e: &zbus::Error) -> bool {
    dbus_error_is(e, &["NoReply", "Timeout", "TimedOut"])
}

/// Turn a D-Bus error on a stored element into an actionable error.
fn element_error(id: &str, what: &str, e: zbus::Error) -> anyhow::Error {
    if is_gone(&e) {
        error::element_not_found(id)
    } else if is_timeout(&e) {
        error::timeout(format!(
            "The application did not answer while {what} (element '{id}'); it may be busy or \
             blocked by a modal dialog"
        ))
    } else {
        anyhow::Error::new(e).context(format!("AT-SPI2 call failed while {what} (element '{id}')"))
    }
}

fn ensure_enabled(live: &Live) -> Result<()> {
    if live.states.is_empty() || is_enabled(live.states) {
        Ok(())
    } else {
        Err(error::unsupported(format!(
            "Element '{}' is disabled (grayed out)",
            live.id
        )))
    }
}

// ── Decoding ──────────────────────────────────────────────────────────────────

/// Decode a raw `GetState` reply, ignoring state bits unknown to atspi-common.
fn parse_states(raw: &[u32]) -> StateSet {
    let low = raw.first().copied().map_or(0, u64::from);
    let high = raw.get(1).copied().map_or(0, u64::from);
    let bits = low | (high << 32);
    StateSet::from_bits(bits).unwrap_or_else(|_| {
        let mut set = StateSet::empty();
        for bit in 0..64 {
            if let Ok(state) = StateSet::from_bits(bits & (1u64 << bit)) {
                set |= state;
            }
        }
        set
    })
}

/// Decode `GetInterfaces` names, ignoring interfaces unknown to atspi-common.
fn parse_interfaces(names: &[String]) -> InterfaceSet {
    let mut set = InterfaceSet::empty();
    for name in names {
        let iface = match name.strip_prefix("org.a11y.atspi.").unwrap_or_default() {
            "Accessible" => Interface::Accessible,
            "Action" => Interface::Action,
            "Application" => Interface::Application,
            "Collection" => Interface::Collection,
            "Component" => Interface::Component,
            "Document" => Interface::Document,
            "EditableText" => Interface::EditableText,
            "Hyperlink" => Interface::Hyperlink,
            "Hypertext" => Interface::Hypertext,
            "Image" => Interface::Image,
            "Selection" => Interface::Selection,
            "Table" => Interface::Table,
            "TableCell" => Interface::TableCell,
            "Text" => Interface::Text,
            "Value" => Interface::Value,
            _ => continue,
        };
        set.insert(iface);
    }
    set
}

// ── Role mapping ──────────────────────────────────────────────────────────────

/// Element type for a raw AT-SPI role number (roles newer than atspi-common
/// map to Unknown, except Switch).
fn element_type_of(role: u32) -> ElementType {
    if role == ROLE_SWITCH {
        return ElementType::CheckBox;
    }
    Role::try_from(role).map_or(ElementType::Unknown, role_to_element_type)
}

fn role_to_element_type(role: Role) -> ElementType {
    use ElementType as T;
    match role {
        Role::Frame | Role::Window | Role::InternalFrame | Role::InputMethodWindow => T::Window,
        Role::Dialog
        | Role::Alert
        | Role::FileChooser
        | Role::ColorChooser
        | Role::FontChooser
        | Role::OptionPane => T::Dialog,
        Role::PushButton | Role::PushButtonMenu | Role::ToggleButton => T::Button,
        Role::Text
        | Role::Entry
        | Role::PasswordText
        | Role::Terminal
        | Role::Editbar
        | Role::DateEditor => T::Edit,
        Role::SpinButton => T::Spinner,
        Role::Label
        | Role::Static
        | Role::Heading
        | Role::Paragraph
        | Role::Caption
        | Role::AcceleratorLabel
        | Role::Definition
        | Role::DescriptionTerm
        | Role::DescriptionValue
        | Role::Footnote
        | Role::Subscript
        | Role::Superscript
        | Role::Mark
        | Role::ContentDeletion
        | Role::ContentInsertion
        | Role::Timer => T::Text,
        Role::CheckBox => T::CheckBox,
        Role::RadioButton => T::RadioButton,
        Role::ComboBox | Role::Autocomplete => T::ComboBox,
        Role::List | Role::ListBox | Role::DescriptionList => T::ListBox,
        Role::ListItem => T::ListItem,
        Role::Tree | Role::TreeTable => T::TreeView,
        Role::TreeItem => T::TreeItem,
        Role::Menu | Role::PopupMenu => T::Menu,
        Role::MenuBar => T::MenuBar,
        Role::MenuItem | Role::CheckMenuItem | Role::RadioMenuItem | Role::TearoffMenuItem => {
            T::MenuItem
        }
        Role::PageTabList => T::TabControl,
        Role::PageTab => T::TabItem,
        Role::ToolBar => T::ToolBar,
        Role::StatusBar => T::StatusBar,
        Role::ScrollBar => T::ScrollBar,
        Role::Slider | Role::Dial | Role::Rating => T::Slider,
        Role::ProgressBar | Role::LevelBar => T::ProgressBar,
        Role::Image
        | Role::Icon
        | Role::DesktopIcon
        | Role::Animation
        | Role::ImageMap
        | Role::Arrow
        | Role::CHART => T::Image,
        Role::Link => T::Link,
        Role::Panel
        | Role::Filler
        | Role::Section
        | Role::Form
        | Role::Grouping
        | Role::Article
        | Role::Landmark
        | Role::Log
        | Role::Marquee
        | Role::BlockQuote
        | Role::Notification
        | Role::InfoBar
        | Role::Header
        | Role::Footer
        | Role::Page
        | Role::Comment
        | Role::Suggestion
        | Role::Math
        | Role::MathFraction
        | Role::MathRoot => T::Group,
        Role::ScrollPane
        | Role::SplitPane
        | Role::Viewport
        | Role::LayeredPane
        | Role::RootPane
        | Role::GlassPane
        | Role::DirectoryPane
        | Role::DesktopFrame
        | Role::Application
        | Role::Embedded
        | Role::HTMLContainer => T::Pane,
        Role::DocumentFrame
        | Role::DocumentWeb
        | Role::DocumentText
        | Role::DocumentSpreadsheet
        | Role::DocumentPresentation
        | Role::DocumentEmail => T::Document,
        Role::Table => T::Table,
        Role::TableRow | Role::TableCell => T::DataItem,
        Role::ColumnHeader | Role::RowHeader | Role::TableColumnHeader | Role::TableRowHeader => {
            T::HeaderItem
        }
        Role::Separator => T::Separator,
        Role::ToolTip => T::ToolTip,
        Role::Calendar => T::Calendar,
        Role::TitleBar => T::TitleBar,
        Role::Canvas | Role::DrawingArea | Role::Audio | Role::Video | Role::Extended => T::Custom,
        _ => T::Unknown,
    }
}

/// `Role::Text` is used both for editable fields (GtkEntry) and plain text.
fn refine_type(role: u32, ifaces: InterfaceSet, states: StateSet) -> ElementType {
    if role == Role::Text as u32
        && !ifaces.contains(Interface::EditableText)
        && !states.contains(State::Editable)
    {
        ElementType::Text
    } else {
        element_type_of(role)
    }
}

fn is_window_role(role: u32) -> bool {
    [
        Role::Frame,
        Role::Window,
        Role::Dialog,
        Role::Alert,
        Role::FileChooser,
        Role::ColorChooser,
        Role::FontChooser,
    ]
    .iter()
    .any(|r| *r as u32 == role)
}

/// Types whose `value` is their text even when empty.
fn is_value_type(t: ElementType) -> bool {
    matches!(
        t,
        ElementType::Edit | ElementType::ComboBox | ElementType::Spinner
    )
}

// ── State helpers ─────────────────────────────────────────────────────────────

fn is_enabled(states: StateSet) -> bool {
    states.contains(State::Enabled) || states.contains(State::Sensitive)
}

fn is_radio(role: u32) -> bool {
    role == Role::RadioButton as u32 || role == Role::RadioMenuItem as u32
}

/// Items of a list/tab/tree/table/menu container, plus radio buttons: what
/// "select" means something for even without the Selectable state.
fn is_item_like(role: u32, t: ElementType) -> bool {
    is_radio(role)
        || matches!(
            t,
            ElementType::ListItem
                | ElementType::TabItem
                | ElementType::TreeItem
                | ElementType::DataItem
                | ElementType::MenuItem
                | ElementType::Menu
        )
}

fn is_checkable(role: u32, t: ElementType, states: StateSet) -> bool {
    t == ElementType::CheckBox
        || role == Role::ToggleButton as u32
        || role == Role::CheckMenuItem as u32
        || (states.contains(State::Checkable) && !is_radio(role))
}

fn is_expandable(states: StateSet) -> bool {
    states.contains(State::Expandable)
        || states.contains(State::Expanded)
        || states.contains(State::Collapsed)
}

fn toggle_state(role: u32, t: ElementType, states: StateSet) -> Option<ToggleState> {
    if !is_checkable(role, t, states) {
        return None;
    }
    Some(if states.contains(State::Indeterminate) {
        ToggleState::Indeterminate
    } else if states.contains(State::Checked)
        || (role == Role::ToggleButton as u32 && states.contains(State::Pressed))
    {
        ToggleState::On
    } else {
        ToggleState::Off
    })
}

fn selected_state(role: u32, states: StateSet) -> Option<bool> {
    if is_radio(role) {
        Some(states.contains(State::Checked) || states.contains(State::Selected))
    } else if states.contains(State::Selectable) {
        Some(states.contains(State::Selected))
    } else if states.contains(State::Selected) {
        Some(true)
    } else {
        None
    }
}

fn expand_state(t: ElementType, states: StateSet) -> Option<ExpandState> {
    if states.contains(State::Expanded) {
        Some(ExpandState::Expanded)
    } else if is_expandable(states) {
        Some(ExpandState::Collapsed)
    } else if t == ElementType::TreeItem {
        Some(ExpandState::LeafNode)
    } else {
        None
    }
}

// ── Actions ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionKind {
    Click,
    Toggle,
    Expand,
    Collapse,
    ExpandOrCollapse,
    Select,
}

/// Classify a programmatic AT-SPI action name (GTK, Qt, Chromium, Firefox).
fn classify_action(name: &str) -> Option<ActionKind> {
    match name.trim() {
        "click" | "press" | "activate" | "jump" | "click-ancestor" => Some(ActionKind::Click),
        "toggle" | "check" | "uncheck" => Some(ActionKind::Toggle),
        "expand" | "open" => Some(ActionKind::Expand),
        "collapse" | "close" => Some(ActionKind::Collapse),
        "expand or contract" | "expand or collapse" | "showmenu" | "show menu" => {
            Some(ActionKind::ExpandOrCollapse)
        }
        "select" | "switch" => Some(ActionKind::Select),
        _ => None,
    }
}

/// Index of the first action of the first kind in `kinds` (preference order).
fn find_action(names: &[String], kinds: &[ActionKind]) -> Option<usize> {
    kinds
        .iter()
        .find_map(|k| names.iter().position(|n| classify_action(n) == Some(*k)))
}

/// A lone action is what "activating" the element means.
fn single_action(names: &[String]) -> Option<usize> {
    (names.len() == 1).then_some(0)
}

fn click_action_index(names: &[String]) -> Option<usize> {
    find_action(names, &[ActionKind::Click]).or_else(|| single_action(names))
}

fn toggle_action_index(names: &[String], checkable: bool) -> Option<usize> {
    find_action(names, &[ActionKind::Toggle])
        .or_else(|| checkable.then(|| click_action_index(names)).flatten())
}

fn select_action_index(names: &[String]) -> Option<usize> {
    find_action(names, &[ActionKind::Select, ActionKind::Click]).or_else(|| single_action(names))
}

fn expand_action_index(
    names: &[String],
    t: ElementType,
    states: StateSet,
    expand: bool,
) -> Option<usize> {
    let direct = if expand {
        ActionKind::Expand
    } else {
        ActionKind::Collapse
    };
    if let Some(i) = find_action(names, &[direct]) {
        return Some(i);
    }
    // The remaining actions *toggle*. They are only safe for collapsing when
    // the element reports its expanded state (the caller has already returned
    // early if it is collapsed); otherwise they could just as well open it.
    let expandable = is_expandable(states);
    if !expand && !expandable {
        return None;
    }
    // GTK3 tree cells always list "expand or contract", even for leaves.
    if expandable || !matches!(t, ElementType::DataItem | ElementType::TreeItem) {
        if let Some(i) = find_action(names, &[ActionKind::ExpandOrCollapse]) {
            return Some(i);
        }
    }
    // Combo boxes and sub-menus open with their press/click action.
    if expandable || matches!(t, ElementType::ComboBox | ElementType::Menu) {
        return click_action_index(names);
    }
    None
}

fn describe_actions(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

/// Everything needed to derive the `actions` list of an element.
struct Caps<'a> {
    element_type: ElementType,
    role: u32,
    states: StateSet,
    ifaces: InterfaceSet,
    action_names: &'a [String],
    parent_selection: bool,
    range_read_only: bool,
}

fn available_actions(c: &Caps) -> Vec<String> {
    let names = c.action_names;
    let has_component = c.ifaces.contains(Interface::Component);
    let mut out: Vec<&str> = Vec::new();
    if click_action_index(names).is_some() {
        out.push("click");
    }
    if toggle_action_index(names, is_checkable(c.role, c.element_type, c.states)).is_some() {
        out.push("toggle");
    }
    if c.states.contains(State::Expanded) {
        if expand_action_index(names, c.element_type, c.states, false).is_some() {
            out.push("collapse");
        }
    } else if expand_action_index(names, c.element_type, c.states, true).is_some() {
        out.push("expand");
    }
    if c.states.contains(State::Selectable)
        || c.parent_selection
        || find_action(names, &[ActionKind::Select]).is_some()
        || (is_item_like(c.role, c.element_type) && select_action_index(names).is_some())
    {
        out.push("select");
    }
    if c.ifaces.contains(Interface::EditableText) && !c.states.contains(State::ReadOnly) {
        out.push("set-text");
        out.push("send-keys");
    }
    if c.ifaces.contains(Interface::Value) && !c.range_read_only {
        out.push("set-range");
    }
    if c.role == Role::ScrollPane as u32 {
        out.push("scroll");
    }
    if has_component && c.states.contains(State::Focusable) {
        out.push("focus");
    }
    if has_component {
        out.push("scroll-into-view");
    }
    out.into_iter().map(str::to_owned).collect()
}

// ── Processes & external commands ─────────────────────────────────────────────

fn process_exists(pid: u32) -> bool {
    pid != 0 && Path::new(&format!("/proc/{pid}")).exists()
}

/// Executable name of a process (`/proc/<pid>/exe`, falling back to `comm`).
fn exe_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|p| {
            p.file_name().map(|n| {
                n.to_string_lossy()
                    .trim_end_matches(" (deleted)")
                    .to_owned()
            })
        })
        .or_else(|| {
            std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .ok()
                .map(|s| s.trim().to_owned())
        })
        .filter(|s| !s.is_empty())
}

fn find_in_path(program: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|p| {
            p.metadata()
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

/// Run a command, killing it when it exceeds `timeout`.
fn run_command(program: &str, args: &[&str], timeout: Duration) -> io::Result<Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{program} did not finish within {} s", timeout.as_secs()),
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn is_wayland() -> bool {
    std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v.eq_ignore_ascii_case("wayland"))
}

fn wayland_note() -> &'static str {
    if is_wayland() {
        " Note: this is a Wayland session — xdotool can only reach XWayland windows, not native \
         Wayland applications."
    } else {
        ""
    }
}

fn xdotool_missing() -> anyhow::Error {
    error::unsupported(format!(
        "xdotool is not installed; OculOS needs it on Linux for keyboard input and window \
         focus/close. Install it (e.g. `sudo apt install xdotool`, `sudo dnf install xdotool` or \
         `sudo pacman -S xdotool`).{}",
        wayland_note()
    ))
}

/// Can xdotool run at all (installed, X display available)?
fn xdotool_ready() -> Result<()> {
    if find_in_path("xdotool").is_none() {
        return Err(xdotool_missing());
    }
    if std::env::var_os("DISPLAY").is_none_or(|d| d.is_empty()) {
        return Err(error::unsupported(format!(
            "No X11 display: DISPLAY is not set, so xdotool cannot send input or manage \
             windows.{}",
            wayland_note()
        )));
    }
    Ok(())
}

fn run_xdotool(args: &[&str], timeout: Duration) -> Result<Output> {
    xdotool_ready()?;
    run_command("xdotool", args, timeout).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => xdotool_missing(),
        io::ErrorKind::TimedOut => error::timeout(format!(
            "xdotool {} did not finish within {} s",
            args.first().copied().unwrap_or_default(),
            timeout.as_secs()
        )),
        _ => anyhow::Error::new(e).context("Failed to run xdotool"),
    })
}

/// Run xdotool and return its stdout; a non-zero exit status is an error.
/// (Arguments are not echoed in errors: they may contain typed secrets.)
fn xdotool(args: &[&str], timeout: Duration) -> Result<String> {
    let out = run_xdotool(args, timeout)?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(anyhow!(
            "xdotool {} failed ({}): {}{}",
            args.first().copied().unwrap_or_default(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
            wayland_note()
        ))
    }
}

/// Visible X11 windows of `pid` (matched through `_NET_WM_PID`).
fn x11_windows(pid: u32) -> Result<Vec<String>> {
    let pid = pid.to_string();
    let out = run_xdotool(
        &["search", "--onlyvisible", "--pid", &pid],
        Duration::from_secs(5),
    )?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Exit status 1 without a message just means "no match".
    if !out.status.success() && !stderr.trim().is_empty() {
        return Err(anyhow!(
            "xdotool search failed: {}{}",
            stderr.trim(),
            wayland_note()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter(|w| w.parse::<u64>().is_ok())
        .map(str::to_owned)
        .collect())
}

// ── Keyboard (xdotool) ────────────────────────────────────────────────────────

/// Type `steps` into the focused window: one `xdotool type` per text run, one
/// `xdotool key` per run of consecutive chords.
fn send_key_steps(steps: &[KeyStep]) -> Result<()> {
    let mut i = 0;
    while i < steps.len() {
        match &steps[i] {
            KeyStep::Text(text) => {
                i += 1;
                if text.is_empty() {
                    continue;
                }
                let chars = u32::try_from(text.chars().count()).unwrap_or(u32::MAX);
                let timeout = Duration::from_secs(5)
                    .saturating_add(Duration::from_millis(30).saturating_mul(chars));
                xdotool(
                    &["type", "--clearmodifiers", "--delay", "8", "--", text],
                    timeout,
                )?;
            }
            KeyStep::Chord(_) => {
                let mut sequences = Vec::new();
                while let Some(KeyStep::Chord(chord)) = steps.get(i) {
                    sequences.extend(chord_keysym(chord));
                    i += 1;
                }
                if sequences.is_empty() {
                    continue;
                }
                let count = u32::try_from(sequences.len()).unwrap_or(u32::MAX);
                let timeout = Duration::from_secs(5)
                    .saturating_add(Duration::from_millis(50).saturating_mul(count));
                let mut args = vec!["key", "--clearmodifiers"];
                args.extend(sequences.iter().map(String::as_str));
                xdotool(&args, timeout)?;
            }
        }
    }
    Ok(())
}

/// xdotool key sequence for a chord, e.g. `ctrl+shift+t`, `super`, `alt+F4`.
fn chord_keysym(chord: &Chord) -> Option<String> {
    let mut parts: Vec<String> = chord
        .modifiers
        .iter()
        .map(|m| modifier_keysym(*m).to_owned())
        .collect();
    if let Some(key) = chord.key {
        parts.push(key_keysym(key));
    }
    (!parts.is_empty()).then(|| parts.join("+"))
}

fn modifier_keysym(m: Modifier) -> &'static str {
    match m {
        Modifier::Ctrl => "ctrl",
        Modifier::Alt => "alt",
        Modifier::Shift => "shift",
        Modifier::Meta => "super",
    }
}

fn key_keysym(key: Key) -> String {
    let name = match key {
        Key::Enter => "Return",
        Key::Tab => "Tab",
        Key::Escape => "Escape",
        Key::Space => "space",
        Key::Backspace => "BackSpace",
        Key::Delete => "Delete",
        Key::Insert => "Insert",
        Key::Home => "Home",
        Key::End => "End",
        Key::PageUp => "Prior",
        Key::PageDown => "Next",
        Key::Up => "Up",
        Key::Down => "Down",
        Key::Left => "Left",
        Key::Right => "Right",
        Key::CapsLock => "Caps_Lock",
        Key::PrintScreen => "Print",
        Key::Menu => "Menu",
        Key::F(n) => return format!("F{n}"),
        Key::Char(c) => return char_keysym(c),
    };
    name.to_owned()
}

/// X keysym name for a character ('+' must be a name: it separates chord parts).
fn char_keysym(c: char) -> String {
    if c.is_ascii_alphanumeric() {
        return c.to_string();
    }
    let name = match c {
        ' ' => "space",
        '!' => "exclam",
        '"' => "quotedbl",
        '#' => "numbersign",
        '$' => "dollar",
        '%' => "percent",
        '&' => "ampersand",
        '\'' => "apostrophe",
        '(' => "parenleft",
        ')' => "parenright",
        '*' => "asterisk",
        '+' => "plus",
        ',' => "comma",
        '-' => "minus",
        '.' => "period",
        '/' => "slash",
        ':' => "colon",
        ';' => "semicolon",
        '<' => "less",
        '=' => "equal",
        '>' => "greater",
        '?' => "question",
        '@' => "at",
        '[' => "bracketleft",
        '\\' => "backslash",
        ']' => "bracketright",
        '^' => "asciicircum",
        '_' => "underscore",
        '`' => "grave",
        '{' => "braceleft",
        '|' => "bar",
        '}' => "braceright",
        '~' => "asciitilde",
        // Any other character: Unicode keysym (XStringToKeysym accepts "U20AC").
        other => return format!("U{:04X}", u32::from(other)),
    };
    name.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn states(list: &[State]) -> StateSet {
        let mut set = StateSet::empty();
        for s in list {
            set.insert(*s);
        }
        set
    }

    fn caps<'a>(role: Role, st: StateSet, ifaces: &[Interface], actions: &'a [String]) -> Caps<'a> {
        let mut set = InterfaceSet::empty();
        for i in ifaces {
            set.insert(*i);
        }
        Caps {
            element_type: refine_type(role as u32, set, st),
            role: role as u32,
            states: st,
            ifaces: set,
            action_names: actions,
            parent_selection: false,
            range_read_only: false,
        }
    }

    #[test]
    fn stable_ids_for_same_object() {
        let a = StoredElement::new(":1.42", "/org/a11y/atspi/accessible/7");
        let b = StoredElement::new(":1.42", "/org/a11y/atspi/accessible/7");
        let c = StoredElement::new(":1.43", "/org/a11y/atspi/accessible/7");
        assert_eq!(a.oculos_id(), b.oculos_id());
        assert_ne!(a.oculos_id(), c.oculos_id());
        assert_eq!(
            a.oculos_id(),
            registry::stable_id((":1.42", "/org/a11y/atspi/accessible/7"))
        );
    }

    #[test]
    fn role_mapping() {
        assert_eq!(
            element_type_of(Role::PushButton as u32),
            ElementType::Button
        );
        assert_eq!(
            element_type_of(Role::SpinButton as u32),
            ElementType::Spinner
        );
        assert_eq!(element_type_of(Role::MenuBar as u32), ElementType::MenuBar);
        assert_eq!(
            element_type_of(Role::Separator as u32),
            ElementType::Separator
        );
        assert_eq!(element_type_of(Role::ToolTip as u32), ElementType::ToolTip);
        assert_eq!(
            element_type_of(Role::Calendar as u32),
            ElementType::Calendar
        );
        assert_eq!(
            element_type_of(Role::TableColumnHeader as u32),
            ElementType::HeaderItem
        );
        assert_eq!(
            element_type_of(Role::TableCell as u32),
            ElementType::DataItem
        );
        assert_eq!(element_type_of(Role::Viewport as u32), ElementType::Pane);
        assert_eq!(element_type_of(Role::Grouping as u32), ElementType::Group);
        assert_eq!(
            element_type_of(Role::DocumentWeb as u32),
            ElementType::Document
        );
        assert_eq!(element_type_of(ROLE_SWITCH), ElementType::CheckBox);
        assert_eq!(element_type_of(9999), ElementType::Unknown);
    }

    #[test]
    fn text_role_is_refined() {
        let editable = parse_interfaces(&names(&["org.a11y.atspi.EditableText"]));
        let t = Role::Text as u32;
        assert_eq!(
            refine_type(t, editable, StateSet::empty()),
            ElementType::Edit
        );
        assert_eq!(
            refine_type(t, InterfaceSet::empty(), StateSet::empty()),
            ElementType::Text
        );
    }

    #[test]
    fn states_decode_and_ignore_unknown_bits() {
        let raw = [(1 << 8) | (1 << 11), 1 << 31];
        let set = parse_states(&raw);
        assert!(set.contains(State::Enabled));
        assert!(set.contains(State::Focusable));
        assert!(!set.contains(State::Focused));
        assert_eq!(parse_states(&[]), StateSet::empty());
    }

    #[test]
    fn interfaces_decode_and_ignore_unknown_names() {
        let set = parse_interfaces(&names(&[
            "org.a11y.atspi.Accessible",
            "org.a11y.atspi.Action",
            "org.a11y.atspi.SomethingNew",
        ]));
        assert!(set.contains(Interface::Action));
        assert!(!set.contains(Interface::Text));
    }

    #[test]
    fn action_classification() {
        let list = names(&["expand or contract", "edit", "activate"]);
        assert_eq!(click_action_index(&list), Some(2));
        assert_eq!(click_action_index(&names(&["custom"])), Some(0));
        assert_eq!(click_action_index(&names(&["a", "b"])), None);
        assert_eq!(toggle_action_index(&names(&["click"]), false), None);
        assert_eq!(toggle_action_index(&names(&["click"]), true), Some(0));
        assert_eq!(toggle_action_index(&names(&["uncheck"]), false), Some(0));
        // Leaf tree cells list "expand or contract" but cannot expand.
        assert_eq!(
            expand_action_index(&list, ElementType::DataItem, StateSet::empty(), true),
            None
        );
        let expandable = states(&[State::Expandable]);
        assert_eq!(
            expand_action_index(&list, ElementType::DataItem, expandable, true),
            Some(0)
        );
        assert_eq!(
            expand_action_index(
                &names(&["press"]),
                ElementType::ComboBox,
                StateSet::empty(),
                true
            ),
            Some(0)
        );
        // Without an expanded state a toggling action could open instead of close.
        assert_eq!(
            expand_action_index(
                &names(&["press"]),
                ElementType::ComboBox,
                StateSet::empty(),
                false
            ),
            None
        );
        assert_eq!(
            expand_action_index(
                &names(&["press"]),
                ElementType::ComboBox,
                states(&[State::Expandable, State::Expanded]),
                false
            ),
            Some(0)
        );
    }

    #[test]
    fn derived_actions() {
        let click = names(&["click"]);
        let st = states(&[State::Enabled, State::Focusable]);
        let a = available_actions(&caps(
            Role::PushButton,
            st,
            &[Interface::Action, Interface::Component],
            &click,
        ));
        assert_eq!(a, ["click", "focus", "scroll-into-view"]);

        let a = available_actions(&caps(
            Role::CheckBox,
            st,
            &[Interface::Action, Interface::Component],
            &click,
        ));
        assert_eq!(a, ["click", "toggle", "focus", "scroll-into-view"]);

        let a = available_actions(&caps(
            Role::Text,
            states(&[State::Enabled, State::Editable]),
            &[Interface::EditableText, Interface::Text],
            &[],
        ));
        assert_eq!(a, ["set-text", "send-keys"]);

        let expanded = states(&[State::Expandable, State::Expanded]);
        let a = available_actions(&caps(
            Role::TreeItem,
            expanded,
            &[Interface::Action],
            &names(&["expand or contract", "activate"]),
        ));
        assert_eq!(a, ["click", "collapse", "select"]);

        // "select" on a plain button would just click it: not offered.
        let a = available_actions(&caps(Role::PushButton, st, &[Interface::Action], &click));
        assert_eq!(a, ["click"]);

        let a = available_actions(&caps(Role::Slider, st, &[Interface::Value], &[]));
        assert_eq!(a, ["set-range"]);

        let a = available_actions(&caps(Role::ScrollPane, StateSet::empty(), &[], &[]));
        assert_eq!(a, ["scroll"]);
    }

    #[test]
    fn toggle_and_selection_state() {
        let t = element_type_of(Role::CheckBox as u32);
        assert_eq!(
            toggle_state(Role::CheckBox as u32, t, states(&[State::Checked])),
            Some(ToggleState::On)
        );
        assert_eq!(
            toggle_state(Role::CheckBox as u32, t, states(&[State::Indeterminate])),
            Some(ToggleState::Indeterminate)
        );
        assert_eq!(
            toggle_state(
                Role::PushButton as u32,
                ElementType::Button,
                StateSet::empty()
            ),
            None
        );
        assert_eq!(
            selected_state(Role::RadioButton as u32, states(&[State::Checked])),
            Some(true)
        );
        assert_eq!(
            selected_state(Role::ListItem as u32, states(&[State::Selectable])),
            Some(false)
        );
    }

    #[test]
    fn keysyms() {
        let chord = |mods: &[Modifier], key: Option<Key>| Chord {
            modifiers: mods.to_vec(),
            key,
        };
        assert_eq!(
            chord_keysym(&chord(
                &[Modifier::Ctrl, Modifier::Shift],
                Some(Key::Char('t'))
            )),
            Some("ctrl+shift+t".into())
        );
        assert_eq!(
            chord_keysym(&chord(&[Modifier::Meta], None)),
            Some("super".into())
        );
        assert_eq!(
            chord_keysym(&chord(&[Modifier::Ctrl], Some(Key::Char('+')))),
            Some("ctrl+plus".into())
        );
        assert_eq!(
            chord_keysym(&chord(&[Modifier::Alt], Some(Key::F(4)))),
            Some("alt+F4".into())
        );
        assert_eq!(chord_keysym(&chord(&[], None)), None);
        assert_eq!(key_keysym(Key::PageDown), "Next");
        assert_eq!(key_keysym(Key::Enter), "Return");
        assert_eq!(char_keysym('{'), "braceleft");
        assert_eq!(char_keysym('€'), "U20AC");
        assert_eq!(char_keysym('7'), "7");
    }

    #[test]
    fn dbus_errors_are_classified() {
        let gone = zbus::Error::FDO(Box::new(fdo::Error::UnknownObject("x".into())));
        assert!(is_gone(&gone));
        let missing = zbus::Error::FDO(Box::new(fdo::Error::UnknownMethod("x".into())));
        assert!(is_missing_method(&missing));
        let slow = zbus::Error::FDO(Box::new(fdo::Error::Timeout("x".into())));
        assert!(is_timeout(&slow));
        assert_eq!(
            error::kind_of(&element_error("abc", "clicking", gone)),
            Some(ErrorKind::NotFound)
        );
        assert_eq!(
            error::kind_of(&element_error("abc", "clicking", slow)),
            Some(ErrorKind::Timeout)
        );
        assert!(!is_gone(&zbus::Error::Failure("boom".into())));
    }
}
