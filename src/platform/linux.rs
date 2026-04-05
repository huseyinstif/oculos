use anyhow::{anyhow, Context, Result};
use dashmap::DashMap;
use std::sync::Arc;
use uuid::Uuid;

use atspi::{
    proxy::accessible::AccessibleProxy, proxy::action::ActionProxy,
    proxy::application::ApplicationProxy, proxy::component::ComponentProxy,
    proxy::editable_text::EditableTextProxy, proxy::text::TextProxy, proxy::value::ValueProxy,
    CoordType, Role, State,
};
use zbus::names::BusName;
use zbus::zvariant::ObjectPath;
use zbus::Connection;

use crate::{
    platform::UiBackend,
    types::{ElementType, ExpandState, RangeInfo, Rect, ToggleState, UiElement, WindowInfo},
};

// ── Element registry ──────────────────────────────────────────────────────────

struct StoredElement {
    bus_name: String,
    object_path: String,
}

type IdRegistry = Arc<DashMap<String, StoredElement>>;

// ── Helpers for zbus type conversions ─────────────────────────────────────────

fn bus_name(s: &str) -> BusName<'_> {
    BusName::try_from(s).unwrap_or_else(|_| BusName::try_from(":0.0").unwrap())
}

fn obj_path(s: &str) -> ObjectPath<'_> {
    ObjectPath::try_from(s).unwrap_or_else(|_| ObjectPath::try_from("/").unwrap())
}

// ── Backend ───────────────────────────────────────────────────────────────────

pub struct LinuxUiBackend {
    connection: Connection,
    registry: IdRegistry,
    rt: Arc<tokio::runtime::Runtime>,
}

impl Drop for LinuxUiBackend {
    fn drop(&mut self) {
        let rt = Arc::clone(&self.rt);
        std::thread::spawn(move || drop(rt));
    }
}

impl LinuxUiBackend {
    pub fn new() -> Result<Self> {
        fn atspi_init_thread() -> Result<(tokio::runtime::Runtime, Connection)> {
            let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("Failed to create dedicated Tokio runtime for AT-SPI2")?;

            let connection = rt.block_on(async {
                let atspi_address: String = {
                    let session = Connection::session()
                        .await
                        .context("Failed to connect to D-Bus session bus")?;
            
                    let addr: String = session
                        .call_method(
                            Some("org.a11y.Bus"),
                            "/org/a11y/bus",
                            Some("org.a11y.Bus"),
                            "GetAddress",
                            &(),
                        )
                        .await
                        .context("Failed to get AT-SPI bus address from org.a11y.Bus")?
                        .body::<String>()
                        .context("Failed to deserialize AT-SPI bus address")?;
            
                    drop(session); // explicitly drop before connecting to AT-SPI bus
                    addr
                };
            
                tracing::info!("Connecting to AT-SPI2 bus at {}", atspi_address);
            
                zbus::ConnectionBuilder::address(atspi_address.as_str())?
                    .build()
                    .await
                    .context("Failed to connect to AT-SPI2 accessibility bus")
            })?;

            Ok((rt, connection))
        }

        let handle = std::thread::spawn(atspi_init_thread);

        let (rt, connection) = handle
            .join()
            .map_err(|_| anyhow!("AT-SPI2 init thread panicked"))??;

        tracing::info!("Connected to AT-SPI2 accessibility bus");

        Ok(Self {
            connection,
            registry: Arc::new(DashMap::new()),
            rt: Arc::new(rt),
        })
    }

    // ── Role → ElementType mapping ────────────────────────────────────────

    fn role_to_element_type(role: Role) -> ElementType {
        match role {
            Role::Frame | Role::Window => ElementType::Window,
            Role::PushButton | Role::ToggleButton => ElementType::Button,
            Role::Text | Role::Entry | Role::PasswordText | Role::SpinButton => ElementType::Edit,
            Role::Label | Role::Static | Role::Heading | Role::Paragraph => ElementType::Text,
            Role::CheckBox | Role::CheckMenuItem => ElementType::CheckBox,
            Role::RadioButton | Role::RadioMenuItem => ElementType::RadioButton,
            Role::ComboBox => ElementType::ComboBox,
            Role::List => ElementType::ListBox,
            Role::ListItem => ElementType::ListItem,
            Role::Tree | Role::TreeTable => ElementType::TreeView,
            Role::TreeItem => ElementType::TreeItem,
            Role::Menu | Role::MenuBar => ElementType::Menu,
            Role::MenuItem => ElementType::MenuItem,
            Role::PageTabList => ElementType::TabControl,
            Role::PageTab => ElementType::TabItem,
            Role::ToolBar => ElementType::ToolBar,
            Role::StatusBar => ElementType::StatusBar,
            Role::ScrollBar => ElementType::ScrollBar,
            Role::Slider => ElementType::Slider,
            Role::ProgressBar => ElementType::ProgressBar,
            Role::Image | Role::Icon => ElementType::Image,
            Role::Link => ElementType::Link,
            Role::Panel | Role::Filler => ElementType::Group,
            Role::ScrollPane => ElementType::Pane,
            Role::Dialog | Role::Alert | Role::FileChooser => ElementType::Dialog,
            Role::DocumentWeb => ElementType::Document,
            Role::Table => ElementType::Table,
            _ => ElementType::Unknown,
        }
    }

    // ── Async helpers ─────────────────────────────────────────────────────

    async fn make_accessible_proxy<'a>(
        conn: &'a Connection,
        bname: &'a str,
        opath: &'a str,
    ) -> Result<AccessibleProxy<'a>> {
        AccessibleProxy::builder(conn)
            .destination(bus_name(bname))?
            .path(obj_path(opath))?
            .build()
            .await
            .context("Failed to build AccessibleProxy")
    }

    async fn make_component_proxy<'a>(
        conn: &'a Connection,
        bname: &'a str,
        opath: &'a str,
    ) -> Result<ComponentProxy<'a>> {
        ComponentProxy::builder(conn)
            .destination(bus_name(bname))?
            .path(obj_path(opath))?
            .build()
            .await
            .context("Failed to build ComponentProxy")
    }

    async fn make_action_proxy<'a>(
        conn: &'a Connection,
        bname: &'a str,
        opath: &'a str,
    ) -> Result<ActionProxy<'a>> {
        ActionProxy::builder(conn)
            .destination(bus_name(bname))?
            .path(obj_path(opath))?
            .build()
            .await
            .context("Failed to build ActionProxy")
    }

    async fn make_application_proxy<'a>(
        conn: &'a Connection,
        bname: &'a str,
        opath: &'a str,
    ) -> Result<ApplicationProxy<'a>> {
        ApplicationProxy::builder(conn)
            .destination(bus_name(bname))?
            .path(obj_path(opath))?
            .build()
            .await
            .context("Failed to build ApplicationProxy")
    }

    async fn make_text_proxy<'a>(
        conn: &'a Connection,
        bname: &'a str,
        opath: &'a str,
    ) -> Result<TextProxy<'a>> {
        TextProxy::builder(conn)
            .destination(bus_name(bname))?
            .path(obj_path(opath))?
            .build()
            .await
            .context("Failed to build TextProxy")
    }

    async fn make_value_proxy<'a>(
        conn: &'a Connection,
        bname: &'a str,
        opath: &'a str,
    ) -> Result<ValueProxy<'a>> {
        ValueProxy::builder(conn)
            .destination(bus_name(bname))?
            .path(obj_path(opath))?
            .build()
            .await
            .context("Failed to build ValueProxy")
    }

    async fn make_editable_text_proxy<'a>(
        conn: &'a Connection,
        bname: &'a str,
        opath: &'a str,
    ) -> Result<EditableTextProxy<'a>> {
        EditableTextProxy::builder(conn)
            .destination(bus_name(bname))?
            .path(obj_path(opath))?
            .build()
            .await
            .context("Failed to build EditableTextProxy")
    }

    // ── Build element ─────────────────────────────────────────────────────

    async fn build_element_async(
        &self,
        bname: &str,
        opath: &str,
        with_children: bool,
        depth: u32,
    ) -> Result<UiElement> {
        let zero_rect = Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        };

        if depth > 48 {
            let id = Uuid::new_v4().to_string();
            self.registry.insert(
                id.clone(),
                StoredElement {
                    bus_name: bname.to_string(),
                    object_path: opath.to_string(),
                },
            );
            return Ok(UiElement {
                oculos_id: id,
                element_type: ElementType::Unknown,
                label: String::new(),
                value: None,
                text_content: None,
                rect: zero_rect,
                enabled: false,
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
                actions: vec![],
                children: vec![],
            });
        }

        let proxy = Self::make_accessible_proxy(&self.connection, bname, opath).await?;

        let name = proxy.name().await.unwrap_or_default();
        let role = proxy.get_role().await.unwrap_or(Role::Invalid);
        let element_type = Self::role_to_element_type(role);

        // State set
        let states = proxy.get_state().await.unwrap_or_default();
        let enabled = states.contains(State::Enabled);
        let focused = states.contains(State::Focused);
        let is_keyboard_focusable = states.contains(State::Focusable);
        let is_selected_state = states.contains(State::Selected);
        let is_checked = states.contains(State::Checked);
        let is_expanded = states.contains(State::Expanded);
        let is_expandable = states.contains(State::Expandable);

        // Bounding box via Component interface
        let rect =
            if let Ok(comp) = Self::make_component_proxy(&self.connection, bname, opath).await {
                if let Ok(extents) = comp.get_extents(CoordType::Screen).await {
                    Rect {
                        x: extents.0,
                        y: extents.1,
                        width: extents.2,
                        height: extents.3,
                    }
                } else {
                    zero_rect
                }
            } else {
                zero_rect
            };

        // Value (for text fields, sliders, etc.)
        let value = self.get_text_value(bname, opath).await;

        let toggle_state = if element_type == ElementType::CheckBox {
            Some(if is_checked {
                ToggleState::On
            } else {
                ToggleState::Off
            })
        } else {
            None
        };

        let is_selected = if is_selected_state { Some(true) } else { None };

        let expand_state = if is_expandable {
            Some(if is_expanded {
                ExpandState::Expanded
            } else {
                ExpandState::Collapsed
            })
        } else {
            None
        };

        let range = self.get_range_info(bname, opath).await;
        let actions = self
            .collect_actions(bname, opath, &element_type, is_keyboard_focusable)
            .await;
        let help_text = proxy.description().await.ok().filter(|s| !s.is_empty());

        // Children
        let children = if with_children {
            let child_count = proxy.child_count().await.unwrap_or(0);
            let mut kids = Vec::with_capacity(child_count as usize);
            for i in 0..child_count {
                if let Ok(child) = proxy.get_child_at_index(i).await {
                    let cb = child.name.clone();
                    let cp = child.path.to_string();
                    if let Ok(elem) =
                        Box::pin(self.build_element_async(&cb, &cp, true, depth + 1)).await
                    {
                        kids.push(elem);
                    }
                }
            }
            kids
        } else {
            vec![]
        };

        let oculos_id = Uuid::new_v4().to_string();
        self.registry.insert(
            oculos_id.clone(),
            StoredElement {
                bus_name: bname.to_string(),
                object_path: opath.to_string(),
            },
        );

        Ok(UiElement {
            oculos_id,
            element_type,
            label: name,
            value,
            text_content: None,
            rect,
            enabled,
            focused,
            is_keyboard_focusable,
            toggle_state,
            is_selected,
            expand_state,
            range,
            automation_id: None,
            class_name: None,
            help_text,
            keyboard_shortcut: None,
            actions,
            children,
        })
    }

    async fn get_text_value(&self, bname: &str, opath: &str) -> Option<String> {
        let tp = Self::make_text_proxy(&self.connection, bname, opath)
            .await
            .ok()?;
        let cc = tp.character_count().await.ok()?;
        if cc == 0 {
            return None;
        }
        tp.get_text(0, cc).await.ok().filter(|s| !s.is_empty())
    }

    async fn get_range_info(&self, bname: &str, opath: &str) -> Option<RangeInfo> {
        let vp = Self::make_value_proxy(&self.connection, bname, opath)
            .await
            .ok()?;
        let current = vp.current_value().await.ok()?;
        let minimum = vp.minimum_value().await.unwrap_or(0.0);
        let maximum = vp.maximum_value().await.unwrap_or(100.0);
        let step = vp.minimum_increment().await.unwrap_or(1.0);
        Some(RangeInfo {
            value: current,
            minimum,
            maximum,
            step,
            read_only: false,
        })
    }

    async fn collect_actions(
        &self,
        bname: &str,
        opath: &str,
        element_type: &ElementType,
        focusable: bool,
    ) -> Vec<String> {
        let mut actions = Vec::new();

        if let Ok(ap) = Self::make_action_proxy(&self.connection, bname, opath).await {
            if let Ok(action_list) = ap.get_actions().await {
                for (i, (name, _desc, _kb)) in action_list.iter().enumerate() {
                    match name.as_str() {
                        "click" | "press" | "activate" => {
                            if !actions.contains(&"click".to_string()) {
                                actions.push("click".into());
                            }
                        }
                        "toggle" => actions.push("toggle".into()),
                        "expand or contract" | "expand" => actions.push("expand".into()),
                        "collapse" => actions.push("collapse".into()),
                        _ => {}
                    }
                    let _ = i;
                }
            }
        }

        if Self::make_editable_text_proxy(&self.connection, bname, opath)
            .await
            .is_ok()
        {
            actions.push("set-text".into());
            actions.push("send-keys".into());
        }

        if matches!(element_type, ElementType::Slider | ElementType::ProgressBar) {
            if Self::make_value_proxy(&self.connection, bname, opath)
                .await
                .is_ok()
            {
                actions.push("set-range".into());
            }
        }

        if focusable {
            actions.push("focus".into());
        }

        actions
    }

    // ── Search helper ─────────────────────────────────────────────────────

    async fn search_elements_async(
        &self,
        bname: &str,
        opath: &str,
        query: Option<&str>,
        element_type: Option<&ElementType>,
        interactive_only: bool,
        results: &mut Vec<UiElement>,
        depth: u32,
    ) {
        if depth > 48 || results.len() >= 500 {
            return;
        }

        if let Ok(elem) = self.build_element_async(bname, opath, false, depth).await {
            let query_lower = query.map(|q| q.to_lowercase());
            let mut matches = true;

            if let Some(ref q) = query_lower {
                let label_match = elem.label.to_lowercase().contains(q.as_str());
                let aid_match = elem
                    .automation_id
                    .as_ref()
                    .map(|a| a.to_lowercase().contains(q.as_str()))
                    .unwrap_or(false);
                if !label_match && !aid_match {
                    matches = false;
                }
            }
            if let Some(wanted) = element_type {
                if &elem.element_type != wanted {
                    matches = false;
                }
            }
            if interactive_only && elem.actions.is_empty() {
                matches = false;
            }
            if matches {
                results.push(elem);
            }
        }

        if let Ok(proxy) = Self::make_accessible_proxy(&self.connection, bname, opath).await {
            let child_count = proxy.child_count().await.unwrap_or(0);
            for i in 0..child_count {
                if let Ok(child) = proxy.get_child_at_index(i).await {
                    let cb = child.name.clone();
                    let cp = child.path.to_string();
                    Box::pin(self.search_elements_async(
                        &cb,
                        &cp,
                        query,
                        element_type,
                        interactive_only,
                        results,
                        depth + 1,
                    ))
                    .await;
                }
            }
        }
    }

    // ── Find app root for a PID ───────────────────────────────────────────

    async fn find_app_root(&self, pid: u32) -> Result<(String, String)> {
        let registry = Self::make_accessible_proxy(
            &self.connection,
            "org.a11y.atspi.Registry",
            "/org/a11y/atspi/accessible/root",
        )
        .await
        .context("Failed to connect to AT-SPI2 registry")?;

        let child_count = registry.child_count().await.unwrap_or(0);
        for i in 0..child_count {
            if let Ok(child) = registry.get_child_at_index(i).await {
                let cb = child.name.clone();
                let cp = child.path.to_string();

                if let Ok(app_proxy) =
                    Self::make_application_proxy(&self.connection, &cb, &cp).await
                {
                    if let Ok(p) = app_proxy.id().await {
                        if p as u32 == pid {
                            return Ok((cb, cp));
                        }
                    }
                }
            }
        }

        Err(anyhow!("No AT-SPI2 application found for PID {}", pid))
    }

    async fn get_component_rect(&self, bname: &str, opath: &str) -> Rect {
        if let Ok(comp) = Self::make_component_proxy(&self.connection, bname, opath).await {
            if let Ok(extents) = comp.get_extents(CoordType::Screen).await {
                return Rect {
                    x: extents.0,
                    y: extents.1,
                    width: extents.2,
                    height: extents.3,
                };
            }
        }
        Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        }
    }

    // ── Sync wrappers ─────────────────────────────────────────────────────

    fn block_on<F: std::future::Future<Output = T>, T>(&self, f: F) -> T {
        self.rt.block_on(f)
    }
}

// ── UiBackend implementation ──────────────────────────────────────────────────

impl UiBackend for LinuxUiBackend {
    fn list_windows(&self) -> Result<Vec<WindowInfo>> {
        self.block_on(async {
            // Use GetChildren directly — child_count() property returns 0 on this atspi version
            let children: Vec<(String, zbus::zvariant::OwnedObjectPath)> = self
                .connection
                .call_method(
                    Some("org.a11y.atspi.Registry"),
                    "/org/a11y/atspi/accessible/root",
                    Some("org.a11y.atspi.Accessible"),
                    "GetChildren",
                    &(),
                )
                .await
                .context("Failed to call GetChildren on AT-SPI registry")?
                .body::<Vec<(String, zbus::zvariant::OwnedObjectPath)>>()
                .context("Failed to deserialize children")?;
    
            tracing::info!("Registry GetChildren returned {} children", children.len());
    
            let mut windows = Vec::new();
    
            for (cb, cp) in &children {
                let cp_str = cp.as_str();
    
                // Get app name
                let app_proxy = match Self::make_accessible_proxy(&self.connection, cb, cp_str).await {
                    Ok(p) => p,
                    Err(_) => continue,
                };
    
                let app_name = app_proxy.name().await.unwrap_or_default();
                if app_name.is_empty() {
                    continue;
                }
    
                // Get PID via Application interface
                let pid = async {
                    let msg = self.connection
                        .call_method(
                            Some(cb.as_str()),
                            cp_str,
                            Some("org.freedesktop.DBus.Properties"),
                            "Get",
                            &("org.a11y.atspi.Application", "Id"),
                        )
                        .await
                        .ok()?;
                    let v = msg.body::<zbus::zvariant::Value>().ok()?;
                    match v {
                        zbus::zvariant::Value::I32(n) => Some(n as u32),
                        zbus::zvariant::Value::U32(n) => Some(n),
                        zbus::zvariant::Value::I64(n) => Some(n as u32),
                        _ => None,
                    }
                }.await.unwrap_or(0);
    
                // Get children of this app (its windows)
                let app_children: Vec<(String, zbus::zvariant::OwnedObjectPath)> = match self
                    .connection
                    .call_method(
                        Some(cb.as_str()),
                        cp_str,
                        Some("org.a11y.atspi.Accessible"),
                        "GetChildren",
                        &(),
                    )
                    .await
                    .and_then(|r| r.body::<Vec<(String, zbus::zvariant::OwnedObjectPath)>>().map_err(Into::into))
                {
                    Ok(c) => c,
                    Err(_) => vec![],
                };
    
                let mut found_window = false;
    
                for (wb, wp) in &app_children {
                    let wp_str = wp.as_str();
    
                    if let Ok(win_proxy) =
                        Self::make_accessible_proxy(&self.connection, wb, wp_str).await
                    {
                        let role = win_proxy.get_role().await.unwrap_or(Role::Invalid);
                        if matches!(role, Role::Frame | Role::Window | Role::Dialog) {
                            let title = win_proxy.name().await.unwrap_or_default();
                            let rect = self.get_component_rect(wb, wp_str).await;
                            windows.push(WindowInfo {
                                pid,
                                hwnd: 0,
                                title,
                                exe_name: app_name.clone(),
                                rect,
                                visible: true,
                            });
                            found_window = true;
                        }
                    }
                }
    
                // Fallback: if no Frame/Window child found, register the app itself
                if !found_window && pid > 0 {
                    windows.push(WindowInfo {
                        pid,
                        hwnd: 0,
                        title: app_name.clone(),
                        exe_name: app_name,
                        rect: Rect {
                            x: 0,
                            y: 0,
                            width: 0,
                            height: 0,
                        },
                        visible: true,
                    });
                }
            }
    
            Ok(windows)
        })
    }

    fn get_ui_tree(&self, pid: u32) -> Result<UiElement> {
        self.block_on(async {
            let (bus, path) = self.find_app_root(pid).await?;
            self.build_element_async(&bus, &path, true, 0).await
        })
    }

    fn get_ui_tree_hwnd(&self, _hwnd: usize) -> Result<UiElement> {
        Err(anyhow!(
            "Linux does not use window handles (HWND). Use the PID-based endpoint instead."
        ))
    }

    fn find_elements(
        &self,
        pid: u32,
        query: Option<&str>,
        element_type: Option<&ElementType>,
        interactive_only: bool,
    ) -> Result<Vec<UiElement>> {
        self.block_on(async {
            let (bus, path) = self.find_app_root(pid).await?;
            let mut results = Vec::new();
            self.search_elements_async(
                &bus,
                &path,
                query,
                element_type,
                interactive_only,
                &mut results,
                0,
            )
            .await;
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
        Err(anyhow!(
            "Linux does not use window handles (HWND). Use the PID-based endpoint instead."
        ))
    }

    fn click_element(&self, oculos_id: &str) -> Result<()> {
        let (bname, opath) = self.get_stored(oculos_id)?;
        self.block_on(async {
            let ap = Self::make_action_proxy(&self.connection, &bname, &opath)
                .await
                .context("Element does not support Action interface")?;

            let action_list = ap.get_actions().await.unwrap_or_default();
            for (i, (name, _, _)) in action_list.iter().enumerate() {
                if matches!(name.as_str(), "click" | "press" | "activate") {
                    ap.do_action(i as i32).await?;
                    return Ok(());
                }
            }
            if !action_list.is_empty() {
                ap.do_action(0).await?;
                return Ok(());
            }
            Err(anyhow!(
                "No clickable action found on element '{}'",
                oculos_id
            ))
        })
    }

    fn set_text(&self, oculos_id: &str, text: &str) -> Result<()> {
        let (bname, opath) = self.get_stored(oculos_id)?;
        self.block_on(async {
            let ep = Self::make_editable_text_proxy(&self.connection, &bname, &opath)
                .await
                .context("Element does not support EditableText interface")?;
            let tp = Self::make_text_proxy(&self.connection, &bname, &opath)
                .await
                .context("Element does not support Text interface")?;

            let cc = tp.character_count().await.unwrap_or(0);
            if cc > 0 {
                let _ = ep.delete_text(0, cc).await;
            }
            ep.insert_text(0, text, text.len() as i32).await?;
            Ok(())
        })
    }

    fn send_keys(&self, oculos_id: &str, text: &str) -> Result<()> {
        self.focus_element(oculos_id)?;
        std::thread::sleep(std::time::Duration::from_millis(60));
        send_key_sequence_linux(text);
        Ok(())
    }

    fn focus_element(&self, oculos_id: &str) -> Result<()> {
        let (bname, opath) = self.get_stored(oculos_id)?;
        self.block_on(async {
            let cp = Self::make_component_proxy(&self.connection, &bname, &opath)
                .await
                .context("Element does not support Component interface")?;
            cp.grab_focus().await?;
            Ok(())
        })
    }

    fn toggle_element(&self, oculos_id: &str) -> Result<()> {
        self.click_element(oculos_id)
    }

    fn expand_element(&self, oculos_id: &str) -> Result<()> {
        let (bname, opath) = self.get_stored(oculos_id)?;
        self.block_on(async {
            let ap = Self::make_action_proxy(&self.connection, &bname, &opath)
                .await
                .context("Element does not support Action interface")?;
            let action_list = ap.get_actions().await.unwrap_or_default();
            for (i, (name, _, _)) in action_list.iter().enumerate() {
                if matches!(name.as_str(), "expand or contract" | "expand" | "open") {
                    ap.do_action(i as i32).await?;
                    return Ok(());
                }
            }
            Err(anyhow!("No expand action found on element '{}'", oculos_id))
        })
    }

    fn collapse_element(&self, oculos_id: &str) -> Result<()> {
        self.expand_element(oculos_id)
    }
    fn select_element(&self, oculos_id: &str) -> Result<()> {
        self.click_element(oculos_id)
    }

    fn set_range(&self, oculos_id: &str, value: f64) -> Result<()> {
        let (bname, opath) = self.get_stored(oculos_id)?;
        self.block_on(async {
            let vp = Self::make_value_proxy(&self.connection, &bname, &opath)
                .await
                .context("Element does not support Value interface")?;
            vp.set_current_value(value).await?;
            Ok(())
        })
    }

    fn scroll_element(&self, oculos_id: &str, direction: &str) -> Result<()> {
        let key = match direction {
            "up" => "Up",
            "down" => "Down",
            "left" => "Left",
            "right" => "Right",
            "page-up" => "Page_Up",
            "page-down" => "Page_Down",
            other => return Err(anyhow!("Unknown scroll direction '{}'", other)),
        };
        self.focus_element(oculos_id)?;
        std::thread::sleep(std::time::Duration::from_millis(30));
        send_key_sequence_linux(&format!("{{{}}}", key));
        Ok(())
    }

    fn scroll_into_view(&self, _oculos_id: &str) -> Result<()> {
        Err(anyhow!(
            "scroll-into-view is not natively supported on Linux AT-SPI2."
        ))
    }

    fn focus_window(&self, pid: u32) -> Result<()> {
        let output = std::process::Command::new("xdotool")
            .args([
                "search",
                "--pid",
                &pid.to_string(),
                "--onlyvisible",
                "windowactivate",
            ])
            .output();
        match output {
            Ok(o) if o.status.success() => Ok(()),
            _ => {
                let _ = std::process::Command::new("wmctrl")
                    .args(["-i", "-a", &format!("0x{:08x}", pid)])
                    .output();
                Ok(())
            }
        }
    }

    fn close_window(&self, pid: u32) -> Result<()> {
        let output = std::process::Command::new("xdotool")
            .args([
                "search",
                "--pid",
                &pid.to_string(),
                "--onlyvisible",
                "windowclose",
            ])
            .output();
        match output {
            Ok(o) if o.status.success() => Ok(()),
            _ => Err(anyhow!(
                "Failed to close window for PID {}. Is xdotool installed?",
                pid
            )),
        }
    }
}

// ── Registry helper ───────────────────────────────────────────────────────────

impl LinuxUiBackend {
    fn get_stored(&self, oculos_id: &str) -> Result<(String, String)> {
        let entry = self
            .registry
            .get(oculos_id)
            .ok_or_else(|| anyhow!("Element '{}' not found in registry", oculos_id))?;
        let bname = entry.value().bus_name.clone();
        let opath = entry.value().object_path.clone();
        drop(entry);
        Ok((bname, opath))
    }
}

// ── Linux keyboard simulation via xdotool ─────────────────────────────────────

fn send_key_sequence_linux(text: &str) {
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '{' {
            let mut key_name = String::new();
            while let Some(&c) = chars.peek() {
                chars.next();
                if c == '}' {
                    break;
                }
                key_name.push(c);
            }
            send_special_key_linux(&key_name);
        } else {
            let _ = std::process::Command::new("xdotool")
                .args(["type", "--clearmodifiers", &ch.to_string()])
                .output();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

fn send_special_key_linux(key_name: &str) {
    let xdotool_key = match key_name {
        "ENTER" | "RETURN" => "Return",
        "TAB" => "Tab",
        "ESC" | "ESCAPE" => "Escape",
        "SPACE" => "space",
        "DELETE" => "Delete",
        "BACKSPACE" => "BackSpace",
        "UP" => "Up",
        "DOWN" => "Down",
        "LEFT" => "Left",
        "RIGHT" => "Right",
        "HOME" => "Home",
        "END" => "End",
        "PGUP" => "Page_Up",
        "PGDN" => "Page_Down",
        "F1" => "F1",
        "F2" => "F2",
        "F3" => "F3",
        "F4" => "F4",
        "F5" => "F5",
        "F6" => "F6",
        "F7" => "F7",
        "F8" => "F8",
        "F9" => "F9",
        "F10" => "F10",
        "F11" => "F11",
        "F12" => "F12",
        s if s.contains('+') => {
            let parts: Vec<&str> = s.splitn(2, '+').collect();
            let modifier = match parts[0] {
                "CTRL" => "ctrl",
                "ALT" => "alt",
                "SHIFT" => "shift",
                "WIN" | "SUPER" => "super",
                other => other,
            };
            let key = parts.get(1).unwrap_or(&"").to_lowercase();
            let combo = format!("{}+{}", modifier, key);
            let _ = std::process::Command::new("xdotool")
                .args(["key", "--clearmodifiers", &combo])
                .output();
            return;
        },
        _ => return,
    };

    let _ = std::process::Command::new("xdotool")
        .args(["key", "--clearmodifiers", xdotool_key])
        .output();
    std::thread::sleep(std::time::Duration::from_millis(20));
}
