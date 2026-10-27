use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::{gio, glib};

use super::completion::{SchemaCache, SharedSchema};
use super::console::Console;
use super::loading_fx;
use super::picker::Picker;
use super::sidebar::Sidebar;
use super::query_tab::QueryTab;
use super::table_tab::TableTab;
use crate::config::ConnectionConfig;
use crate::db::log::QueryLog;
use crate::db::{self, DbKind, DbObject, ObjectKind, ObjectList, SharedDriver};
use crate::{runtime, secrets};

mod backup_menu;
mod database_menu;
mod object_menu;

enum Tab {
    Table(Rc<TableTab>),
    Query(Rc<QueryTab>),
}

/// The database and SSH passwords saved in the keyring for `cfg`.
pub async fn credentials(cfg: &ConnectionConfig) -> (Option<String>, Option<String>) {
    let lookup = |slot| {
        let id = cfg.id.clone();
        async move {
            secrets::get_password(&id, slot).await.unwrap_or_else(|e| {
                eprintln!("keyring unavailable: {e}");
                None
            })
        }
    };
    let password = if cfg.kind == DbKind::Sqlite { None } else { lookup(secrets::Slot::Database).await };
    let ssh_secret = if cfg.ssh.enabled && cfg.ssh.auth != crate::config::SshAuth::Agent {
        lookup(secrets::Slot::Ssh).await
    } else {
        None
    };
    (password, ssh_secret)
}

/// Fetch the passwords from the keyring and connect (optionally through SSH).
/// Everything the driver sends is recorded into `log`.
pub async fn open_driver(cfg: ConnectionConfig, log: Arc<QueryLog>) -> anyhow::Result<(SharedDriver, String)> {
    let (password, ssh_secret) = credentials(&cfg).await;
    let driver = db::logged(db::connect(&cfg, password, ssh_secret).await?, log);
    let info = driver.server_info().await.unwrap_or_default();
    Ok((driver, info))
}

/// Everything shown for one open connection: sidebar and tabs.
pub struct Workspace {
    pub widget: adw::BreakpointBin,
    actions: gio::SimpleActionGroup,
    config: RefCell<ConnectionConfig>,
    /// Shared with the sidebar, which loads columns through it.
    driver: Rc<RefCell<SharedDriver>>,
    /// Server line under the connection name in the sidebar header.
    subtitle: gtk::Label,
    tab_view: adw::TabView,
    content_stack: gtk::Stack,
    sidebar: Rc<Sidebar>,
    /// Query tabs showing a function's source, so clicking it again returns to its tab.
    routine_pages: RefCell<Vec<(adw::TabPage, DbObject)>>,
    sidebar_stack: gtk::Stack,
    search: gtk::SearchEntry,
    banner: adw::Banner,
    /// Top-bar button with a popover listing the server's databases.
    db_button: gtk::MenuButton,
    db_picker: Rc<Picker>,
    schema_button: gtk::MenuButton,
    /// Server description from the driver, e.g. "erp · PostgreSQL 17".
    server_info: RefCell<String>,
    schema_picker: Rc<Picker>,
    /// Schema shown in the sidebar (PostgreSQL).
    current_schema: Rc<RefCell<String>>,
    truncated: Cell<bool>,
    schema: SharedSchema,
    tabs: RefCell<Vec<(adw::TabPage, Tab)>>,
    query_counter: Cell<u32>,
    log: Arc<QueryLog>,
    console: Rc<Console>,
}

impl Workspace {
    pub fn new(config: ConnectionConfig, driver: SharedDriver, server_info: String, log: Arc<QueryLog>) -> Rc<Self> {
        // ---- Sidebar ----
        let search = gtk::SearchEntry::builder()
            .placeholder_text(if config.kind.is_sql() { "Search tables" } else { "Search keys" })
            .margin_start(8)
            .margin_end(8)
            .margin_top(4)
            .margin_bottom(6)
            .build();

        // Database switcher: an icon in the top bar opening a searchable list.
        let db_picker = Picker::new("Databases", "Search databases", "drive-harddisk-symbolic");
        let db_button = gtk::MenuButton::builder()
            .icon_name("drive-multidisk-symbolic")
            .tooltip_text("Switch Database")
            .popover(&db_picker.popover)
            .visible(false)
            .build();

        // Schema selector (PostgreSQL): the sidebar shows one schema at a time.
        let schema_picker = Picker::new("Schemas", "Search schemas", "folder-symbolic");
        let schema_button = gtk::MenuButton::builder()
            .icon_name("folder-symbolic")
            .popover(&schema_picker.popover)
            .tooltip_text("Switch Schema")
            .valign(gtk::Align::Center)
            .visible(config.kind == DbKind::Postgres)
            .css_classes(["flat", "header-small"])
            .build();

        let banner = adw::Banner::builder()
            .title("Showing the first 50,000 keys. Press Enter in the search box to scan the server.")
            .revealed(false)
            .build();

        let driver = Rc::new(RefCell::new(driver));
        let sidebar = Sidebar::new(config.kind, driver.clone());
        let sidebar_scroll = gtk::ScrolledWindow::builder()
            .child(&sidebar.list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let sidebar_stack = gtk::Stack::new();
        sidebar_stack.add_named(
            &adw::Spinner::builder()
                .width_request(32)
                .height_request(32)
                .halign(gtk::Align::Center)
                .valign(gtk::Align::Center)
                .build(),
            Some("loading"),
        );
        sidebar_stack.add_named(&sidebar_scroll, Some("list"));
        sidebar_stack.add_named(
            &adw::StatusPage::builder()
                .icon_name("edit-find-symbolic")
                .title("Nothing Here")
                .css_classes(["compact"])
                .build(),
            Some("empty"),
        );

        let sidebar_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar_box.append(&search);
        sidebar_box.append(&banner);
        sidebar_box.append(&sidebar_stack);

        let reload_objects = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Reload list")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "header-small"])
            .build();
        // Identity block: name + "database › schema · server" (the rail shows the avatar).
        let name = gtk::Label::builder()
            .label(config.display_name())
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["identity-name"])
            .build();
        let subtitle = gtk::Label::builder()
            .label(&server_info)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).valign(gtk::Align::Center).build();
        text.append(&name);
        text.append(&subtitle);
        let identity = gtk::Box::builder().margin_start(6).build();
        identity.append(&text);
        let sidebar_header = adw::HeaderBar::builder()
            .title_widget(&gtk::Label::new(None))
            .show_end_title_buttons(false)
            .css_classes(["fx-header"])
            .build();
        sidebar_header.pack_start(&identity);
        sidebar_header.pack_end(&reload_objects);
        sidebar_header.pack_end(&schema_button);
        // The header sits on an animated layer that comes alive while the connection works.
        let header_fx = gtk::Overlay::builder()
            .child(&loading_fx::new(log.clone(), config.effective_color().map(|c| c.rgb())))
            .build();
        header_fx.add_overlay(&sidebar_header);
        header_fx.set_measure_overlay(&sidebar_header, true);
        // A barely-there wash of the connection's colour behind its name.
        if let Some(color) = config.effective_color() {
            header_fx.add_css_class(&format!("identity-wash-{}", color.name()));
        }
        let sidebar_toolbar = adw::ToolbarView::new();
        sidebar_toolbar.add_top_bar(&header_fx);
        sidebar_toolbar.set_content(Some(&sidebar_box));

        // ---- Content ----
        let tab_view = adw::TabView::new();
        let tab_bar = adw::TabBar::builder().view(&tab_view).autohide(false).build();

        let new_query = gtk::Button::builder()
            .icon_name("tab-new-symbolic")
            .tooltip_text("New Query (Ctrl+T)")
            .action_name("win.new-query")
            .build();
        let toggle_sidebar = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-symbolic")
            .tooltip_text("Toggle Sidebar (F9)")
            .active(true)
            .build();
        // The environment tag sits in the middle of the top bar.
        let env_label = match config.tag {
            Some(tag) => gtk::Label::builder()
                .label(format!("● {}", tag.label()))
                .tooltip_text(format!("{} environment", tag.label()))
                .valign(gtk::Align::Center)
                .css_classes(["env-pill", &format!("env-{}", tag.color().name())])
                .build(),
            None => gtk::Label::new(None),
        };
        let content_header = adw::HeaderBar::builder()
            .show_start_title_buttons(false)
            .title_widget(&env_label)
            .build();
        content_header.pack_start(&toggle_sidebar);
        content_header.pack_start(&new_query);
        content_header.pack_start(&db_button);
        let console_button = gtk::ToggleButton::builder()
            .icon_name("utilities-terminal-symbolic")
            .tooltip_text("Console (Ctrl+Shift+C)")
            .build();
        content_header.pack_end(&console_button);
        // Backup & restore of the whole database; the actions live in the workspace's group.
        let actions = gio::SimpleActionGroup::new();
        let backup_menu = gio::Menu::new();
        backup_menu.append(Some("Back Up Database…"), Some("ws.backup"));
        backup_menu.append(Some("Restore Database…"), Some("ws.restore"));
        let backup_button = gtk::MenuButton::builder()
            .icon_name("document-save-symbolic")
            .tooltip_text("Backup & Restore")
            .menu_model(&backup_menu)
            .visible(config.kind.is_sql())
            .build();
        backup_button.insert_action_group("ws", Some(&actions));
        content_header.pack_end(&backup_button);

        let empty = adw::StatusPage::builder()
            .icon_name("x-office-spreadsheet-symbolic")
            .title("Open a Table")
            .description("Pick a table from the sidebar, or press Ctrl+T for a new query")
            .build();
        let content_stack = gtk::Stack::new();
        content_stack.add_named(&empty, Some("empty"));
        content_stack.add_named(&tab_view, Some("tabs"));

        // The console opens under the tabs, in a resizable split.
        let console = Console::new(log.clone(), config.kind);
        let content_split = gtk::Paned::builder()
            .orientation(gtk::Orientation::Vertical)
            .start_child(&content_stack)
            .end_child(&console.widget)
            .resize_end_child(false)
            .shrink_end_child(false)
            .shrink_start_child(false)
            .vexpand(true)
            .build();
        console_button.connect_toggled({
            let (console, split) = (console.clone(), content_split.downgrade());
            move |b| {
                let open = b.is_active();
                if open && let Some(split) = split.upgrade() {
                    // Start at about a third of the height; later drags are kept.
                    let height = split.height();
                    if split.position() == 0 || split.position() >= height - 40 {
                        split.set_position((height * 2 / 3).max(height - 320));
                    }
                }
                console.set_open(open);
            }
        });
        console.widget.connect_visible_notify({
            let button = console_button.downgrade();
            move |w| {
                if let Some(b) = button.upgrade() {
                    b.set_active(w.is_visible());
                }
            }
        });

        let content_toolbar = adw::ToolbarView::new();
        content_toolbar.add_top_bar(&content_header);
        content_toolbar.add_top_bar(&tab_bar);
        content_toolbar.set_content(Some(&content_split));

        let split = adw::OverlaySplitView::builder()
            .sidebar(&sidebar_toolbar)
            .content(&content_toolbar)
            .min_sidebar_width(220.0)
            .max_sidebar_width(360.0)
            .sidebar_width_fraction(0.22)
            .build();
        split.bind_property("show-sidebar", &toggle_sidebar, "active").bidirectional().sync_create().build();

        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            700.0,
            adw::LengthUnit::Sp,
        ));
        breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
        let widget = adw::BreakpointBin::builder().width_request(360).height_request(300).child(&split).build();
        widget.add_breakpoint(breakpoint);

        let config_schema = config.schema.trim().to_string();
        let ws = Rc::new(Workspace {
            widget,
            actions,
            config: RefCell::new(config),
            driver,
            subtitle,
            tab_view,
            content_stack,
            sidebar,
            routine_pages: RefCell::new(Vec::new()),
            sidebar_stack,
            search,
            banner,
            db_button,
            db_picker,
            schema_button,
            server_info: RefCell::new(server_info.clone()),
            schema_picker,
            current_schema: Rc::new(RefCell::new(
                if config_schema.is_empty() { "public".to_string() } else { config_schema },
            )),
            truncated: Cell::new(false),
            schema: Rc::new(RefCell::new(SchemaCache::default())),
            tabs: RefCell::new(Vec::new()),
            query_counter: Cell::new(0),
            log,
            console,
        });

        ws.setup_sidebar();
        ws.setup_database_management();
        ws.setup_actions(&split);
        ws.setup_paste_shortcut();
        ws.setup_tabs();

        let weak = Rc::downgrade(&ws);
        reload_objects.connect_clicked(move |_| {
            if let Some(ws) = weak.upgrade() {
                ws.load_objects(None);
            }
        });

        ws.load_objects(None);
        ws.load_databases();
        ws
    }

    pub fn config(&self) -> ConnectionConfig {
        self.config.borrow().clone()
    }

    /// Run one of this workspace's actions (the window forwards `win.*` shortcuts here).
    pub fn activate(&self, name: &str) {
        self.actions.activate_action(name, None);
    }

    pub fn has_changes(&self) -> bool {
        self.tabs.borrow().iter().any(|(_, t)| matches!(t, Tab::Table(t) if t.has_changes()))
    }

    /// Close all tabs so the connection can be dropped.
    pub fn shutdown(&self) {
        self.tabs.borrow_mut().clear();
    }

    fn driver(&self) -> SharedDriver {
        self.driver.borrow().clone()
    }

    fn kind(&self) -> DbKind {
        self.config.borrow().kind
    }

    fn setup_sidebar(self: &Rc<Self>) {
        let search = self.search.downgrade();
        let schema = (self.kind() == DbKind::Postgres).then(|| self.current_schema.clone());
        self.sidebar.set_filter(move |obj| {
            let Some(search) = search.upgrade() else { return true };
            if let Some(schema) = &schema
                && obj.schema != *schema.borrow()
            {
                return false;
            }
            let needle = search.text().to_lowercase();
            needle.is_empty() || obj.name.to_lowercase().contains(&needle)
        });
        // Sections open or closed as the connection was last left.
        let id = self.config.borrow().id.clone();
        self.sidebar.set_open_sections(crate::config::sidebar_sections(&id));
        self.sidebar.connect_sections_changed(move |sections| {
            if let Err(e) = crate::config::save_sidebar_sections(&id, sections) {
                eprintln!("could not save sidebar state: {e:#}");
            }
        });
        let weak = Rc::downgrade(self);
        self.sidebar.connect_open(move |obj| {
            if let Some(ws) = weak.upgrade() {
                ws.open_object(obj);
            }
        });
        let weak = Rc::downgrade(self);
        self.sidebar.connect_pins_changed(move |pins| {
            let Some(ws) = weak.upgrade() else { return };
            let cfg = ws.config.borrow();
            if let Err(e) = crate::config::save_pinned_objects(&cfg.id, &cfg.database, pins) {
                eprintln!("could not save pinned objects: {e:#}");
            }
        });
        let weak = Rc::downgrade(self);
        self.sidebar.connect_context_menu(move |obj, anchor, x, y| {
            if let Some(ws) = weak.upgrade() {
                ws.show_object_menu(obj, &anchor, x, y);
            }
        });
        let stack = self.sidebar_stack.downgrade();
        self.sidebar.connect_empty(move |empty| {
            let Some(stack) = stack.upgrade() else { return };
            if stack.visible_child_name().as_deref() != Some("loading") {
                stack.set_visible_child_name(if empty { "empty" } else { "list" });
            }
        });

        let weak = Rc::downgrade(self);
        self.search.connect_search_changed(move |_| {
            if let Some(ws) = weak.upgrade() {
                ws.sidebar.refilter();
            }
        });
        // Enter opens the first match, or scans the server when the key list is truncated.
        let weak = Rc::downgrade(self);
        self.search.connect_activate(move |entry| {
            let Some(ws) = weak.upgrade() else { return };
            // Enter can arrive before the debounced search-changed; filter now.
            ws.sidebar.refilter();
            let first = ws.sidebar.first_object();
            if ws.truncated.get() || (ws.kind() == DbKind::Redis && first.is_none()) {
                let text = entry.text().to_string();
                ws.load_objects(Some(text).filter(|t| !t.is_empty()));
                return;
            }
            if let Some(obj) = first {
                ws.open_object(obj);
            }
        });

        let weak = Rc::downgrade(self);
        self.db_picker.connect_pick(move |name| {
            if let Some(ws) = weak.upgrade() {
                ws.switch_database(name);
            }
        });
        let weak = Rc::downgrade(self);
        self.schema_picker.connect_pick(move |name| {
            if let Some(ws) = weak.upgrade() {
                ws.switch_schema(name);
            }
        });
    }

    /// Ctrl+V pastes rows into the open table even when focus is elsewhere (the sidebar,
    /// the database switcher…). Runs last (bubble phase), so the grid itself, text
    /// fields and the query editor still handle their own Ctrl+V first.
    fn setup_paste_shortcut(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let action = gtk::CallbackAction::new(move |_, _| {
            let Some(ws) = weak.upgrade() else { return glib::Propagation::Proceed };
            let pasted = ws.current_tab(|tab| match tab {
                Tab::Table(t) => {
                    t.paste_rows();
                    true
                }
                Tab::Query(_) => false,
            });
            if pasted == Some(true) { glib::Propagation::Stop } else { glib::Propagation::Proceed }
        });
        let controller = gtk::ShortcutController::new();
        controller.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string("<Control>v"), Some(action)));
        self.widget.add_controller(controller);
    }

    fn setup_actions(self: &Rc<Self>, split: &adw::OverlaySplitView) {
        let actions: [(&str, fn(&Rc<Workspace>)); 11] = [
            ("backup", |ws| ws.start_backup(ws.current_database())),
            ("restore", |ws| ws.start_restore(ws.current_database())),
            ("toggle-console", |ws| ws.console.set_open(!ws.console.is_open())),
            ("new-query", |ws| ws.new_query_tab("")),
            ("close-tab", |ws| {
                if let Some(page) = ws.tab_view.selected_page() {
                    ws.tab_view.close_page(&page);
                }
            }),
            ("refresh", |ws| ws.refresh_current()),
            ("find", |ws| ws.find()),
            ("save", |ws| {
                ws.current_tab(|tab| match tab {
                    Tab::Table(t) => t.commit(),
                    Tab::Query(q) => q.save_dialog(),
                });
            }),
            ("insert-row", |ws| {
                ws.current_tab(|tab| {
                    if let Tab::Table(t) = tab {
                        t.add_row();
                    }
                });
            }),
            ("toggle-inspector", |ws| {
                ws.current_tab(|tab| {
                    if let Tab::Table(t) = tab {
                        t.toggle_inspector();
                    }
                });
            }),
            ("stop", |ws| {
                ws.current_tab(|tab| {
                    if let Tab::Query(q) = tab {
                        q.cancel();
                    }
                });
            }),
        ];
        for (name, f) in actions {
            let action = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(ws) = weak.upgrade() {
                    f(&ws);
                }
            });
            self.actions.add_action(&action);
        }
        let action = gio::SimpleAction::new("toggle-sidebar", None);
        let split = split.downgrade();
        action.connect_activate(move |_, _| {
            if let Some(s) = split.upgrade() {
                s.set_show_sidebar(!s.shows_sidebar());
            }
        });
        self.actions.add_action(&action);
    }

    /// Highlight the open table in the sidebar (nothing for query tabs).
    fn sync_sidebar(&self) {
        let open = self.current_tab(|tab| match tab {
            Tab::Table(t) => Some(t.object.clone()),
            Tab::Query(_) => None,
        });
        let open = open.flatten().or_else(|| {
            let page = self.tab_view.selected_page()?;
            self.routine_pages.borrow().iter().find(|(p, _)| *p == page).map(|(_, o)| o.clone())
        });
        self.sidebar.select(open.as_ref());
    }

    fn setup_tabs(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.tab_view.connect_selected_page_notify(move |_| {
            if let Some(ws) = weak.upgrade() {
                ws.sync_sidebar();
            }
        });
        let weak = Rc::downgrade(self);
        self.tab_view.connect_close_page(move |view, page| {
            let Some(ws) = weak.upgrade() else {
                view.close_page_finish(page, true);
                return glib::Propagation::Stop;
            };
            let table = ws.tabs.borrow().iter().find_map(|(p, t)| match t {
                Tab::Table(t) if p == page && t.has_changes() => Some(t.clone()),
                _ => None,
            });
            match table {
                Some(t) => {
                    // Keep the tab open and ask; "Discard" closes it again without changes.
                    view.close_page_finish(page, false);
                    let (view, page) = (view.clone(), page.clone());
                    t.guard(move |_| view.close_page(&page));
                }
                None => {
                    ws.tabs.borrow_mut().retain(|(p, _)| p != page);
                    ws.routine_pages.borrow_mut().retain(|(p, _)| p != page);
                    view.close_page_finish(page, true);
                }
            }
            glib::Propagation::Stop
        });
        let stack = self.content_stack.clone();
        self.tab_view.connect_n_pages_notify(move |view| {
            stack.set_visible_child_name(if view.n_pages() == 0 { "empty" } else { "tabs" });
        });
    }

    fn load_objects(self: &Rc<Self>, pattern: Option<String>) {
        self.sidebar_stack.set_visible_child_name("loading");
        let driver = self.driver();
        let weak = Rc::downgrade(self);
        runtime::spawn(
            async move {
                let objects = driver.list_objects(pattern.as_deref()).await;
                let columns = driver.list_columns().await.unwrap_or_default();
                (objects, columns)
            },
            move |(res, columns)| {
                let Some(ws) = weak.upgrade() else { return };
                match res {
                    Ok(list) => ws.set_objects(list, columns),
                    Err(e) => {
                        ws.set_objects(ObjectList::default(), Vec::new());
                        ws.show_error("Could not load tables", &format!("{e:#}"));
                    }
                }
            },
        );
    }

    fn set_objects(&self, list: ObjectList, columns: Vec<(String, String)>) {
        {
            let mut schema = self.schema.borrow_mut();
            schema.tables = list.objects.iter().filter(|o| o.kind != ObjectKind::Key).map(|o| o.name.clone()).collect();
            schema.columns = columns;
        }
        if self.kind() == DbKind::Postgres {
            let mut schemas: Vec<String> = Vec::new();
            for o in &list.objects {
                if !schemas.contains(&o.schema) {
                    schemas.push(o.schema.clone());
                }
            }
            self.update_schemas(schemas);
        }
        let empty = list.objects.is_empty();
        // Pins belong to the database being shown (it can be switched).
        let (id, database) = {
            let cfg = self.config.borrow();
            (cfg.id.clone(), cfg.database.clone())
        };
        self.sidebar.set_pinned(crate::config::pinned_objects(&id, &database));
        self.sidebar.set_objects(list.objects);
        self.truncated.set(list.truncated);
        self.banner.set_revealed(list.truncated);
        self.sidebar_stack.set_visible_child_name(if empty { "empty" } else { "list" });
        self.sync_sidebar();
    }

    /// Keep the current schema if it still exists, else the configured default, else the first one.
    fn update_schemas(&self, schemas: Vec<String>) {
        let configured = self.config.borrow().schema.trim().to_string();
        let current = self.current_schema.borrow().clone();
        let chosen = [current, configured, "public".to_string()]
            .into_iter()
            .find(|s| !s.is_empty() && schemas.contains(s))
            .or_else(|| schemas.first().cloned());
        if let Some(chosen) = chosen {
            self.set_schema(chosen);
        }
        self.schema_picker.set_items(&schemas, &self.current_schema.borrow());
        self.schema_button.set_sensitive(schemas.len() > 1);
    }

    fn set_schema(&self, name: String) {
        let changed = *self.current_schema.borrow() != name;
        *self.current_schema.borrow_mut() = name.clone();
        self.schema_button.set_tooltip_text(Some(&format!("Switch Schema (current: {name})")));
        self.render_subtitle();
        self.sidebar.refilter();
        if changed || name != "public" {
            // Unqualified names in queries resolve to the selected schema first.
            let driver = self.driver();
            let sql = format!("SET search_path TO {}, public", db::quote_ident(DbKind::Postgres, &name));
            runtime::spawn(async move { driver.execute(&sql).await }, |res| {
                if let Err(e) = res {
                    eprintln!("could not set search_path: {e:#}");
                }
            });
        }
    }

    /// "erp · PostgreSQL 17" becomes "erp › public · PostgreSQL 17" for PostgreSQL.
    fn render_subtitle(&self) {
        let info = self.server_info.borrow().clone();
        let text = match (self.kind(), info.split_once(" · ")) {
            (DbKind::Postgres, Some((db, rest))) => format!("{db} › {} · {rest}", self.current_schema.borrow()),
            _ => info,
        };
        self.subtitle.set_text(&text);
    }

    fn switch_schema(&self, name: String) {
        self.set_schema(name);
        let schemas: Vec<String> = {
            let mut v = Vec::new();
            for o in self.sidebar.objects() {
                if !v.contains(&o.schema) {
                    v.push(o.schema);
                }
            }
            v
        };
        self.schema_picker.set_items(&schemas, &self.current_schema.borrow());
    }

    fn load_databases(self: &Rc<Self>) {
        let driver = self.driver();
        let weak = Rc::downgrade(self);
        runtime::spawn(async move { driver.list_databases().await }, move |res| {
            let Some(ws) = weak.upgrade() else { return };
            let names = res.unwrap_or_default();
            let current = ws.current_database();
            ws.db_picker.set_items(&names, &current);
            ws.db_button.set_tooltip_text(Some(&format!("Switch Database (current: {current})")));
            ws.db_button.set_visible(names.len() > 1 || ws.kind().manages_databases());
        });
    }

    /// The database this workspace is connected to, with the server's default filled in.
    fn current_database(&self) -> String {
        let cfg = self.config.borrow();
        match (cfg.kind, cfg.database.trim()) {
            (_, d) if !d.is_empty() => d.to_string(),
            (DbKind::Postgres, _) => "postgres".into(),
            (DbKind::Redis, _) => "0".into(),
            _ => String::new(),
        }
    }

    /// Reconnect to another database on the same server. Open tabs are closed.
    fn switch_database(self: &Rc<Self>, name: String) {
        if self.config.borrow().database == name {
            return;
        }
        if self.has_changes() {
            self.show_error("Unsaved Changes", "Commit or discard pending changes before switching databases.");
            self.load_databases();
            return;
        }
        let mut cfg = self.config.borrow().clone();
        cfg.database = name.clone();
        self.sidebar_stack.set_visible_child_name("loading");
        self.db_button.set_sensitive(false);
        let weak = Rc::downgrade(self);
        runtime::spawn(open_driver(cfg.clone(), self.log.clone()), move |res| {
            let Some(ws) = weak.upgrade() else { return };
            ws.db_button.set_sensitive(true);
            match res {
                Ok((driver, info)) => {
                    let pages: Vec<adw::TabPage> = ws.tabs.borrow().iter().map(|(p, _)| p.clone()).collect();
                    ws.tabs.borrow_mut().clear();
                    for page in pages {
                        ws.tab_view.close_page(&page);
                    }
                    *ws.driver.borrow_mut() = driver;
                    *ws.config.borrow_mut() = cfg;
                    *ws.server_info.borrow_mut() = info;
                    ws.render_subtitle();
                    ws.load_objects(None);
                    // Refresh the tick and tooltip in the database switcher.
                    ws.load_databases();
                }
                Err(e) => {
                    ws.show_error("Could not switch database", &format!("{e:#}"));
                    ws.sidebar_stack.set_visible_child_name("list");
                    ws.load_databases();
                }
            }
        });
    }

    fn open_object(self: &Rc<Self>, obj: DbObject) {
        self.open_object_tab(obj, false);
    }

    /// Open `obj`, focusing a tab that already shows it unless `new_tab`.
    fn open_object_tab(self: &Rc<Self>, obj: DbObject, new_tab: bool) {
        if obj.kind.is_routine() {
            self.open_routine(obj);
            return;
        }
        // Focus an existing tab for the same object.
        let existing = self.tabs.borrow().iter().find_map(|(page, tab)| match tab {
            Tab::Table(t) if t.object == obj => Some(page.clone()),
            _ => None,
        });
        if let (Some(page), false) = (existing, new_tab) {
            self.tab_view.set_selected_page(&page);
            return;
        }
        self.add_table_tab(obj, None);
    }

    /// Open `obj` in a new tab, optionally filtered to rows where (column) = (value).
    fn add_table_tab(self: &Rc<Self>, obj: DbObject, filter: Option<(&str, &str)>) {
        let tab = TableTab::new(self.driver(), obj.clone(), filter);
        let weak = Rc::downgrade(self);
        // Double-clicking a foreign-key value opens the referenced row in a new tab.
        tab.connect_open_reference(move |target, column, value| {
            if let Some(ws) = weak.upgrade() {
                ws.add_table_tab(target, Some((column, value)));
            }
        });
        let page = self.tab_view.append(&tab.widget);
        match filter {
            Some((column, value)) => page.set_title(&format!("{} ({column} = {value})", obj.name)),
            None => page.set_title(&obj.name),
        }
        page.set_tooltip(&format!("{}.{}", obj.schema, obj.name));
        page.set_icon(Some(&gio::ThemedIcon::new(obj.kind.icon_name())));
        self.tab_view.set_selected_page(&page);
        self.tabs.borrow_mut().push((page, Tab::Table(tab)));
        // The page switch fired before the tab was registered; highlight it now.
        self.sync_sidebar();
    }

    /// A function or procedure opens as its `CREATE` statement in a query tab, ready to edit and run.
    fn open_routine(self: &Rc<Self>, obj: DbObject) {
        let existing = self.routine_pages.borrow().iter().find(|(_, o)| *o == obj).map(|(p, _)| p.clone());
        if let Some(page) = existing {
            self.tab_view.set_selected_page(&page);
            return;
        }
        let driver = self.driver();
        let weak = Rc::downgrade(self);
        let target = obj.clone();
        runtime::spawn(async move { driver.definition(&target).await }, move |res| {
            let Some(ws) = weak.upgrade() else { return };
            match res {
                Ok(sql) => {
                    let page = ws.add_query_tab(sql.trim_end(), &obj.name, obj.kind.icon_name());
                    page.set_tooltip(&format!("{}.{}({})", obj.schema, obj.name, obj.signature));
                    ws.routine_pages.borrow_mut().push((page, obj));
                    ws.sync_sidebar();
                }
                Err(e) => ws.show_error("Could not open the function", &format!("{e:#}")),
            }
        });
    }

    pub fn new_query_tab(self: &Rc<Self>, text: &str) {
        let n = self.query_counter.get() + 1;
        self.query_counter.set(n);
        self.add_query_tab(text, &format!("Query {n}"), "utilities-terminal-symbolic");
    }

    fn add_query_tab(self: &Rc<Self>, text: &str, title: &str, icon: &str) -> adw::TabPage {
        let tab = QueryTab::new(
            self.driver(),
            self.kind(),
            self.config.borrow().id.clone(),
            self.schema.clone(),
        );
        tab.set_text(text);
        let page = self.tab_view.append(&tab.widget);
        page.set_title(title);
        page.set_icon(Some(&gio::ThemedIcon::new(icon)));
        self.tab_view.set_selected_page(&page);
        tab.focus();
        self.tabs.borrow_mut().push((page.clone(), Tab::Query(tab)));
        page
    }

    fn current_tab<R>(&self, f: impl FnOnce(&Tab) -> R) -> Option<R> {
        let page = self.tab_view.selected_page()?;
        let tab = self.tabs.borrow().iter().find(|(p, _)| *p == page).map(|(_, t)| match t {
            Tab::Table(t) => Tab::Table(t.clone()),
            Tab::Query(q) => Tab::Query(q.clone()),
        })?;
        Some(f(&tab))
    }

    fn refresh_current(self: &Rc<Self>) {
        let handled = self.current_tab(|tab| match tab {
            Tab::Table(t) => t.reload(),
            Tab::Query(q) => q.run_current(),
        });
        if handled.is_none() {
            self.load_objects(None);
        }
    }

    /// Ctrl+F: focus the table filter if a table is open, otherwise the sidebar search.
    fn find(&self) {
        let focused = self.current_tab(|tab| match tab {
            Tab::Table(t) if t.object.kind != ObjectKind::Key => {
                t.focus_filter();
                true
            }
            _ => false,
        });
        if focused != Some(true) {
            self.search.grab_focus();
        }
    }

    fn show_error(&self, heading: &str, body: &str) {
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.add_response("ok", "OK");
        dialog.present(Some(&self.widget));
    }
}
