//! Slint callbacks and asynchronous database operations. UI state stays on the event-loop thread.
use super::{completion::SchemaCache, model::*, *};
use crate::{
    config::{self, Color, ConnectionConfig, SshAuth, Store, Tag},
    db::{self, DbKind, FetchOptions},
    export, filter, library, runtime, secrets,
};
use anyhow::{Context, Result, ensure};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::{cell::RefCell, path::PathBuf, rc::Rc, time::Instant};

type App = Rc<Controller>;
type ConfirmAction = Box<dyn FnOnce(&App)>;
struct Controller {
    ui: slint::Weak<AppWindow>,
    store: RefCell<Store>,
    workspaces: RefCell<Vec<Workspace>>,
    active: RefCell<Option<String>>,
    next_id: std::cell::Cell<u64>,
    confirmation: RefCell<Option<ConfirmAction>>,
    library_sql: RefCell<Vec<String>>,
    connecting: RefCell<std::collections::HashSet<String>>,
    auth_retry: RefCell<Option<((ConnectionConfig, Option<String>, Option<String>), secrets::Slot)>>,
    /// Connections found by the import window: (connection, selected, already in the store).
    import_batch: RefCell<Vec<(crate::import::Imported, bool, bool)>>,
    /// A password-protected TablePlus export waiting for its password.
    pending_export: RefCell<Option<PathBuf>>,
    /// The filter rules shown in the filter bar (edited in place by the UI).
    filter_rules: Rc<VecModel<FilterRule>>,
    /// Rules and "match all" per table tab, and the tab whose rules are shown.
    saved_rules: RefCell<std::collections::HashMap<u64, (Vec<FilterRule>, bool)>>,
    rules_tab: std::cell::Cell<Option<u64>>,
    /// Waits for a table to finish loading before a sidebar action (export, import…) runs.
    waiter: RefCell<Option<slint::Timer>>,
    /// All allowed values for the cell being edited; the dialog shows the ones matching the search.
    cell_choices: RefCell<Vec<String>>,
}
fn strings(values: impl IntoIterator<Item = impl Into<SharedString>>) -> ModelRc<SharedString> {
    Rc::new(VecModel::from(
        values.into_iter().map(Into::into).collect::<Vec<_>>(),
    ))
    .into()
}
fn model<T: Clone + 'static>(values: Vec<T>) -> ModelRc<T> {
    Rc::new(VecModel::from(values)).into()
}
/// Replacing a model resets list views (scroll position, hover), so skip identical updates.
fn unchanged<T: Clone + PartialEq + 'static>(current: &ModelRc<T>, new: &[T]) -> bool {
    current.row_count() == new.len() && current.iter().zip(new).all(|(a, b)| &a == b)
}
fn tint(c: Option<Color>) -> slint::Color {
    let (r, g, b) = c.unwrap_or(Color::Teal).rgb();
    slint::Color::from_rgb_u8((r * 255.) as u8, (g * 255.) as u8, (b * 255.) as u8)
}
fn connection_item(c: &ConnectionConfig, group: String) -> ConnectionItem {
    let name = c.display_name();
    let initials = name.split_whitespace().filter_map(|word| word.chars().find(|c| c.is_alphanumeric())).take(2).collect::<String>();
    let short_name = if initials.chars().count() > 1 { initials } else { name.chars().filter(|c| c.is_alphanumeric()).take(2).collect() }.to_uppercase();
    let (badge, badge_color) = engine_badge(c.kind);
    let subtitle = match c.kind {
        DbKind::Sqlite if c.ssh.enabled => format!("SSH : {}", c.file_path),
        DbKind::Sqlite => c.file_path.clone(),
        _ => [
            if c.ssh.enabled { "SSH".to_string() } else { c.host.clone() },
            c.database.clone(),
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" : "),
    };
    ConnectionItem {
        id: c.id.as_str().into(),
        name: c.display_name().into(),
        detail: c.summary().into(),
        kind: c.kind.label().into(),
        tag: c.tag.map_or(String::new(), |t| t.label().to_lowercase()).into(),
        tag_color: tint(c.tag.map(Tag::color)),
        tint: tint(c.effective_color()),
        group: group.into(),
        short_name: short_name.into(),
        badge: badge.into(),
        badge_color,
        subtitle: subtitle.into(),
        ..Default::default()
    }
}
fn engine_badge(kind: DbKind) -> (&'static str, slint::Color) {
    let (text, hex) = match kind {
        DbKind::Postgres => ("Pg", 0x3a86d9),
        DbKind::Mysql => ("My", 0xe8923a),
        DbKind::Sqlite => ("Sl", 0x8a3cf0),
        DbKind::Redis => ("Re", 0xc0392b),
    };
    (text, slint::Color::from_rgb_u8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8))
}
/// The PostgreSQL schema the sidebar shows, or None for all of them.
fn sidebar_schema(w: &Workspace) -> Option<String> {
    if w.config.kind != DbKind::Postgres || w.schema_filter == "*" {
        return None;
    }
    if !w.schema_filter.is_empty() {
        return Some(w.schema_filter.clone());
    }
    let configured = w.config.schema.trim();
    if !configured.is_empty() {
        return Some(configured.to_string());
    }
    w.objects.iter().any(|o| o.schema == "public").then(|| "public".to_string())
}
/// Which secret to ask for when a connection attempt failed because of missing or wrong credentials.
fn auth_failure(cfg: &ConnectionConfig, error: &anyhow::Error) -> Option<secrets::Slot> {
    let text = format!("{error:#}").to_lowercase();
    if cfg.ssh.enabled && (text.contains("ssh tunnel failed") || text.contains("no ssh password")) {
        let wants_secret = cfg.ssh.auth == SshAuth::Password
            || text.contains("passphrase")
            || text.contains("incorrect passphrase")
            || text.contains("load key");
        return (wants_secret && (text.contains("permission denied") || text.contains("no ssh password") || text.contains("passphrase")))
            .then_some(secrets::Slot::Ssh);
    }
    if cfg.kind == DbKind::Sqlite {
        return None;
    }
    [
        "password authentication failed",
        "password missing",
        "no password supplied",
        "password is required",
        "password required",
        "access denied for user",
        "noauth",
        "wrongpass",
        "invalid password",
        "authentication failed",
        "requires a password",
    ]
    .iter()
    .any(|needle| text.contains(needle))
    .then_some(secrets::Slot::Database)
}
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}
fn local_time(at: std::time::SystemTime) -> String {
    let secs = at.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let t = secs.as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:02}:{:02}:{:02}.{:03}", tm.tm_hour, tm.tm_min, tm.tm_sec, secs.subsec_millis())
}

/// Match the native title bar to the chosen appearance; `None` follows the system.
fn window_theme(ui: &AppWindow, mode: i32) {
    use slint::winit_030::{WinitWindowAccessor, winit::window::Theme};
    let theme = match mode {
        1 => Some(Theme::Light),
        2 => Some(Theme::Dark),
        _ => None,
    };
    ui.window().with_winit_window(|w| w.set_theme(theme));
}

pub fn run(files: Vec<PathBuf>) -> Result<()> {
    let ui = AppWindow::new()?;
    let app = Rc::new(Controller {
        ui: ui.as_weak(),
        store: RefCell::new(config::load_store()),
        workspaces: RefCell::new(Vec::new()),
        active: RefCell::new(None),
        next_id: std::cell::Cell::new(0),
        confirmation: RefCell::new(None),
        library_sql: RefCell::new(Vec::new()),
        connecting: RefCell::new(Default::default()),
        auth_retry: RefCell::new(None),
        import_batch: RefCell::new(Vec::new()),
        pending_export: RefCell::new(None),
        filter_rules: Rc::new(VecModel::default()),
        saved_rules: RefCell::new(Default::default()),
        rules_tab: std::cell::Cell::new(None),
        waiter: RefCell::new(None),
        cell_choices: RefCell::new(Vec::new()),
    });
    ui.invoke_set_appearance(config::appearance());
    let weak = ui.as_weak();
    let _ = slint::spawn_local(async move {
        use slint::winit_030::WinitWindowAccessor;
        let Some(ui) = weak.upgrade() else { return };
        if ui.window().winit_window().await.is_ok() {
            window_theme(&ui, config::appearance());
        }
    });
    let a = app.clone();
    ui.on_command(move |action| a.command(action.as_str()));
    let a = app.clone();
    ui.on_connection_action(move |id, action| a.connection_action(id.as_str(), action.as_str()));
    ui.on_highlight(|text| {
        model(
            editor::highlight(text.as_str())
                .into_iter()
                .map(|(line, col, text, kind)| SqlToken {
                    line: line as i32,
                    col: col as i32,
                    text: text.into(),
                    kind: kind as i32,
                })
                .collect(),
        )
    });
    ui.on_caret_at(|text, cursor| {
        let before = text.get(..cursor.max(0) as usize).unwrap_or(text.as_str());
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        Caret {
            line: before.matches('\n').count() as i32,
            col: before[line_start..].chars().count() as i32,
        }
    });
    let a = app.clone();
    ui.on_session_action(move |index, action| a.session_action(index, action.as_str()));
    let a = app.clone();
    ui.on_object_action(move |index, action| a.object_action(index, action.as_str()));
    let a = app.clone();
    ui.on_tab_action(move |index, action| a.tab_action(index, action.as_str()));
    let a = app.clone();
    ui.on_cell_action(move |r, c, edit| a.cell_action(r, c, edit));
    let a = app.clone();
    ui.on_cell_select(move |r, c, mode| a.cell_select(r, c, mode));
    let a = app.clone();
    ui.on_paste_text(move |text| a.paste_rows(text.as_str()));
    let a = app.clone();
    ui.on_field_edit(move |index, value, null| a.field_edit(index, value.as_str(), null));
    let a = app.clone();
    ui.on_form_action(move |action| a.form_action(action.as_str()));
    let a = app.clone();
    ui.on_search_changed(move |kind| {
        if kind == "connections" {
            a.refresh_home();
        } else if kind == "palette" {
            a.refresh_palette();
        } else if kind == "choices" {
            if let Some(ui) = a.ui.upgrade() {
                let search = ui.get_cell_choice_search().to_lowercase();
                ui.set_cell_choices(strings(
                    a.cell_choices.borrow().iter().filter(|c| c.to_lowercase().contains(&search)).cloned(),
                ));
            }
        } else {
            a.refresh_objects();
        }
    });
    let a = app.clone();
    ui.on_library_action(move |index, action| {
        if action == "delete" {
            a.delete_library(index);
        } else {
            a.load_library(index);
        }
    });
    let a = app.clone();
    ui.on_result_selected(move |index| a.select_result(index));
    let a = app.clone();
    ui.on_completion_selected(move |index| a.complete_selected(index));
    let weak = Rc::downgrade(&app);
    ui.window().on_close_requested(move || {
        let Some(a) = weak.upgrade() else { return slint::CloseRequestResponse::HideWindow; };
        let pending = a.workspaces.borrow().iter().any(|w| w.pending() > 0 || w.busy());
        if pending {
            a.confirm("Close Pueo?", "There are pending edits or database operations. Closing will discard uncommitted edits and disconnect.", "Close Pueo", |a| { if let Some(ui) = a.ui.upgrade() { let _ = ui.hide(); let _ = slint::quit_event_loop(); } });
            slint::CloseRequestResponse::KeepWindowShown
        } else { slint::CloseRequestResponse::HideWindow }
    });
    ui.set_color_names(strings(
        std::iter::once("No color").chain(Color::ALL.iter().map(|c| c.label())),
    ));
    ui.set_tag_names(strings(
        std::iter::once("No tag").chain(Tag::ALL.iter().map(|t| t.label())),
    ));
    ui.set_ssh_auth_names(strings(SshAuth::ALL.iter().map(|a| a.label())));
    ui.set_filter_ops(strings(filter::Op::ALL.iter().map(|op| op.label())));
    ui.set_filter_op_values(model(filter::Op::ALL.iter().map(|op| op.needs_value()).collect()));
    ui.set_filter_rules(app.filter_rules.clone().into());
    ui.set_version(env!("CARGO_PKG_VERSION").into());
    app.refresh_home();
    app.render();
    let a = app.clone();
    slint::Timer::single_shot(std::time::Duration::from_millis(1), move || {
        for path in files {
            let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
            cfg.file_path = path.to_string_lossy().into_owned();
            a.connect(cfg, None, None, false);
        }
    });
    let timer = slint::Timer::default();
    let a = app.clone();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(500),
        move || {
            a.refresh_console();
            a.refresh_activity();
        },
    );
    ui.run()?;
    Ok(())
}

impl Controller {
    fn id(&self) -> u64 {
        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        id
    }
    fn active_index(&self) -> Option<usize> {
        let id = self.active.borrow();
        self.workspaces
            .borrow()
            .iter()
            .position(|w| Some(&w.config.id) == id.as_ref())
    }
    fn context(&self) -> Option<(String, u64, db::SharedDriver)> {
        let ui = self.ui.upgrade()?;
        if ui.get_home() {
            return None;
        }
        let index = self.active_index()?;
        let ws = self.workspaces.borrow();
        let w = &ws[index];
        let tab = w.active()?;
        Some((w.config.id.clone(), tab.id, w.driver.clone()))
    }
    fn notice(&self, text: impl AsRef<str>, error: bool) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_notice(text.as_ref().into());
            ui.set_notice_error(error);
        }
    }
    fn error(&self, error: anyhow::Error) {
        self.notice(format!("{error:#}"), true);
    }
    fn confirm(
        self: &App,
        title: &str,
        text: &str,
        action: &str,
        done: impl FnOnce(&App) + 'static,
    ) {
        *self.confirmation.borrow_mut() = Some(Box::new(done));
        if let Some(ui) = self.ui.upgrade() {
            ui.set_confirm_title(title.into());
            ui.set_confirm_text(text.into());
            ui.set_confirm_action(action.into());
            ui.set_confirm_dialog(true);
        }
    }
    fn guard(self: &App, done: impl FnOnce(&App) + 'static) {
        let Some(index) = self.active_index() else {
            return;
        };
        let ws = self.workspaces.borrow();
        let w = &ws[index];
        if w.busy() {
            drop(ws);
            self.notice(
                "Wait for the operation to finish, or stop the query first.",
                true,
            );
            return;
        }
        if w.active().is_some_and(|t| t.pending() > 0) {
            drop(ws);
            self.confirm("Discard pending changes?", "This action replaces the current data. Commit your edits first, or discard them to continue.", "Discard and continue", move |a| {
                if let Some(index) = a.active_index() && let Some(t) = a.workspaces.borrow_mut()[index].active_mut() && let TabContent::Table(table) = &mut t.content { table.data.discard(); }
                done(a);
            });
        } else {
            drop(ws);
            done(self);
        }
    }
    fn refresh_home(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let search = ui.get_connection_search().to_lowercase();
        let store = self.store.borrow();
        ui.set_group_names(strings(
            std::iter::once("Ungrouped".to_string())
                .chain(store.groups.iter().map(|g| g.name.clone())),
        ));
        let mut connections = Vec::new();
        for group in std::iter::once(None).chain(store.groups.iter().map(Some)) {
            let matches = store
                .connections
                .iter()
                .filter(|c| {
                    let grouped = c
                        .group
                        .as_ref()
                        .and_then(|id| store.groups.iter().find(|g| &g.id == id));
                    grouped.map(|g| &g.id) == group.map(|g| &g.id)
                        && format!(
                            "{} {} {} {} {}",
                            c.display_name(),
                            c.summary(),
                            c.kind.label(),
                            c.tag.map_or("", Tag::short),
                            group.map_or("", |g| &g.name)
                        )
                        .to_lowercase()
                        .contains(&search)
                })
                .collect::<Vec<_>>();
            if let Some(g) = group {
                if !search.is_empty() && matches.is_empty() {
                    continue;
                }
                let (symbol, icon) = match &g.icon {
                    config::Icon::Emoji(s) => (s.clone(), slint::Image::default()),
                    config::Icon::Image(path) => (
                        String::new(),
                        slint::Image::load_from_path(std::path::Path::new(path))
                            .unwrap_or_default(),
                    ),
                    config::Icon::Symbol(_) => ("▣".into(), slint::Image::default()),
                };
                connections.push(ConnectionItem {
                    id: g.id.as_str().into(),
                    name: format!("{} ({})", g.name, matches.len()).into(),
                    tint: tint(g.color),
                    header: true,
                    collapsed: g.collapsed,
                    symbol: symbol.into(),
                    icon,
                    ..Default::default()
                });
                if g.collapsed && search.is_empty() {
                    continue;
                }
            }
            connections.extend(
                matches
                    .into_iter()
                    .map(|c| connection_item(c, group.map_or(String::new(), |g| g.name.clone()))),
            );
        }
        ui.set_connections(model(connections));
    }
    fn refresh_palette(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let search = ui.get_palette_search().to_lowercase();
        let store = self.store.borrow();
        let items = store
            .connections
            .iter()
            .filter(|c| {
                format!(
                    "{} {} {} {}",
                    c.display_name(),
                    c.summary(),
                    c.kind.label(),
                    c.tag.map_or("", Tag::label)
                )
                .to_lowercase()
                .contains(&search)
            })
            .map(|c| connection_item(c, String::new()))
            .collect();
        ui.set_palette_items(model(items));
        ui.set_palette_index(0);
    }
    fn refresh_objects(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let Some(index) = self.active_index() else {
            ui.set_objects(model(Vec::new()));
            return;
        };
        let ws = self.workspaces.borrow();
        let w = &ws[index];
        let search = ui.get_object_search().to_lowercase();
        let shown_schema = sidebar_schema(w);
        if w.config.kind == DbKind::Postgres {
            let mut schemas: Vec<String> = w.objects.iter().map(|o| o.schema.clone()).filter(|s| !s.is_empty()).collect();
            schemas.sort();
            schemas.dedup();
            let index = shown_schema.as_ref().and_then(|s| schemas.iter().position(|x| x == s)).map_or(0, |p| p + 1);
            let names: Vec<String> = std::iter::once("All schemas".to_string()).chain(schemas).collect();
            if !unchanged(&ui.get_schemas(), &names.iter().map(SharedString::from).collect::<Vec<_>>()) {
                ui.set_schemas(strings(names));
            }
            ui.set_schema_index(index as i32);
        } else if ui.get_schemas().row_count() > 0 {
            ui.set_schemas(strings(Vec::<String>::new()));
        }
        let mut objects = Vec::new();
        let mut groups = std::collections::BTreeMap::<String, Vec<(usize, &db::DbObject)>>::new();
        for (i, o) in w.objects.iter().enumerate().filter(|(_, o)| {
            shown_schema.as_ref().is_none_or(|s| &o.schema == s)
                && format!("{} {}", o.name, o.schema)
                    .to_lowercase()
                    .contains(&search)
        }) {
            let section = if o.kind.is_routine() {
                "functions".into()
            } else if o.kind == db::ObjectKind::View {
                "views".into()
            } else if o.kind == db::ObjectKind::Key {
                o.schema.clone()
            } else {
                "tables".into()
            };
            groups.entry(section).or_default().push((i, o));
        }
        for (section, mut entries) in groups {
            let open = !search.is_empty()
                || w.sections
                    .get(&section)
                    .copied()
                    .unwrap_or(section != "functions");
            let mut title = section.clone();
            if let Some(first) = title.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            objects.push(ObjectItem {
                label: title.into(),
                header: true,
                expanded: open,
                count: entries.len() as i32,
                section: section.as_str().into(),
                ..Default::default()
            });
            if !open {
                continue;
            }
            entries.sort_by_key(|(_, o)| {
                (
                    !w.pins.contains(&(o.schema.clone(), o.name.clone())),
                    o.name.to_lowercase(),
                    o.schema.clone(),
                )
            });
            for (i, o) in entries {
                let key = (o.schema.clone(), o.name.clone());
                objects.push(ObjectItem {
                    label: if o.signature.is_empty() {
                        o.name.clone()
                    } else {
                        format!("{}({})", o.name, o.signature)
                    }
                    .into(),
                    detail: o.schema.as_str().into(),
                    index: i as i32,
                    pinned: w.pins.contains(&key),
                    expanded: w.expanded.contains_key(&key),
                    kind: match o.kind {
                        db::ObjectKind::Table => "table",
                        db::ObjectKind::View => "view",
                        db::ObjectKind::Key => "key",
                        db::ObjectKind::Function | db::ObjectKind::Procedure => "function",
                    }
                    .into(),
                    ..Default::default()
                });
                if let Some(metadata) = w.expanded.get(&key) {
                    for row in &metadata.rows {
                        objects.push(ObjectItem {
                            label: row.first().and_then(|v| v.as_deref()).unwrap_or("").into(),
                            column_type: row.get(1).and_then(|v| v.as_deref()).unwrap_or("").into(),
                            primary: row.get(4).and_then(|v| v.as_deref()) == Some("PRI"),
                            index: i as i32,
                            kind: "column".into(),
                            child: true,
                            ..Default::default()
                        });
                    }
                }
            }
        }
        if !unchanged(&ui.get_objects(), &objects) {
            ui.set_objects(model(objects));
        }
        ui.set_truncated(w.truncated);
    }
    fn render(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let index = self.active_index();
        let ws = self.workspaces.borrow();
        let sessions: Vec<_> = ws.iter().map(|w| connection_item(&w.config, String::new())).collect();
        if !unchanged(&ui.get_sessions(), &sessions) {
            ui.set_sessions(model(sessions));
        }
        ui.set_active_session(index.map_or(-1, |i| i as i32));
        ui.set_connected(index.is_some());
        ui.set_busy(!self.connecting.borrow().is_empty() || index.is_some_and(|i| ws[i].busy()));
        ui.set_editable(false);
        ui.set_query(false);
        ui.set_table(false);
        ui.set_is_key(false);
        ui.set_active_tab(-1);
        ui.set_pending(0);
        ui.set_columns(strings(Vec::<String>::new()));
        ui.set_rows(model(Vec::new()));
        ui.set_fields(model(Vec::new()));
        ui.set_tabs(model(Vec::new()));
        ui.set_results(strings(Vec::<String>::new()));
        ui.set_can_prev(false);
        ui.set_can_next(false);
        ui.set_selected_row(-1);
        ui.set_selected_column(-1);
        ui.set_row_summary("".into());
        ui.set_console_seq(-1);
        ui.set_edit_row(-1);
        ui.set_selected_count(0);
        if let Some(index) = index {
            let w = &ws[index];
            ui.set_session_name(w.config.display_name().into());
            ui.set_server_info(w.info.as_str().into());
            let mut pill = w
                .config
                .tag
                .map_or(String::new(), |t| format!("{} | ", t.short()));
            pill.push_str(&w.info.replace(" · ", " : "));
            if let Some(TabContent::Table(t)) = w.active().map(|t| &t.content) {
                if t.object.schema.is_empty() || t.object.kind == db::ObjectKind::Key {
                    pill.push_str(&format!(" : {}", t.object.name));
                } else {
                    pill.push_str(&format!(" : {}.{}", t.object.schema, t.object.name));
                }
            }
            ui.set_pill(pill.into());
            ui.set_pill_color(tint(w.config.effective_color()));
            ui.set_database(w.config.database.as_str().into());
            ui.set_databases(strings(w.databases.clone()));
            ui.set_database_index(
                w.databases
                    .iter()
                    .position(|d| d == &w.config.database)
                    .unwrap_or(0) as i32,
            );
            ui.set_tabs(model(
                w.tabs
                    .iter()
                    .map(|t| TabItem {
                        title: t.title.as_str().into(),
                        dirty: t.pending() > 0,
                        busy: t.busy,
                        kind: match &t.content {
                            TabContent::Query(_) => 0,
                            TabContent::Table(t) => match t.object.kind {
                                db::ObjectKind::Table => 1,
                                db::ObjectKind::View => 2,
                                db::ObjectKind::Key => 3,
                                _ => 4,
                            },
                        },
                    })
                    .collect(),
            ));
            if let Some(tab) = w.active() {
                ui.set_active_tab(w.tabs.iter().position(|t| t.id == tab.id).unwrap() as i32);
                ui.set_status(tab.status.as_str().into());
                ui.set_pending(tab.pending() as i32);
                match &tab.content {
                    TabContent::Query(q) => {
                        ui.set_query(true);
                        ui.set_result_index(q.result as i32);
                        ui.set_line_numbers(line_numbers(&q.sql).into());
                        if ui.get_sql().as_str() != q.sql {
                            ui.set_sql(q.sql.as_str().into());
                        }
                        ui.set_results(strings(q.results.iter().enumerate().map(|(i, r)| {
                            format!(
                                "{} · {} rows · {:.1} ms",
                                i + 1,
                                r.rows.len(),
                                r.elapsed.as_secs_f64() * 1000.
                            )
                        })));
                    }
                    TabContent::Table(t) => {
                        ui.set_table(true);
                        if self.rules_tab.get() != Some(tab.id) {
                            self.rules_tab.set(Some(tab.id));
                            let (rules, all) = self.saved_rules.borrow().get(&tab.id).cloned().unwrap_or_default();
                            let rules = if rules.is_empty() {
                                vec![FilterRule::default()]
                            } else {
                                rules
                            };
                            self.filter_rules.set_vec(rules);
                            ui.set_filter_match_all(all || !self.saved_rules.borrow().contains_key(&tab.id));
                        }
                        ui.set_is_key(t.object.kind == db::ObjectKind::Key);
                        ui.set_view(t.view);
                        ui.set_filter(t.filter_text.as_str().into());
                        ui.set_page_label(
                            format!(
                                "{} / {}",
                                t.options.offset / PAGE_SIZE + 1,
                                t.total.div_ceil(PAGE_SIZE as u64).max(1)
                            )
                            .into(),
                        );
                        ui.set_row_summary(if t.view == 0 && !t.data.rows.is_empty() {
                            format!(
                                "{}-{} of {} rows",
                                thousands(t.options.offset as u64 + 1),
                                thousands((t.options.offset + t.data.rows.len()) as u64),
                                thousands(t.total)
                            )
                        } else {
                            format!("{} rows", thousands(tab.grid().rows.len() as u64))
                        }
                        .into());
                        ui.set_can_prev(t.view == 0 && t.options.offset > 0);
                        ui.set_can_next(
                            t.view == 0 && (t.options.offset + PAGE_SIZE) < t.total as usize,
                        );
                    }
                }
                let grid = tab.grid();
                ui.set_editable(grid.editable());
                if !ui.get_columns().iter().eq(grid.columns.iter().map(SharedString::from))
                    || ui.get_column_widths().row_count() != grid.columns.len()
                {
                    ui.set_column_widths(model(
                        grid.columns
                            .iter()
                            .enumerate()
                            .map(|(i, name)| {
                                let longest = grid
                                    .rows
                                    .iter()
                                    .take(200)
                                    .filter_map(|r| r.current.get(i))
                                    .map(|v| v.as_deref().map_or(4, |v| v.chars().count()))
                                    .chain([name.chars().count() + 2])
                                    .max()
                                    .unwrap_or(4);
                                (longest as f32 * 7.6 + 26.0).clamp(56.0, 340.0)
                            })
                            .collect(),
                    ));
                }
                ui.set_columns(strings(grid.columns.clone()));
                ui.set_rows(model(
                    grid.rows
                        .iter()
                        .enumerate()
                        .map(|(index, r)| RowItem {
                            marked: grid.marked.contains(&index),
                            state: if r.deleted {
                                2
                            } else if r.original.is_none() {
                                1
                            } else {
                                0
                            },
                            cells: model(
                                r.current
                                    .iter()
                                    .enumerate()
                                    .map(|(i, c)| CellItem {
                                        text: c
                                            .as_deref()
                                            .unwrap_or("NULL")
                                            .replace(['\n', '\r'], " ↵ ")
                                            .into(),
                                        null: c.is_none(),
                                        multiline: c.as_deref().is_some_and(|v| v.contains(['\n', '\r'])),
                                        numeric: c.as_deref().is_some_and(|v| {
                                            !v.is_empty() && v.parse::<f64>().is_ok()
                                        }),
                                        changed: r
                                            .original
                                            .as_ref()
                                            .is_some_and(|original| original.get(i) != Some(c)),
                                    })
                                    .collect(),
                            ),
                        })
                        .collect(),
                ));
                ui.set_selected_count(grid.selected_rows().len() as i32);
                if let Some((row, col)) = grid.selected {
                    ui.set_selected_row(row as i32);
                    ui.set_selected_column(col as i32);
                    if let Some(r) = grid.rows.get(row) {
                        ui.set_fields(model(
                            grid.columns
                                .iter()
                                .zip(&r.current)
                                .map(|(c, v)| FieldItem {
                                    name: c.as_str().into(),
                                    value: v.as_deref().unwrap_or("").into(),
                                    null: v.is_none(),
                                })
                                .collect(),
                        ));
                    }
                }
            } else {
                ui.set_status("Choose an object or open a query tab".into());
            }
        } else {
            ui.set_home(true);
            ui.set_status("Ready".into());
        }
        drop(ws);
        self.refresh_objects();
        self.refresh_console();
    }
    fn refresh_activity(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let index = self.active_index();
        let ws = self.workspaces.borrow();
        ui.set_busy(
            !self.connecting.borrow().is_empty()
                || index.is_some_and(|i| ws[i].busy() || ws[i].log.activity().0 > 0),
        );
    }
    fn refresh_console(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        if !ui.get_console_open() {
            return;
        }
        let Some(index) = self.active_index() else {
            return;
        };
        let ws = self.workspaces.borrow();
        let filter = ui.get_console_filter();
        let log = ws[index].log.since(0);
        let last = log.last().map_or(0, |e| e.seq as i32);
        if last == ui.get_console_seq() && filter == ui.get_console_shown_filter() {
            return;
        }
        let entries = log
            .iter()
            .filter(|e| {
                filter == 0
                    || (filter == 1 && e.kind != db::log::LogKind::Meta)
                    || (filter == 2 && e.kind == db::log::LogKind::User)
            })
            .map(|e| {
                let ms = e.duration.as_secs_f64() * 1000.;
                ConsoleEntry {
                    time: local_time(e.at).into(),
                    duration: if ms < 1. { "<1 ms".into() } else { format!("{ms:.0} ms").into() },
                    sql: e.sql.split_whitespace().collect::<Vec<_>>().join(" ").into(),
                    error: e.error.clone().unwrap_or_default().into(),
                    user: e.kind == db::log::LogKind::User,
                }
            })
            .collect::<Vec<_>>();
        ui.set_console_seq(last);
        ui.set_console_shown_filter(filter);
        ui.set_console_entries(model(entries));
    }
    fn set_busy(&self, session: &str, tab: Option<u64>, busy: bool) {
        if let Some(w) = self
            .workspaces
            .borrow_mut()
            .iter_mut()
            .find(|w| w.config.id == session)
        {
            if let Some(id) = tab {
                if let Some(t) = w.tabs.iter_mut().find(|t| t.id == id) {
                    t.busy = busy;
                }
            } else {
                w.busy = busy;
            }
        }
        self.render();
    }
    fn connect(
        self: &App,
        cfg: ConnectionConfig,
        password: Option<String>,
        ssh_secret: Option<String>,
        lookup: bool,
    ) {
        if self
            .workspaces
            .borrow()
            .iter()
            .any(|w| w.config.id == cfg.id)
        {
            *self.active.borrow_mut() = Some(cfg.id);
            if let Some(ui) = self.ui.upgrade() {
                ui.set_home(false);
            }
            self.render();
            return;
        }
        if !self.connecting.borrow_mut().insert(cfg.id.clone()) {
            return;
        }
        self.render();
        let a = self.clone();
        let id = cfg.id.clone();
        let retry = (cfg.clone(), password.clone(), ssh_secret.clone());
        runtime::spawn(
            async move {
                let password = match password {
                    None if lookup && cfg.kind != DbKind::Sqlite => {
                        secrets::get_password(&cfg.id, secrets::Slot::Database).await?
                    }
                    p => p,
                };
                let ssh_secret = match ssh_secret {
                    None if lookup && cfg.ssh.enabled => {
                        secrets::get_password(&cfg.id, secrets::Slot::Ssh).await?
                    }
                    s => s,
                };
                let log = db::log::QueryLog::new();
                let driver = db::logged(
                    db::connect(&cfg, password.clone(), ssh_secret.clone()).await?,
                    log.clone(),
                );
                let info = driver.server_info().await?;
                let objects = driver.list_objects(None).await?;
                let mut databases = driver.list_databases().await?;
                if databases.is_empty() {
                    databases.push(if cfg.kind == DbKind::Sqlite {
                        "main".into()
                    } else {
                        cfg.database.clone()
                    });
                }
                let columns = driver.list_columns().await.unwrap_or_default();
                let pins = config::pinned_objects(&cfg.id, &cfg.database);
                let sections = config::sidebar_sections(&cfg.id);
                let schema = SchemaCache {
                    tables: objects
                        .objects
                        .iter()
                        .filter(|o| !o.kind.is_routine())
                        .map(|o| o.name.clone())
                        .collect(),
                    columns,
                };
                Ok::<_, anyhow::Error>(Workspace {
                    config: cfg,
                    driver,
                    log,
                    info,
                    databases,
                    objects: objects.objects,
                    sections,
                    expanded: Default::default(),
                    truncated: objects.truncated,
                    pins,
                    schema,
                    tabs: Vec::new(),
                    active_tab: None,
                    busy: false,
                    schema_filter: String::new(),
                    password,
                    ssh_secret,
                })
            },
            move |result| {
                a.connecting.borrow_mut().remove(&id);
                match result {
                    Ok(w) => {
                        *a.active.borrow_mut() = Some(w.config.id.clone());
                        a.workspaces.borrow_mut().push(w);
                        if let Some(ui) = a.ui.upgrade() {
                            ui.set_home(false);
                            ui.set_notice("".into());
                        }
                    }
                    Err(e) => match auth_failure(&retry.0, &e) {
                        Some(slot) => a.ask_password(retry, slot, &e),
                        None => a.error(e),
                    },
                }
                a.render();
            },
        );
    }
    fn ask_password(&self, retry: (ConnectionConfig, Option<String>, Option<String>), slot: secrets::Slot, error: &anyhow::Error) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let cfg = &retry.0;
        let ssh = matches!(slot, secrets::Slot::Ssh);
        let tried = if ssh { retry.2.is_some() } else { retry.1.is_some() };
        let who = if ssh {
            format!("{}@{}", cfg.ssh.user, cfg.ssh.host)
        } else if cfg.database.is_empty() {
            format!("{}@{}", cfg.user, cfg.host)
        } else {
            format!("{}@{}/{}", cfg.user, cfg.host, cfg.database)
        };
        ui.set_auth_title(
            if ssh {
                if cfg.ssh.auth == SshAuth::Key { "SSH Key Passphrase" } else { "SSH Password" }
            } else {
                "Database Password"
            }
            .into(),
        );
        ui.set_auth_text(
            format!(
                "{} {} for {who}.",
                if tried { "That password didn't work. Enter the password" } else { "Enter the password" },
                if ssh && cfg.ssh.auth == SshAuth::Key { "for the SSH key" } else { "" },
            )
            .replace("  ", " ")
            .into(),
        );
        ui.set_auth_detail(format!("{error:#}").lines().last().unwrap_or_default().trim().into());
        ui.set_auth_name(cfg.display_name().into());
        ui.set_auth_password("".into());
        ui.set_auth_dialog(true);
        *self.auth_retry.borrow_mut() = Some((retry, slot));
    }
    fn open_import(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        ui.set_import_sources(model(
            crate::import::App::ALL
                .iter()
                .map(|app| {
                    let (badge, hex) = app.badge();
                    let path = app.path();
                    ImportSource {
                        name: app.name().into(),
                        badge: badge.into(),
                        color: slint::Color::from_rgb_u8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8),
                        detail: if path.is_some() { "Found on this Mac" } else { "Not installed" }.into(),
                        available: path.is_some(),
                    }
                })
                .collect(),
        ));
        ui.set_import_step(0);
        ui.set_import_dialog(true);
    }
    fn show_import(&self, source: &str, batch: Result<crate::import::Batch>) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let batch = match batch {
            Ok(b) => b,
            Err(e) => {
                self.error(e.context(format!("Could not read connections from {source}")));
                return;
            }
        };
        let store = self.store.borrow();
        *self.import_batch.borrow_mut() = batch
            .connections
            .into_iter()
            .map(|i| {
                let exists = crate::import::exists(&store, &i.config);
                (i, !exists, exists)
            })
            .collect();
        drop(store);
        ui.set_import_title(format!("Import from {source}").into());
        let mut note = Vec::new();
        if !batch.skipped.is_empty() {
            note.push(format!("Not supported yet: {}.", batch.skipped.join(", ")));
        }
        let batch_ref = self.import_batch.borrow();
        let with_password = batch_ref.iter().filter(|i| i.0.password.is_some() || i.0.ssh_secret.is_some()).count();
        if with_password > 0 {
            note.push(format!("Passwords for {with_password} connection{} will be saved in your keychain.", if with_password == 1 { "" } else { "s" }));
        } else if batch_ref.iter().any(|i| i.0.config.kind != DbKind::Sqlite) {
            note.push("Passwords are not copied from other apps. Pueo asks for each one the first time you connect.".into());
        }
        drop(batch_ref);
        ui.set_import_note(note.join(" ").into());
        self.refresh_import();
        ui.set_import_step(1);
        ui.set_import_dialog(true);
    }
    fn refresh_import(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let batch = self.import_batch.borrow();
        ui.set_import_items(model(
            batch
                .iter()
                .map(|(i, selected, exists)| {
                    let mut item = connection_item(&i.config, i.group.clone().unwrap_or_default());
                    if *exists {
                        item.subtitle = format!("{} · already added", item.subtitle).into();
                    }
                    ImportItem { connection: item, selected: *selected, exists: *exists }
                })
                .collect(),
        ));
        ui.set_import_count(batch.iter().filter(|i| i.1).count() as i32);
    }
    fn run_import(self: &App) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let chosen: Vec<_> = self
            .import_batch
            .borrow_mut()
            .drain(..)
            .filter(|(_, selected, _)| *selected)
            .map(|(i, _, _)| i)
            .collect();
        let passwords: Vec<(ConnectionConfig, secrets::Slot, String)> = chosen
            .iter()
            .flat_map(|i| {
                [
                    i.password.clone().map(|p| (i.config.clone(), secrets::Slot::Database, p)),
                    i.ssh_secret.clone().map(|p| (i.config.clone(), secrets::Slot::Ssh, p)),
                ]
            })
            .flatten()
            .collect();
        let mut store = self.store.borrow().clone();
        let added = crate::import::merge(&mut store, chosen);
        ui.set_import_dialog(false);
        if added.is_empty() {
            self.notice("No new connections to import.", false);
            return;
        }
        if !self.save_store(store) {
            return;
        }
        for (cfg, slot, password) in passwords.into_iter().filter(|(c, _, _)| added.contains(&c.id)) {
            let a = self.clone();
            runtime::spawn(
                async move { secrets::set_password(&cfg.id, slot, &cfg.display_name(), &password).await },
                move |r: Result<()>| {
                    if let Err(e) = r {
                        a.error(e.context("Could not save an imported password in the keychain"));
                    }
                },
            );
        }
        self.notice(
            format!("Imported {} connection{}.", added.len(), if added.len() == 1 { "" } else { "s" }),
            false,
        );
    }
    fn open_export(&self, path: PathBuf, password: String) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        match crate::import::load_tableplus_export(&path, &password) {
            Err(e) if e.downcast_ref::<crate::import::NeedsPassword>().is_some() => {
                ui.set_auth_title("TablePlus Export Password".into());
                ui.set_auth_name(path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default().into());
                ui.set_auth_text(
                    if password.is_empty() {
                        "Enter the password you chose when exporting these connections from TablePlus."
                    } else {
                        "That password didn't open the file. Enter the password used for the export."
                    }
                    .into(),
                );
                ui.set_auth_detail("".into());
                ui.set_auth_password("".into());
                ui.set_auth_keychain(false);
                ui.set_auth_dialog(true);
                *self.pending_export.borrow_mut() = Some(path);
            }
            result => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.show_import(&name, result);
            }
        }
    }
    fn submit_password(self: &App) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        if let Some(path) = self.pending_export.borrow_mut().take() {
            ui.set_auth_dialog(false);
            ui.set_auth_keychain(true);
            self.open_export(path, ui.get_auth_password().to_string());
            return;
        }
        let Some(((cfg, password, ssh_secret), slot)) = self.auth_retry.borrow_mut().take() else {
            return;
        };
        ui.set_auth_dialog(false);
        let secret = ui.get_auth_password().to_string();
        if ui.get_auth_remember() {
            let (id, name, secret) = (cfg.id.clone(), cfg.display_name(), secret.clone());
            let a = self.clone();
            runtime::spawn(
                async move { secrets::set_password(&id, slot, &name, &secret).await },
                move |r: Result<()>| {
                    if let Err(e) = r {
                        a.error(e.context("Could not save the password in the keychain"));
                    }
                },
            );
        }
        match slot {
            secrets::Slot::Ssh => self.connect(cfg, password, Some(secret), true),
            secrets::Slot::Database => self.connect(cfg, Some(secret), ssh_secret, true),
        }
    }
    fn new_query(self: &App, sql: String, title: &str) {
        let Some(index) = self.active_index() else {
            return;
        };
        let id = self.id();
        let mut ws = self.workspaces.borrow_mut();
        let w = &mut ws[index];
        w.tabs.push(Tab {
            id,
            title: title.into(),
            content: TabContent::Query(QueryState {
                sql,
                results: Vec::new(),
                result: 0,
                grid: Grid::default(),
            }),
            busy: false,
            status: "Write a query. Ctrl/⌘+Enter runs the statement or selection.".into(),
        });
        w.active_tab = Some(id);
        drop(ws);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_home(false);
            ui.set_completion_open(false);
        }
        self.render();
        if let Some(ui) = self.ui.upgrade() { ui.set_editor_focus_request(ui.get_editor_focus_request()+1); }
    }
    fn session_action(self: &App, index: i32, action: &str) {
        let Some(id) = usize::try_from(index)
            .ok()
            .and_then(|i| self.workspaces.borrow().get(i).map(|w| w.config.id.clone()))
        else {
            return;
        };
        *self.active.borrow_mut() = Some(id);
        match action {
            "new-query" => {
                self.new_query(String::new(), "Query");
                return;
            }
            "close" => {
                self.command("close-connection");
                return;
            }
            _ => {}
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_home(false);
            ui.set_object_search("".into());
            ui.set_completion_open(false);
            ui.set_library_open(false);
        }
        self.render();
    }
    fn tab_action(self: &App, index: i32, action: &str) {
        let Some(i) = self.active_index() else {
            return;
        };
        if action == "close" {
            let target = usize::try_from(index).ok().and_then(|index| { let ws = self.workspaces.borrow(); ws[i].tabs.get(index).map(|t| (ws[i].config.id.clone(),t.id)) });
            if let Some((session,id)) = target { self.close_tab(&session,id); }
            return;
        }
        let mut ws = self.workspaces.borrow_mut();
        if let Ok(index) = usize::try_from(index)
            && let Some(tab) = ws[i].tabs.get(index)
        {
            ws[i].active_tab = Some(tab.id);
        }
        drop(ws);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_completion_open(false);
            ui.set_library_open(false);
        }
        self.render();
    }
    fn close_tab(self: &App,session: &str,id: u64) {
        let ws = self.workspaces.borrow();
        let Some(tab) = ws.iter().find(|w| w.config.id == session).and_then(|w| w.tabs.iter().find(|t| t.id == id)) else { return; };
        if tab.busy { self.notice("Stop the running query or wait before closing this tab.",true); return; }
        let pending = tab.pending(); let name = tab.title.clone(); let session = session.to_string(); drop(ws);
        let close = move |a: &App| {
            let mut ws = a.workspaces.borrow_mut();
            if let Some(w) = ws.iter_mut().find(|w| w.config.id == session) && let Some(index) = w.tabs.iter().position(|t| t.id == id) {
                w.tabs.remove(index);
                if w.active_tab == Some(id) { w.active_tab = w.tabs.get(index.min(w.tabs.len().saturating_sub(1))).map(|t| t.id); }
            }
            drop(ws); a.render();
        };
        if pending > 0 { self.confirm(&format!("Close {name}?"),"This tab has pending edits. Commit them first, or discard them to close the tab.","Discard and close",close); }
        else { close(self); }
    }
    fn object_action(self: &App, index: i32, action: &str) {
        let Some(i) = self.active_index() else {
            return;
        };
        if let Some(section) = action.strip_prefix("section:") {
            let mut ws = self.workspaces.borrow_mut();
            let w = &mut ws[i];
            let value = !w
                .sections
                .get(section)
                .copied()
                .unwrap_or(section != "functions");
            w.sections.insert(section.into(), value);
            if let Err(e) = config::save_sidebar_sections(&w.config.id, &w.sections) {
                self.error(e);
            }
            drop(ws);
            self.refresh_objects();
            return;
        }
        let Some(object) = usize::try_from(index)
            .ok()
            .and_then(|j| self.workspaces.borrow()[i].objects.get(j).cloned())
        else {
            return;
        };
        if action == "pin" {
            let mut ws = self.workspaces.borrow_mut();
            let w = &mut ws[i];
            let key = (object.schema.clone(), object.name.clone());
            if !w.pins.remove(&key) {
                w.pins.insert(key);
            }
            if let Err(e) = config::save_pinned_objects(&w.config.id, &w.config.database, &w.pins) {
                self.error(e);
            }
            drop(ws);
            self.refresh_objects();
            return;
        }
        if self.workspaces.borrow()[i].busy() {
            self.notice("Wait for the current database operation.", true);
            return;
        }
        let kind = self.workspaces.borrow()[i].config.kind;
        match action {
            "copy-name" => {
                let name = if kind == DbKind::Postgres && !object.schema.is_empty() {
                    format!("{}.{}", object.schema, object.name)
                } else {
                    object.name.clone()
                };
                if let Some(ui) = self.ui.upgrade() {
                    ui.set_clipboard_content(name.into());
                    ui.invoke_copy_grid();
                }
                return;
            }
            "clone" if kind != DbKind::Redis && !object.kind.is_routine() => {
                if let Some(ui) = self.ui.upgrade() {
                    ui.set_clone_index(index);
                    ui.set_clone_source(object.name.as_str().into());
                    ui.set_clone_name(format!("{}_copy", object.name).into());
                    ui.set_clone_data(true);
                    ui.set_clone_dialog(true);
                }
                return;
            }
            "truncate" | "drop" if kind != DbKind::Redis && !object.kind.is_routine() => {
                if action == "truncate" && object.kind != db::ObjectKind::Table {
                    self.notice("Only tables can be truncated.", true);
                    return;
                }
                let (title, what, verb) = if action == "truncate" {
                    ("Truncate table?", "every row in", "Truncate")
                } else {
                    ("Delete table?", "", "Delete")
                };
                let sql = if action == "truncate" {
                    db::truncate_sql(kind, &object, false, false).0.join(";\n")
                } else {
                    db::drop_sql(kind, &object, false)
                };
                let text = if action == "truncate" {
                    format!("This permanently removes {what} {}. It can't be undone.", object.name)
                } else {
                    format!("This permanently deletes {} and its data. It can't be undone.", object.name)
                };
                let name = object.name.clone();
                self.confirm(title, &text, verb, move |a| {
                    a.run_sql(sql, format!("{} {name}.", if verb == "Truncate" { "Truncated" } else { "Deleted" }));
                });
                return;
            }
            "structure" | "import" | "open-new" => {}
            a if a.starts_with("export:") => {}
            _ => {}
        }
        if let Some(format) = action.strip_prefix("export:") {
            let format: i32 = format.parse().unwrap_or(0);
            self.object_action(index, "open");
            self.after_load(move |a| {
                if let Some(ui) = a.ui.upgrade() {
                    ui.set_export_format(format);
                }
                a.export();
            });
            return;
        }
        if action == "import" || action == "structure" {
            let then = action.to_string();
            self.object_action(index, "open");
            self.after_load(move |a| {
                if then == "import" { a.import() } else { a.command("view:1") }
            });
            return;
        }
        if matches!(action, "clone" | "truncate" | "drop") {
            let kind = self.workspaces.borrow()[i].driver.kind();
            let sql = if kind == DbKind::Redis && action == "drop" {
                format!("DEL {:?}", object.name)
            } else if kind == DbKind::Redis || object.kind.is_routine() {
                self.notice("Use a query tab for this object type.", false);
                return;
            } else {
                match action {
                    "clone" => db::clone_sql(kind, &object, &format!("{}_copy", object.name), true)
                        .join(";\n"),
                    "truncate" => {
                        if object.kind != db::ObjectKind::Table {
                            self.notice("Only tables can be truncated.", true);
                            return;
                        }
                        db::truncate_sql(kind, &object, false, false).0.join(";\n")
                    }
                    _ => db::drop_sql(kind, &object, false),
                }
            };
            self.new_query(
                format!(
                    "-- Review this statement before running it.\n{sql}{}",
                    if kind.is_sql() { ";" } else { "" }
                ),
                "Object action",
            );
            return;
        }
        if action == "columns" {
            if object.kind.is_routine() || object.kind == db::ObjectKind::Key {
                return;
            }
            let key = (object.schema.clone(), object.name.clone());
            if self.workspaces.borrow_mut()[i]
                .expanded
                .remove(&key)
                .is_some()
            {
                self.refresh_objects();
                return;
            }
            let (session, driver) = {
                let ws = self.workspaces.borrow();
                (ws[i].config.id.clone(), ws[i].driver.clone())
            };
            self.set_busy(&session, None, true);
            let a = self.clone();
            runtime::spawn(
                async move { driver.structure(&object).await },
                move |result| {
                    if let Some(w) = a
                        .workspaces
                        .borrow_mut()
                        .iter_mut()
                        .find(|w| w.config.id == session)
                    {
                        w.busy = false;
                        match result {
                            Ok(metadata) => {
                                w.expanded.insert(key, metadata);
                            }
                            Err(e) => a.error(e),
                        }
                    }
                    a.render();
                },
            );
            return;
        }
        if object.kind.is_routine() {
            let driver = self.workspaces.borrow()[i].driver.clone();
            let session = self.workspaces.borrow()[i].config.id.clone();
            let a = self.clone();
            self.set_busy(&session, None, true);
            runtime::spawn(
                async move { driver.definition(&object).await },
                move |result| {
                    a.set_busy(&session, None, false);
                    match result {
                        Ok(sql) => {
                            *a.active.borrow_mut() = Some(session);
                            a.new_query(sql, "Definition");
                        }
                        Err(e) => a.error(e),
                    }
                },
            );
            return;
        }
        let existing = self.workspaces.borrow()[i]
            .tabs
            .iter()
            .find(|t| action != "open-new" && matches!(&t.content, TabContent::Table(table) if table.object == object))
            .map(|t| t.id);
        if let Some(id) = existing {
            self.workspaces.borrow_mut()[i].active_tab = Some(id);
            self.render();
            return;
        }
        let id = self.id();
        let mut ws = self.workspaces.borrow_mut();
        let w = &mut ws[i];
        w.tabs.push(Tab {
            id,
            title: object.name.clone(),
            content: TabContent::Table(TableState {
                object,
                options: FetchOptions {
                    limit: PAGE_SIZE,
                    ..Default::default()
                },
                total: 0,
                view: 0,
                data: Grid::default(),
                metadata: Grid::default(),
                filter_text: String::new(),
            }),
            busy: false,
            status: "Loading…".into(),
        });
        w.active_tab = Some(id);
        drop(ws);
        self.load_table();
    }
    /// Run `then` once the active tab has finished loading (gives up after 30 s).
    fn after_load(self: &App, then: impl FnOnce(&App) + 'static) {
        let timer = slint::Timer::default();
        let weak = Rc::downgrade(self);
        let then = RefCell::new(Some(then));
        let started = Instant::now();
        timer.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(80), move || {
            let Some(a) = weak.upgrade() else { return };
            let idle = a.active_index().is_some_and(|i| {
                let ws = a.workspaces.borrow();
                !ws[i].busy() && ws[i].active().is_some_and(|t| !t.busy)
            });
            if idle || started.elapsed() > std::time::Duration::from_secs(30) {
                if let Some(f) = then.borrow_mut().take()
                    && idle
                {
                    f(&a);
                }
                a.waiter.borrow_mut().take();
            }
        });
        *self.waiter.borrow_mut() = Some(timer);
    }
    /// Run statements on the active connection, then refresh the sidebar and the open tab.
    fn run_sql(self: &App, sql: String, done: String) {
        let Some(i) = self.active_index() else {
            return;
        };
        let (session, driver) = {
            let ws = self.workspaces.borrow();
            (ws[i].config.id.clone(), ws[i].driver.clone())
        };
        self.set_busy(&session, None, true);
        let a = self.clone();
        runtime::spawn(async move { driver.execute(&sql).await }, move |result| {
            a.set_busy(&session, None, false);
            match result {
                Ok(_) => {
                    a.notice(done, false);
                    a.reload_objects(None);
                    if let Some(tab) = a.workspaces.borrow().iter().find(|w| w.config.id == session).and_then(|w| w.active_tab) {
                        let s = session.clone();
                        slint::Timer::single_shot(std::time::Duration::from_millis(1), {
                            let a = a.clone();
                            move || a.reload_tab(&s, tab)
                        });
                    }
                }
                Err(e) => a.error(e),
            }
            a.render();
        });
    }
    fn load_table(self: &App) {
        let Some((session, id, driver)) = self.context() else {
            return;
        };
        self.load_table_at(session, id, driver);
    }
    fn reload_tab(self: &App, session: &str, id: u64) {
        let driver = self
            .workspaces
            .borrow()
            .iter()
            .find(|w| w.config.id == session)
            .map(|w| w.driver.clone());
        if let Some(driver) = driver {
            self.load_table_at(session.to_owned(), id, driver);
        }
    }
    fn load_table_at(self: &App, session: String, id: u64, driver: db::SharedDriver) {
        let Some((object, options, view)) = self
            .workspaces
            .borrow()
            .iter()
            .find(|w| w.config.id == session)
            .and_then(|w| w.tabs.iter().find(|t| t.id == id))
            .and_then(|t| match &t.content {
                TabContent::Table(t) => Some((t.object.clone(), t.options.clone(), t.view)),
                _ => None,
            })
        else {
            return;
        };
        self.set_busy(&session, Some(id), true);
        let a = self.clone();
        runtime::spawn(
            async move {
                if view == 0 {
                    let keys = driver.primary_key(&object).await?;
                    let mut options = options;
                    if options.order_by.is_none() && object.kind != db::ObjectKind::Key {
                        options.order_by = keys
                            .as_ref()
                            .and_then(|k| k.first())
                            .map(|k| (k.clone(), true));
                    }
                    let rows = driver.fetch_rows(&object, &options).await?;
                    let total = driver
                        .count_rows(&object, options.filter.as_deref())
                        .await?;
                    let choices = driver.column_choices(&object).await?;
                    let mut grid = Grid::from_result(&rows);
                    grid.keys = keys;
                    grid.choices = choices;
                    Ok::<_, anyhow::Error>((grid, total, rows.elapsed))
                } else {
                    let result = if view == 1 {
                        driver.structure(&object).await?
                    } else {
                        driver.indexes(&object).await?
                    };
                    Ok((
                        Grid::from_result(&result),
                        result.rows.len() as u64,
                        result.elapsed,
                    ))
                }
            },
            move |result| {
                let mut ws = a.workspaces.borrow_mut();
                if let Some(tab) = ws
                    .iter_mut()
                    .find(|w| w.config.id == session)
                    .and_then(|w| w.tabs.iter_mut().find(|t| t.id == id))
                {
                    tab.busy = false;
                    match result {
                        Ok((grid, total, elapsed)) => {
                            tab.status = format!(
                                "{} rows · {:.1} ms{}",
                                grid.rows.len(),
                                elapsed.as_secs_f64() * 1000.,
                                if view == 0 && !grid.editable() {
                                    " · Read-only (no primary key)"
                                } else {
                                    ""
                                }
                            );
                            if let TabContent::Table(t) = &mut tab.content {
                                if view == 0 {
                                    if t.options.order_by.is_none()
                                        && t.object.kind != db::ObjectKind::Key
                                    {
                                        t.options.order_by = grid
                                            .keys
                                            .as_ref()
                                            .and_then(|k| k.first())
                                            .map(|k| (k.clone(), true));
                                    }
                                    t.data = grid;
                                    t.total = total;
                                } else {
                                    t.metadata = grid;
                                }
                            }
                        }
                        Err(e) => {
                            tab.status = "Could not load data".into();
                            a.error(e);
                        }
                    }
                }
                drop(ws);
                a.render();
            },
        );
    }
    fn run_query(self: &App, all: bool) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let Some((session, id, driver)) = self.context() else {
            return;
        };
        let Some(index) = self.active_index() else {
            return;
        };
        if self.workspaces.borrow()[index].busy() {
            return;
        }
        if !matches!(
            self.workspaces.borrow()[index].active().map(|t| &t.content),
            Some(TabContent::Query(_))
        ) {
            return;
        }
        let text = ui.get_sql().to_string();
        if let Some(t) = self.workspaces.borrow_mut()[index].active_mut()
            && let TabContent::Query(q) = &mut t.content
        {
            q.sql = text.clone();
        }
        let kind = self.workspaces.borrow()[index].config.kind;
        let cursor = (ui.get_cursor().max(0) as usize).min(text.len());
        let anchor = (ui.get_anchor().max(0) as usize).min(text.len());
        let selected = if all {
            text.clone()
        } else if cursor != anchor && text.is_char_boundary(cursor) && text.is_char_boundary(anchor)
        {
            text[cursor.min(anchor)..cursor.max(anchor)].to_string()
        } else if kind.is_sql() {
            let chars = text[..cursor].chars().count();
            editor::statement_at(&text, chars)
                .map(|(s, e)| text.chars().skip(s).take(e - s).collect::<String>())
                .unwrap_or_default()
        } else {
            let start = text[..cursor].rfind('\n').map_or(0, |i| i + 1);
            let end = text[cursor..].find('\n').map_or(text.len(), |i| cursor + i);
            text[start..end].to_string()
        };
        if selected.trim().is_empty() {
            self.notice("Type a query first.", false);
            return;
        }
        self.notice("", false);
        self.set_busy(&session, Some(id), true);
        let a = self.clone();
        let started = Instant::now();
        runtime::spawn(
            async move {
                driver
                    .execute(&selected)
                    .await
                    .map(|results| (results, selected.clone()))
                    .map_err(|e| (e, selected))
            },
            move |result| {
                let elapsed = started.elapsed();
                let (ok, sql) = match &result {
                    Ok((_, s)) => (true, s),
                    Err((_, s)) => (false, s),
                };
                if let Err(e) = library::append_history(&library::HistoryEntry {
                    connection_id: session.clone(),
                    sql: sql.clone(),
                    at: library::now(),
                    ok,
                    millis: elapsed.as_millis() as u64,
                }) {
                    a.error(e);
                }
                let mut ws = a.workspaces.borrow_mut();
                if let Some(tab) = ws
                    .iter_mut()
                    .find(|w| w.config.id == session)
                    .and_then(|w| w.tabs.iter_mut().find(|t| t.id == id))
                {
                    tab.busy = false;
                    match result {
                        Ok((results, _)) => {
                            let affected = results.iter().filter_map(|r| r.affected).sum::<u64>();
                            tab.status = format!(
                                "{} results · {} affected rows · {:.1} ms",
                                results.len(),
                                affected,
                                elapsed.as_secs_f64() * 1000.
                            );
                            if let TabContent::Query(q) = &mut tab.content {
                                q.grid = results.first().map(Grid::from_result).unwrap_or_default();
                                q.results = results;
                                q.result = 0;
                            }
                        }
                        Err((e, _)) => {
                            tab.status =
                                format!("Query failed · {:.1} ms", elapsed.as_secs_f64() * 1000.);
                            a.error(e);
                        }
                    }
                }
                drop(ws);
                a.render();
                a.refresh_library();
            },
        );
    }
    fn commit(self: &App) {
        let Some((session, id, driver)) = self.context() else {
            return;
        };
        let Some(index) = self.active_index() else {
            return;
        };
        let ws = self.workspaces.borrow();
        if ws[index].busy() {
            return;
        }
        let Some(tab) = ws[index].active() else {
            return;
        };
        let TabContent::Table(t) = &tab.content else {
            return;
        };
        let object = t.object.clone();
        let changes = match t.data.changes() {
            Ok(c) if c.is_empty() => return,
            Ok(c) => c,
            Err(e) => {
                self.error(e);
                return;
            }
        };
        drop(ws);
        self.set_busy(&session, Some(id), true);
        let a = self.clone();
        runtime::spawn(
            async move { driver.apply_changes(&object, &changes).await },
            move |result| {
                let mut ws = a.workspaces.borrow_mut();
                let mut reload = false;
                if let Some(tab) = ws
                    .iter_mut()
                    .find(|w| w.config.id == session)
                    .and_then(|w| w.tabs.iter_mut().find(|t| t.id == id))
                {
                    tab.busy = false;
                    match result {
                        Ok(count) => {
                            if let TabContent::Table(t) = &mut tab.content {
                                t.data.rows.retain(|r| !r.deleted);
                                for r in &mut t.data.rows {
                                    r.original = Some(r.current.clone());
                                }
                                t.data.selected = None;
                            }
                            a.notice(format!("Committed {count} row changes."), false);
                            reload = true;
                        }
                        Err(e) => a.error(e),
                    }
                }
                drop(ws);
                a.render();
                if reload {
                    a.reload_tab(&session, id);
                }
            },
        );
    }
    /// Route native Edit-menu actions through Slint's focused text input.
    /// Disable the menu accelerators during dispatch so the generated key event cannot invoke the menu recursively.
    fn edit_text(&self,action: &str) {
        use slint::platform::{Key,WindowEvent};
        let Some(ui) = self.ui.upgrade() else { return; };
        let (key,shift) = match action { "undo" => ("z",false), "redo" => ("z",true), "cut" => ("x",false), "copy" => ("c",false), "paste" => ("v",false), "select-all" => ("a",false), _ => return };
        ui.set_edit_dispatching(true);
        let window = ui.window();
        window.dispatch_event(WindowEvent::KeyPressed { text: Key::Control.into() });
        if shift { window.dispatch_event(WindowEvent::KeyPressed { text: Key::Shift.into() }); }
        window.dispatch_event(WindowEvent::KeyPressed { text: key.into() });
        window.dispatch_event(WindowEvent::KeyReleased { text: key.into() });
        if shift { window.dispatch_event(WindowEvent::KeyReleased { text: Key::Shift.into() }); }
        window.dispatch_event(WindowEvent::KeyReleased { text: Key::Control.into() });
        ui.set_edit_dispatching(false);
    }
    fn command(self: &App, action: &str) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        match action {
            "dismiss" => ui.set_notice("".into()),
            "auth-submit" => self.submit_password(),
            "auth-cancel" => {
                ui.set_auth_dialog(false);
                ui.set_auth_keychain(true);
                if self.pending_export.borrow_mut().take().is_some() {
                    return;
                }
                if let Some(((_, _, _), _)) = self.auth_retry.borrow_mut().take() {
                    self.notice(ui.get_auth_detail().as_str(), true);
                }
            }
            "quit" => {
                let quit = |a: &App| {
                    if let Some(ui) = a.ui.upgrade() {
                        let _ = ui.hide();
                    }
                    let _ = slint::quit_event_loop();
                };
                if self
                    .workspaces
                    .borrow()
                    .iter()
                    .any(|w| w.pending() > 0 || w.busy())
                {
                    self.confirm(
                        "Close Pueo?",
                        "Pending changes will be discarded and connections will close.",
                        "Close Pueo",
                        quit,
                    );
                } else {
                    quit(self);
                }
            }
            "confirm" => {
                ui.set_confirm_dialog(false);
                let done = self.confirmation.borrow_mut().take();
                if let Some(done) = done {
                    done(self);
                }
            }
            "new-connection" => self.show_form(None),
            "open-file" => {
                if let Some(file) = rfd::FileDialog::new()
                    .set_title("Open SQLite database")
                    .add_filter("SQLite database", &["db", "sqlite", "sqlite3"])
                    .pick_file()
                {
                    let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
                    cfg.file_path = file.to_string_lossy().into_owned();
                    self.connect(cfg, None, None, false);
                }
            }
            "import-open" => self.open_import(),
            "import-file" => {
                let Some(file) = rfd::FileDialog::new()
                    .set_title("Import connections from a file")
                    .add_filter("Connection files", &["tableplusconnection", "json", "csv", "plist", "txt", "url", "db", "sqlite", "sqlite3"])
                    .pick_file()
                else {
                    return;
                };
                if file.extension().is_some_and(|e| e == "tableplusconnection") {
                    self.open_export(file, String::new());
                    return;
                }
                let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.show_import(&name, crate::import::load_file(&file));
            }
            "import-all" | "import-none" => {
                for item in self.import_batch.borrow_mut().iter_mut() {
                    item.1 = action == "import-all" && !item.2;
                }
                self.refresh_import();
            }
            "import-back" => ui.set_import_step(0),
            "import-run" => self.run_import(),
            a if a.starts_with("import-app:") => {
                let Some(app) = a["import-app:".len()..].parse::<usize>().ok().and_then(|i| crate::import::App::ALL.get(i).copied()) else {
                    return;
                };
                match app.path() {
                    Some(path) => self.show_import(app.name(), app.load(&path)),
                    None => self.notice(format!("{} was not found on this computer.", app.name()), true),
                }
            }
            a if a.starts_with("import-toggle:") => {
                if let Some(i) = a["import-toggle:".len()..].parse::<usize>().ok()
                    && let Some(item) = self.import_batch.borrow_mut().get_mut(i)
                    && !item.2
                {
                    item.1 = !item.1;
                }
                self.refresh_import();
            }
            "export-connections" => {
                let Some(file) = rfd::FileDialog::new()
                    .set_title("Export connections")
                    .set_file_name("pueo-connections.json")
                    .save_file()
                else {
                    return;
                };
                match crate::import::write_pueo(&file, &self.store.borrow()) {
                    Ok(()) => self.notice(
                        format!(
                            "Exported {} connections to {}. Passwords are not included.",
                            self.store.borrow().connections.len(),
                            file.display()
                        ),
                        false,
                    ),
                    Err(e) => self.error(e),
                }
            }
            "new-group" => self.show_group(&config::Group::new(""), -1),
            "edit-group" => {
                let s = self.store.borrow();
                if let Some(g) = s.groups.first() {
                    self.show_group(g, 0);
                }
            }
            "group-save" => {
                let name = ui.get_group_name().trim().to_string();
                if name.is_empty() {
                    self.notice("Enter a group name.", true);
                    return;
                }
                let mut store = self.store.borrow().clone();
                let mut group = usize::try_from(ui.get_group_index())
                    .ok()
                    .and_then(|index| store.groups.get(index).cloned())
                    .unwrap_or_else(|| config::Group::new(&name));
                group.name = name;
                group.color = usize::try_from(ui.get_group_color() - 1)
                    .ok()
                    .and_then(|i| Color::ALL.get(i).copied());
                let value = ui.get_group_icon_value().to_string();
                group.icon = match ui.get_group_icon_kind() {
                    1 => config::Icon::Emoji(value),
                    2 => config::Icon::Image(value),
                    _ => config::Icon::Symbol(if value.is_empty() {
                        "folder-symbolic".into()
                    } else {
                        value
                    }),
                };
                if let Ok(index) = usize::try_from(ui.get_group_index())
                    && index < store.groups.len()
                {
                    store.groups[index] = group;
                } else {
                    store.groups.push(group);
                }
                if self.save_store(store) {
                    ui.set_group_dialog(false);
                }
            }
            "group-delete" => {
                let Some(id) = usize::try_from(ui.get_group_index())
                    .ok()
                    .and_then(|i| self.store.borrow().groups.get(i).map(|g| g.id.clone()))
                else {
                    return;
                };
                let mut store = self.store.borrow().clone();
                store.delete_group(&id);
                if self.save_store(store) {
                    ui.set_group_dialog(false);
                }
            }
            "group-image" => {
                if let Some(file) = rfd::FileDialog::new()
                    .add_filter("Image", &["png", "jpg", "jpeg", "svg"])
                    .pick_file()
                {
                    match config::import_icon_image(&file) {
                        Ok(path) => ui.set_group_icon_value(path.into()),
                        Err(e) => self.error(e),
                    }
                }
            }
            "new-query" => self.new_query(String::new(), "Query"),
            "close-tab" => { if let Some((session,id,_)) = self.context() { self.close_tab(&session,id); } },
            "previous-tab" | "next-tab" => {
                if let Some(index) = self.active_index() { let ws = self.workspaces.borrow(); let w = &ws[index];
                    if !w.tabs.is_empty() { let current = w.tabs.iter().position(|t| Some(t.id) == w.active_tab).unwrap_or(0); let next = if action == "next-tab" { (current+1)%w.tabs.len() } else { (current+w.tabs.len()-1)%w.tabs.len() }; drop(ws); self.tab_action(next as i32,"switch"); }
                }
            },
            "edit-connection" => { if let Some(index) = self.active_index() { self.show_form(Some(&self.workspaces.borrow()[index].config)); } },
            "documentation" => { let a = self.clone(); runtime::spawn(async {
                #[cfg(target_os = "macos")] let mut command = tokio::process::Command::new("open");
                #[cfg(target_os = "linux")] let mut command = tokio::process::Command::new("xdg-open");
                let status = command.arg("https://github.com/optis20052/Pueo").status().await?;
                ensure!(status.success(),"Could not open the documentation"); Ok::<_,anyhow::Error>(())
            },move |result| { if let Err(e) = result { a.error(e); } }); },
            _ if action.starts_with("edit:") => self.edit_text(&action[5..]),
            "close-connection" => self.close_connection(),
            a if a.starts_with("appearance:") => {
                let mode = a["appearance:".len()..].parse().unwrap_or(0);
                ui.invoke_set_appearance(mode);
                window_theme(&ui, mode);
                if let Err(e) = config::save_appearance(mode) {
                    self.notice(&format!("Could not save the appearance: {e}"), true);
                }
            }
            a if a.starts_with("schema:") => {
                if let Some(i) = self.active_index() {
                    self.workspaces.borrow_mut()[i].schema_filter = a["schema:".len()..].to_string();
                    self.refresh_objects();
                }
            }
            "clone-run" => {
                ui.set_clone_dialog(false);
                let Some(i) = self.active_index() else {
                    return;
                };
                let name = ui.get_clone_name().trim().to_string();
                let Some(object) = usize::try_from(ui.get_clone_index())
                    .ok()
                    .and_then(|j| self.workspaces.borrow()[i].objects.get(j).cloned())
                else {
                    return;
                };
                if name.is_empty() || name == object.name {
                    self.notice("Choose a new name for the copy.", true);
                    return;
                }
                let kind = self.workspaces.borrow()[i].config.kind;
                let sql = db::clone_sql(kind, &object, &name, ui.get_clone_data()).join(";\n");
                self.run_sql(sql, format!("Created {name}."));
            }
            "sql-edited" => {
                ui.set_line_numbers(line_numbers(ui.get_sql().as_str()).into());
                if let Some(i) = self.active_index()
                    && let Some(tab) = self.workspaces.borrow_mut()[i].active_mut()
                    && let TabContent::Query(q) = &mut tab.content
                {
                    q.sql = ui.get_sql().to_string();
                }
                ui.set_completion_open(false);
            }
            "run" => self.run_query(false),
            "run-all" => self.run_query(true),
            "stop" => {
                let Some(i) = self.active_index() else {
                    return;
                };
                let driver = self.workspaces.borrow()[i].driver.clone();
                let a = self.clone();
                runtime::spawn(async move { driver.cancel().await }, move |result| {
                    if let Err(e) = result {
                        a.error(e);
                    }
                });
            }
            "refresh" => {
                if ui.get_query() {
                    self.run_query(true);
                } else {
                    self.guard(|a| a.load_table());
                }
            }
            "filter" => {
                let text = ui.get_filter().to_string();
                self.guard(move |a| {
                    if let Some(i) = a.active_index()
                        && let Some(t) = a.workspaces.borrow_mut()[i].active_mut()
                        && let TabContent::Table(table) = &mut t.content
                    {
                        table.filter_text = text.clone();
                        table.options.filter = (!text.trim().is_empty()).then_some(text);
                        table.options.offset = 0;
                        table.view = 0;
                    }
                    a.load_table();
                });
            }
            "filter-add" => {
                let col = self
                    .filter_rules
                    .row_data(self.filter_rules.row_count().saturating_sub(1))
                    .map_or(0, |r| r.col);
                self.filter_rules.push(FilterRule { col, ..Default::default() });
            }
            "filter-apply" | "filter-clear" => {
                let Some(i) = self.active_index() else {
                    return;
                };
                let kind = self.workspaces.borrow()[i].config.kind;
                let clear = action == "filter-clear";
                if clear {
                    self.filter_rules.set_vec(vec![FilterRule::default()]);
                    ui.set_filter("".into());
                } else if !ui.get_filter_sql_mode() {
                    let columns = ui.get_columns();
                    let conditions: Vec<filter::Condition> = self
                        .filter_rules
                        .iter()
                        .filter_map(|r| columns.row_data(r.col.max(0) as usize).map(|c| (c, r)))
                        .map(|(column, r)| filter::Condition {
                            column: column.to_string(),
                            op: filter::Op::ALL.get(r.op.max(0) as usize).copied().unwrap_or(filter::Op::ALL[0]),
                            value: r.value.to_string(),
                        })
                        .collect();
                    let sql = filter::build_where(kind, &conditions, ui.get_filter_match_all()).unwrap_or_default();
                    ui.set_filter(sql.into());
                }
                if let Some(tab) = self.rules_tab.get() {
                    self.saved_rules
                        .borrow_mut()
                        .insert(tab, (self.filter_rules.iter().collect(), ui.get_filter_match_all()));
                }
                self.command("filter");
            }
            a if a.starts_with("filter-remove:") => {
                if let Ok(index) = a["filter-remove:".len()..].parse::<usize>()
                    && index < self.filter_rules.row_count()
                {
                    self.filter_rules.remove(index);
                    if self.filter_rules.row_count() == 0 {
                        self.command("filter-clear");
                    }
                }
            }
            "prev" | "next" => {
                let next = action == "next";
                self.guard(move |a| {
                    if let Some(i) = a.active_index()
                        && let Some(t) = a.workspaces.borrow_mut()[i].active_mut()
                        && let TabContent::Table(table) = &mut t.content
                    {
                        if next {
                            table.options.offset += PAGE_SIZE;
                        } else {
                            table.options.offset = table.options.offset.saturating_sub(PAGE_SIZE);
                        }
                    }
                    a.load_table();
                });
            }
            "add-row" | "duplicate-row" | "delete-row" | "discard" => {
                let Some(i) = self.active_index() else {
                    return;
                };
                if self.workspaces.borrow()[i].busy() {
                    return;
                }
                if let Some(tab) = self.workspaces.borrow_mut()[i].active_mut() {
                    if action == "discard" {
                        if let TabContent::Table(table) = &mut tab.content {
                            table.data.discard();
                        }
                    } else {
                        let grid = tab.grid_mut();
                        match action {
                            "add-row" => grid.add(None),
                            "duplicate-row" => grid.duplicate_selected(),
                            "delete-row" => grid.delete_selected(),
                            _ => {}
                        }
                    }
                }
                self.render();
            }
            "save" => {
                if ui.get_query() {
                    ui.set_save_name("".into());
                    ui.set_save_dialog(true);
                } else {
                    self.commit();
                }
            }
            "commit" => self.commit(),
            "cell-apply" => {
                let Some(i) = self.active_index() else {
                    return;
                };
                if self.workspaces.borrow()[i].busy() {
                    return;
                }
                if let Some(tab) = self.workspaces.borrow_mut()[i].active_mut() {
                    let grid = tab.grid_mut();
                    if let Some((r, c)) = grid.selected {
                        grid.set_cell(
                            r,
                            c,
                            (!ui.get_cell_null()).then(|| ui.get_cell_value().to_string()),
                        );
                    }
                }
                ui.set_cell_dialog(false);
                self.render();
            }
            "copy" => {
                let Some(i) = self.active_index() else {
                    return;
                };
                let ws = self.workspaces.borrow();
                let Some(tab) = ws[i].active() else {
                    return;
                };
                let g = tab.grid();
                let rows = g.selected_rows();
                let tsv = if rows.is_empty() {
                    g.values().iter().map(|r| row_tsv(r)).collect::<Vec<_>>().join("\n")
                } else {
                    rows.iter().filter_map(|&r| g.rows.get(r)).map(|r| row_tsv(&r.current)).collect::<Vec<_>>().join("\n")
                };
                let count = if rows.is_empty() { g.rows.len() } else { rows.len() };
                ui.set_clipboard_content(tsv.into());
                ui.invoke_copy_grid();
                self.notice(format!("Copied {count} row{} as tab-separated text.", if count == 1 { "" } else { "s" }), false);
            }
            "copy-cell" | "set-null" => {
                let Some(i) = self.active_index() else {
                    return;
                };
                let mut ws = self.workspaces.borrow_mut();
                let Some(tab) = ws[i].active_mut() else {
                    return;
                };
                if tab.busy {
                    return;
                }
                let g = tab.grid_mut();
                let Some((r, c)) = g.selected else {
                    return;
                };
                if action == "set-null" {
                    let rows = g.selected_rows();
                    for row in rows {
                        g.set_cell(row, c, None);
                    }
                    drop(ws);
                    self.render();
                    return;
                }
                let value = g.rows.get(r).and_then(|row| row.current.get(c)).cloned().flatten();
                ui.set_clipboard_content(value.unwrap_or_default().into());
                ui.invoke_copy_grid();
            }
            "export" => self.export(),
            "import" => self.import(),
            "objects" => self.reload_objects(None),
            "scan" => {
                let pattern = ui.get_object_search().trim().to_string();
                self.reload_objects((!pattern.is_empty()).then_some(pattern));
            }
            "console" => self.refresh_console(),
            "clear-console" => {
                if let Some(i) = self.active_index() {
                    self.workspaces.borrow()[i].log.clear();
                }
                ui.set_console_seq(-1);
                self.refresh_console();
            }
            "library" => self.refresh_library(),
            "clear-history" => {
                let Some(i) = self.active_index() else {
                    return;
                };
                let id = self.workspaces.borrow()[i].config.id.clone();
                self.confirm(
                    "Clear query history?",
                    "This removes the stored query history for this connection.",
                    "Clear history",
                    move |a| {
                        if let Err(e) = library::clear_history(&id) {
                            a.error(e);
                        }
                        a.refresh_library();
                    },
                );
            }
            "query-save" => {
                let Some(i) = self.active_index() else {
                    return;
                };
                let name = ui.get_save_name().trim().to_string();
                if name.is_empty() {
                    self.notice("Enter a query name.", true);
                    return;
                }
                let id = self.workspaces.borrow()[i].config.id.clone();
                match library::save_query(&id, &name, ui.get_sql().as_str()) {
                    Ok(()) => {
                        ui.set_save_dialog(false);
                        self.notice("Query saved.", false);
                        self.refresh_library();
                    }
                    Err(e) => self.error(e),
                }
            }
            "complete" => self.complete(),
            _ if action.starts_with("view:") => {
                let view = action[5..].parse::<i32>().unwrap_or(0).clamp(0, 2);
                if let Some(i) = self.active_index() {
                    if self.workspaces.borrow()[i].busy() {
                        return;
                    }
                    if let Some(tab) = self.workspaces.borrow_mut()[i].active_mut()
                        && let TabContent::Table(t) = &mut tab.content
                    {
                        t.view = view;
                    }
                    if view == 0 {
                        self.render();
                    } else {
                        self.load_table();
                    }
                }
            }
            _ if action.starts_with("sort:") => {
                let column = action[5..].parse::<usize>().unwrap_or(usize::MAX);
                self.guard(move |a| {
                    if let Some(i) = a.active_index()
                        && let Some(tab) = a.workspaces.borrow_mut()[i].active_mut()
                        && let TabContent::Table(t) = &mut tab.content
                        && t.view == 0
                        && let Some(name) = t.data.columns.get(column)
                    {
                        let asc = !t
                            .options
                            .order_by
                            .as_ref()
                            .is_some_and(|(c, asc)| c == name && *asc);
                        t.options.order_by = Some((name.clone(), asc));
                        t.options.offset = 0;
                    }
                    a.load_table();
                });
            }
            _ if action.starts_with("session:") => {
                if let Ok(index) = action[8..].parse::<i32>() {
                    self.session_action(index - 1, "switch");
                }
            }
            _ if action.starts_with("database:") => self.switch_database(action[9..].into()),
            _ if action.starts_with("group-select:") => {
                let index = action[13..].parse::<usize>().unwrap_or(0).saturating_sub(1);
                if let Some(group) = self.store.borrow().groups.get(index) {
                    self.show_group(group, index as i32);
                }
            }
            _ => {}
        }
    }
    fn close_connection(self: &App) {
        let Some(i) = self.active_index() else {
            return;
        };
        let ws = self.workspaces.borrow();
        let w = &ws[i];
        if w.busy() {
            drop(ws);
            self.notice("Stop the running query or wait before disconnecting.", true);
            return;
        }
        let id = w.config.id.clone();
        let pending = w.pending();
        drop(ws);
        let close = move |a: &App| {
            let mut ws = a.workspaces.borrow_mut();
            ws.retain(|w| w.config.id != id);
            *a.active.borrow_mut() = ws.last().map(|w| w.config.id.clone());
            drop(ws);
            a.render();
        };
        if pending > 0 {
            self.confirm(
                "Disconnect with pending edits?",
                "Uncommitted edits in this connection will be discarded.",
                "Discard and disconnect",
                close,
            );
        } else {
            close(self);
        }
    }
    fn cell_action(self: &App, row: i32, col: i32, edit: bool) {
        let Some(i) = self.active_index() else {
            return;
        };
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let mut ws = self.workspaces.borrow_mut();
        let Some(tab) = ws[i].active_mut() else {
            return;
        };
        if tab.busy {
            return;
        }
        let g = tab.grid_mut();
        let (Ok(row), Ok(col)) = (usize::try_from(row), usize::try_from(col)) else {
            return;
        };
        let Some(value) = g.rows.get(row).and_then(|r| r.current.get(col)) else {
            return;
        };
        if edit {
            ui.set_cell_title(format!("{} · Row {}", g.columns[col], row + 1).into());
            ui.set_cell_value(value.as_deref().unwrap_or("").into());
            ui.set_cell_null(value.is_none());
            let choices = g.choices.get(&g.columns[col]).map_or(Vec::new(), |c| c.values.clone());
            ui.set_cell_choice_search("".into());
            ui.set_cell_choices(strings(choices.clone()));
            *self.cell_choices.borrow_mut() = choices;
            ui.set_cell_dialog(true);
        }
        g.select(row, col, 0);
        if !edit {
            ui.set_selected_row(row as i32);
            ui.set_selected_column(col as i32);
            ui.set_fields(model(
                g.columns
                    .iter()
                    .zip(&g.rows[row].current)
                    .map(|(c, v)| FieldItem {
                        name: c.as_str().into(),
                        value: v.as_deref().unwrap_or("").into(),
                        null: v.is_none(),
                    })
                    .collect(),
            ));
            return;
        }
        drop(ws);
        self.render();
    }
    fn cell_select(&self, row: i32, col: i32, mode: i32) {
        let Some(i) = self.active_index() else {
            return;
        };
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let mut ws = self.workspaces.borrow_mut();
        let Some(tab) = ws[i].active_mut() else {
            return;
        };
        let g = tab.grid_mut();
        let (Ok(row), Ok(col)) = (usize::try_from(row), usize::try_from(col)) else {
            return;
        };
        if row >= g.rows.len() {
            return;
        }
        let before = g.marked.clone();
        g.select(row, col, mode);
        let rows = ui.get_rows();
        for changed in before.symmetric_difference(&g.marked) {
            if let Some(mut item) = rows.row_data(*changed) {
                item.marked = g.marked.contains(changed);
                rows.set_row_data(*changed, item);
            }
        }
        ui.set_selected_row(row as i32);
        ui.set_selected_column(col as i32);
        ui.set_selected_count(g.selected_rows().len() as i32);
        ui.set_fields(model(
            g.columns
                .iter()
                .zip(&g.rows[row].current)
                .map(|(c, v)| FieldItem {
                    name: c.as_str().into(),
                    value: v.as_deref().unwrap_or("").into(),
                    null: v.is_none(),
                })
                .collect(),
        ));
    }
    fn paste_rows(&self, text: &str) {
        let Some(i) = self.active_index() else {
            return;
        };
        let mut ws = self.workspaces.borrow_mut();
        if ws[i].busy() {
            return;
        }
        let Some(tab) = ws[i].active_mut() else {
            return;
        };
        let g = tab.grid_mut();
        if !g.editable() {
            drop(ws);
            self.notice("This result can't be edited, so rows can't be pasted into it.", true);
            return;
        }
        let added = g.paste_rows(text);
        drop(ws);
        if added == 0 {
            self.notice("The clipboard has no rows to paste. Copy tab-separated rows first.", true);
            return;
        }
        self.notice(format!("Pasted {added} row{} — commit to save them.", if added == 1 { "" } else { "s" }), false);
        self.render();
    }
    fn field_edit(&self, column: i32, value: &str, null: bool) {
        let Some(i) = self.active_index() else {
            return;
        };
        let mut ws = self.workspaces.borrow_mut();
        if ws[i].busy() {
            return;
        }
        if let Some(tab) = ws[i].active_mut() {
            let g = tab.grid_mut();
            if let Some((row, _)) = g.selected
                && let Ok(col) = usize::try_from(column)
            {
                g.set_cell(row, col, (!null).then(|| value.to_string()));
            }
        }
        drop(ws);
        self.render();
    }
    fn save_store(&self, store: Store) -> bool {
        if let Err(e) = config::save_store(&store) {
            self.error(e);
            return false;
        }
        *self.store.borrow_mut() = store;
        self.refresh_home();
        true
    }
    fn connection_action(self: &App, id: &str, action: &str) {
        if action.starts_with("group-") {
            let mut store = self.store.borrow().clone();
            let Some(index) = store.groups.iter().position(|g| g.id == id) else {
                return;
            };
            match action {
                "group-toggle" => store.groups[index].collapsed = !store.groups[index].collapsed,
                "group-edit" => {
                    self.show_group(&store.groups[index], index as i32);
                    return;
                }
                "group-up" if index > 0 => {
                    let target = store.groups[index - 1].id.clone();
                    store.move_group(id, &target, false);
                }
                "group-down" if index + 1 < store.groups.len() => {
                    let target = store.groups[index + 1].id.clone();
                    store.move_group(id, &target, true);
                }
                _ => return,
            }
            self.save_store(store);
            return;
        }
        if let Some((after, target)) = action
            .strip_prefix("drop-before:")
            .map(|t| (false, t))
            .or_else(|| action.strip_prefix("drop-after:").map(|t| (true, t)))
        {
            let mut store = self.store.borrow().clone();
            let target_group = store.groups.iter().any(|g| g.id == target);
            if store.groups.iter().any(|g| g.id == id) {
                let target = if target_group {
                    Some(target.to_string())
                } else {
                    store
                        .connections
                        .iter()
                        .find(|c| c.id == target)
                        .and_then(|c| c.group.clone())
                };
                let Some(target) = target else { return };
                store.move_group(id, &target, after);
            } else if target_group {
                store.move_connection(id, &config::ConnectionDrop::IntoGroup(Some(target.into())));
            } else {
                store.move_connection(
                    id,
                    &if after {
                        config::ConnectionDrop::After(target.into())
                    } else {
                        config::ConnectionDrop::Before(target.into())
                    },
                );
            }
            self.save_store(store);
            return;
        }
        if action == "up" || action == "down" {
            let mut store = self.store.borrow().clone();
            let Some(index) = store.connections.iter().position(|c| c.id == id) else {
                return;
            };
            let peer = if action == "up" {
                (0..index)
                    .rev()
                    .find(|i| store.connections[*i].group == store.connections[index].group)
            } else {
                (index + 1..store.connections.len())
                    .find(|i| store.connections[*i].group == store.connections[index].group)
            };
            if let Some(peer) = peer {
                let target = store.connections[peer].id.clone();
                store.move_connection(
                    id,
                    &if action == "up" {
                        config::ConnectionDrop::Before(target)
                    } else {
                        config::ConnectionDrop::After(target)
                    },
                );
                self.save_store(store);
            }
            return;
        }
        let Some(cfg) = self
            .store
            .borrow()
            .connections
            .iter()
            .find(|c| c.id == id)
            .cloned()
        else {
            return;
        };
        match action {
            "connect" => self.connect(cfg, None, None, true),
            "edit" => self.show_form(Some(&cfg)),
            "delete" => {
                self.confirm(
                    "Delete saved connection?",
                    &format!("Remove {} and its saved passwords?", cfg.display_name()),
                    "Delete connection",
                    move |a| {
                        let a = a.clone();
                        runtime::spawn(
                            async move {
                                if cfg.kind != DbKind::Sqlite {
                                    secrets::delete_passwords(&cfg.id).await?;
                                }
                                Ok::<_, anyhow::Error>(cfg.id)
                            },
                            move |result| match result {
                                Ok(id) => {
                                    let mut store = a.store.borrow().clone();
                                    store.connections.retain(|c| c.id != id);
                                    a.save_store(store);
                                }
                                Err(e) => a.error(e),
                            },
                        );
                    },
                );
            }
            "duplicate" => {
                let a = self.clone();
                runtime::spawn(
                    async move {
                        let mut copy = cfg.clone();
                        copy.id = uuid::Uuid::new_v4().to_string();
                        copy.name = format!("{} (copy)", cfg.display_name());
                        if cfg.kind != DbKind::Sqlite {
                            for slot in [secrets::Slot::Database, secrets::Slot::Ssh] {
                                if let Some(password) = secrets::get_password(&cfg.id, slot).await?
                                {
                                    secrets::set_password(
                                        &copy.id,
                                        slot,
                                        &copy.display_name(),
                                        &password,
                                    )
                                    .await?;
                                }
                            }
                        }
                        Ok::<_, anyhow::Error>(copy)
                    },
                    move |result| match result {
                        Ok(copy) => {
                            let mut store = a.store.borrow().clone();
                            store.connections.push(copy);
                            a.save_store(store);
                        }
                        Err(e) => a.error(e),
                    },
                );
            }
            _ => {}
        }
    }
    fn show_group(&self, g: &config::Group, index: i32) {
        if let Some(ui) = self.ui.upgrade() {
            ui.set_group_index(index);
            ui.set_group_name(g.name.as_str().into());
            ui.set_group_color(g.color.map_or(0, |c| {
                Color::ALL.iter().position(|x| *x == c).unwrap() as i32 + 1
            }));
            let (kind, value) = match &g.icon {
                config::Icon::Symbol(s) => (0, s),
                config::Icon::Emoji(s) => (1, s),
                config::Icon::Image(s) => (2, s),
            };
            ui.set_group_icon_kind(kind);
            ui.set_group_icon_value(value.as_str().into());
            ui.set_group_dialog(true);
        }
    }
    fn show_form(&self, cfg: Option<&ConnectionConfig>) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let c = cfg
            .cloned()
            .unwrap_or_else(|| ConnectionConfig::new(DbKind::Postgres));
        let store = self.store.borrow();
        ui.set_form(ConnectionForm {
            id: cfg.map_or("", |c| c.id.as_str()).into(),
            name: c.name.into(),
            kind: DbKind::ALL.iter().position(|k| *k == c.kind).unwrap() as i32,
            host: c.host.into(),
            port: c.port.to_string().into(),
            user: c.user.into(),
            database: c.database.into(),
            file: c.file_path.into(),
            schema: c.schema.into(),
            tls: c.use_tls,
            password: "".into(),
            ssh: c.ssh.enabled,
            ssh_host: c.ssh.host.into(),
            ssh_port: c.ssh.port.to_string().into(),
            ssh_user: c.ssh.user.into(),
            ssh_auth: SshAuth::ALL.iter().position(|a| *a == c.ssh.auth).unwrap() as i32,
            ssh_key: c.ssh.key_path.into(),
            ssh_secret: "".into(),
            color: c.color.map_or(0, |c| {
                Color::ALL.iter().position(|x| *x == c).unwrap() as i32 + 1
            }),
            tag: c.tag.map_or(0, |c| {
                Tag::ALL.iter().position(|x| *x == c).unwrap() as i32 + 1
            }),
            group: c
                .group
                .and_then(|id| store.groups.iter().position(|g| g.id == id))
                .map_or(0, |i| i as i32 + 1),
            remember: true,
        });
        ui.set_form_notice("".into());
        ui.set_form_busy(false);
        ui.set_connection_dialog(true);
    }
    fn config_from_form(&self, f: &ConnectionForm) -> Result<ConnectionConfig> {
        let kind = DbKind::ALL
            .get(f.kind.max(0) as usize)
            .copied()
            .context("Unknown database engine")?;
        let mut c = ConnectionConfig::new(kind);
        if !f.id.is_empty() {
            c.id = f.id.to_string();
        }
        c.name = f.name.trim().into();
        c.host = f.host.trim().into();
        c.user = f.user.trim().into();
        c.database = f.database.trim().into();
        c.schema = f.schema.trim().into();
        c.file_path = f.file.trim().into();
        c.use_tls = f.tls;
        c.port = if kind == DbKind::Sqlite {
            0
        } else {
            f.port
                .parse::<u16>()
                .context("Port must be a number from 1 to 65535")?
        };
        if kind == DbKind::Sqlite {
            ensure!(
                !c.file_path.is_empty(),
                "Choose a SQLite file or enter :memory:"
            );
        } else {
            ensure!(!c.host.is_empty(), "Enter the database host");
            ensure!(c.port > 0, "Port cannot be zero");
        }
        if kind == DbKind::Redis && !c.database.is_empty() {
            c.database
                .parse::<u32>()
                .context("Redis database must be a nonnegative number")?;
        }
        c.ssh.enabled = f.ssh && kind != DbKind::Sqlite;
        c.ssh.host = f.ssh_host.trim().into();
        c.ssh.user = f.ssh_user.trim().into();
        c.ssh.key_path = f.ssh_key.trim().into();
        c.ssh.auth = *SshAuth::ALL
            .get(f.ssh_auth.max(0) as usize)
            .context("Invalid SSH authentication")?;
        if c.ssh.enabled {
            c.ssh.port = f.ssh_port.parse::<u16>().context("Invalid SSH port")?;
            ensure!(
                c.ssh.port > 0 && !c.ssh.host.is_empty(),
                "Enter the SSH host and port"
            );
        }
        c.color = f
            .color
            .checked_sub(1)
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| Color::ALL.get(i).copied());
        c.tag = f
            .tag
            .checked_sub(1)
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| Tag::ALL.get(i).copied());
        c.group = f
            .group
            .checked_sub(1)
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| self.store.borrow().groups.get(i).map(|g| g.id.clone()));
        Ok(c)
    }
    fn form_action(self: &App, action: &str) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        if ui.get_form_busy() {
            return;
        }
        if action == "kind" {
            let mut f = ui.get_form();
            f.port = DbKind::ALL
                .get(f.kind.max(0) as usize)
                .map_or(5432, |k| k.default_port())
                .to_string()
                .into();
            ui.set_form(f);
            return;
        }
        if action == "file" || action == "key" {
            if let Some(path) = rfd::FileDialog::new().pick_file() {
                let mut f = ui.get_form();
                if action == "file" {
                    f.file = path.to_string_lossy().as_ref().into();
                } else {
                    f.ssh_key = path.to_string_lossy().as_ref().into();
                }
                ui.set_form(f);
            }
            return;
        }
        let f = ui.get_form();
        let cfg = match self.config_from_form(&f) {
            Ok(c) => c,
            Err(e) => {
                ui.set_form_notice(format!("{e:#}").into());
                return;
            }
        };
        let action = action.to_string();
        let a = self.clone();
        let old_id = f.id.to_string();
        let remember = f.remember;
        // UI-generated SharedStrings never cross onto Tokio threads.
        let password = f.password.to_string();
        let ssh_secret = f.ssh_secret.to_string();
        ui.set_form_busy(true);
        ui.set_form_ok(false);
        ui.set_form_notice("Working…".into());
        runtime::spawn(
            async move {
                let mut password = (!password.is_empty()).then_some(password);
                let mut ssh_secret = (!ssh_secret.is_empty()).then_some(ssh_secret);
                if cfg.kind != DbKind::Sqlite && !old_id.is_empty() {
                    if password.is_none() {
                        password = secrets::get_password(&old_id, secrets::Slot::Database).await?;
                    }
                    if cfg.ssh.enabled && ssh_secret.is_none() {
                        ssh_secret = secrets::get_password(&old_id, secrets::Slot::Ssh).await?;
                    }
                }
                if action == "test" {
                    let driver = db::connect(&cfg, password, ssh_secret).await?;
                    let info = driver.server_info().await?;
                    return Ok::<_, anyhow::Error>((cfg, None, None, action, Some(info)));
                }
                if remember && cfg.kind != DbKind::Sqlite {
                    if let Some(p) = &password {
                        secrets::set_password(
                            &cfg.id,
                            secrets::Slot::Database,
                            &cfg.display_name(),
                            p,
                        )
                        .await?;
                    }
                    if let Some(p) = &ssh_secret {
                        secrets::set_password(&cfg.id, secrets::Slot::Ssh, &cfg.display_name(), p)
                            .await?;
                    }
                }
                Ok((cfg, password, ssh_secret, action, None))
            },
            move |result| {
                let Some(ui) = a.ui.upgrade() else {
                    return;
                };
                ui.set_form_busy(false);
                match result {
                    Ok((cfg, password, ssh_secret, action, info)) => {
                        if let Some(info) = info {
                            ui.set_form_ok(true);
                            ui.set_form_notice(format!("Connected successfully · {info}").into());
                            return;
                        }
                        let mut store = a.store.borrow().clone();
                        let moved = store
                            .connections
                            .iter()
                            .find(|c| c.id == cfg.id)
                            .is_some_and(|c| c.group != cfg.group);
                        if let Some(c) = store.connections.iter_mut().find(|c| c.id == cfg.id) {
                            *c = cfg.clone();
                        } else {
                            store.connections.push(cfg.clone());
                        }
                        if moved {
                            store.move_connection(
                                &cfg.id,
                                &config::ConnectionDrop::IntoGroup(cfg.group.clone()),
                            );
                        }
                        if !a.save_store(store) {
                            ui.set_form_notice(
                                "Could not save the connection. See the error below.".into(),
                            );
                            return;
                        }
                        ui.set_connection_dialog(false);
                        ui.set_form(ConnectionForm::default());
                        if action == "connect" {
                            a.connect(cfg, password, ssh_secret, false);
                        }
                    }
                    Err(e) => ui.set_form_notice(format!("{e:#}").into()),
                }
            },
        );
    }
    fn reload_objects(self: &App, pattern: Option<String>) {
        let Some(i) = self.active_index() else {
            return;
        };
        let ws = self.workspaces.borrow();
        let w = &ws[i];
        if w.busy() {
            return;
        }
        let (session, driver) = (w.config.id.clone(), w.driver.clone());
        drop(ws);
        self.set_busy(&session, None, true);
        let a = self.clone();
        runtime::spawn(
            async move {
                let objects = driver.list_objects(pattern.as_deref()).await?;
                let columns = driver.list_columns().await.unwrap_or_default();
                Ok::<_, anyhow::Error>((objects, columns))
            },
            move |result| {
                let mut ws = a.workspaces.borrow_mut();
                if let Some(w) = ws.iter_mut().find(|w| w.config.id == session) {
                    w.busy = false;
                    match result {
                        Ok((objects, columns)) => {
                            w.schema = SchemaCache {
                                tables: objects
                                    .objects
                                    .iter()
                                    .filter(|o| !o.kind.is_routine())
                                    .map(|o| o.name.clone())
                                    .collect(),
                                columns,
                            };
                            w.objects = objects.objects;
                            w.truncated = objects.truncated;
                            if let Some(ui) = a.ui.upgrade() {
                                ui.set_object_search("".into());
                            }
                        }
                        Err(e) => a.error(e),
                    }
                }
                drop(ws);
                a.render();
            },
        );
    }
    fn switch_database(self: &App, database: String) {
        let Some(i) = self.active_index() else {
            return;
        };
        let ws = self.workspaces.borrow();
        let w = &ws[i];
        if w.config.database == database || w.config.kind == DbKind::Sqlite {
            return;
        }
        if w.busy() {
            drop(ws);
            self.notice("Wait before switching databases.", true);
            return;
        }
        let pending = w.pending();
        let mut cfg = w.config.clone();
        cfg.database = database;
        let password = w.password.clone();
        let ssh_secret = w.ssh_secret.clone();
        drop(ws);
        let reconnect = move |a: &App| {
            let session = cfg.id.clone();
            a.set_busy(&session, None, true);
            let a = a.clone();
            runtime::spawn(
                async move {
                    let log = db::log::QueryLog::new();
                    let driver = db::logged(
                        db::connect(&cfg, password.clone(), ssh_secret.clone()).await?,
                        log.clone(),
                    );
                    let info = driver.server_info().await?;
                    let databases = driver.list_databases().await?;
                    let objects = driver.list_objects(None).await?;
                    let columns = driver.list_columns().await.unwrap_or_default();
                    let pins = config::pinned_objects(&cfg.id, &cfg.database);
                    let sections = config::sidebar_sections(&cfg.id);
                    let schema = SchemaCache {
                        tables: objects
                            .objects
                            .iter()
                            .filter(|o| !o.kind.is_routine())
                            .map(|o| o.name.clone())
                            .collect(),
                        columns,
                    };
                    Ok::<_, anyhow::Error>(Workspace {
                        config: cfg,
                        driver,
                        log,
                        info,
                        databases,
                        objects: objects.objects,
                        sections,
                        expanded: Default::default(),
                        truncated: objects.truncated,
                        pins,
                        schema,
                        tabs: Vec::new(),
                        active_tab: None,
                        busy: false,
                        schema_filter: String::new(),
                        password,
                        ssh_secret,
                    })
                },
                move |result| {
                    let mut ws = a.workspaces.borrow_mut();
                    if let Some(w) = ws.iter_mut().find(|w| w.config.id == session) {
                        w.busy = false;
                        match result {
                            Ok(new) => *w = new,
                            Err(e) => a.error(e),
                        }
                    }
                    drop(ws);
                    a.render();
                },
            );
        };
        if pending > 0 {
            self.confirm(
                "Switch databases?",
                "The current tabs will close and uncommitted edits will be discarded.",
                "Discard and switch",
                reconnect,
            );
        } else {
            reconnect(self);
        }
    }
    fn select_result(&self, index: i32) {
        let Some(i) = self.active_index() else {
            return;
        };
        if let Some(t) = self.workspaces.borrow_mut()[i].active_mut()
            && let TabContent::Query(q) = &mut t.content
            && let Ok(index) = usize::try_from(index)
            && let Some(result) = q.results.get(index)
        {
            q.result = index;
            q.grid = Grid::from_result(result);
        }
        self.render();
    }
    fn refresh_library(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let Some(i) = self.active_index() else {
            return;
        };
        let id = self.workspaces.borrow()[i].config.id.clone();
        let search = ui.get_library_search().to_lowercase();
        let entries = if ui.get_library_mode() == 0 {
            library::load_history(&id, 500)
                .into_iter()
                .map(|h| {
                    (
                        format!(
                            "{} · {} ms · {}",
                            if h.ok { "✓" } else { "!" },
                            h.millis,
                            h.sql.replace('\n', " ")
                        ),
                        h.sql,
                    )
                })
                .collect::<Vec<_>>()
        } else {
            library::load_saved(&id)
                .into_iter()
                .map(|q| (q.name, q.sql))
                .collect()
        };
        let entries = entries
            .into_iter()
            .filter(|(name, sql)| format!("{name} {sql}").to_lowercase().contains(&search))
            .collect::<Vec<_>>();
        ui.set_library_items(strings(entries.iter().map(|(name, _)| name.clone())));
        *self.library_sql.borrow_mut() = entries.into_iter().map(|(_, sql)| sql).collect();
    }
    fn delete_library(self: &App, index: i32) {
        let Some(i) = self.active_index() else {
            return;
        };
        let id = self.workspaces.borrow()[i].config.id.clone();
        let Some(sql) = usize::try_from(index)
            .ok()
            .and_then(|i| self.library_sql.borrow().get(i).cloned())
        else {
            return;
        };
        let label = self
            .ui
            .upgrade()
            .and_then(|ui| {
                use slint::Model;
                ui.get_library_items().row_data(index.max(0) as usize)
            })
            .map(|s| s.to_string());
        let Some(query) = library::load_saved(&id)
            .into_iter()
            .find(|q| q.sql == sql && label.as_deref() == Some(q.name.as_str()))
        else {
            return;
        };
        self.confirm(
            "Delete saved query?",
            &format!("Remove {} from the query library?", query.name),
            "Delete query",
            move |a| {
                if let Err(e) = library::delete_saved(&query.id) {
                    a.error(e);
                }
                a.refresh_library();
            },
        );
    }
    fn load_library(self: &App, index: i32) {
        let Some(sql) = usize::try_from(index)
            .ok()
            .and_then(|i| self.library_sql.borrow().get(i).cloned())
        else {
            return;
        };
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let in_query = self.active_index().is_some_and(|i| {
            matches!(
                self.workspaces.borrow()[i].active().map(|t| &t.content),
                Some(TabContent::Query(_))
            )
        });
        if !in_query {
            self.new_query(sql, "Query");
            return;
        }
        ui.set_sql(sql.into());
        if let Some(i) = self.active_index()
            && let Some(t) = self.workspaces.borrow_mut()[i].active_mut()
            && let TabContent::Query(q) = &mut t.content
        {
            q.sql = ui.get_sql().to_string();
        }
    }
    fn complete(&self) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let Some(i) = self.active_index() else {
            return;
        };
        let ws = self.workspaces.borrow();
        let w = &ws[i];
        let (_, items) = completion::candidates(
            w.config.kind,
            &w.schema,
            ui.get_sql().as_str(),
            ui.get_cursor().max(0) as usize,
        );
        ui.set_completion_open(!items.is_empty());
        ui.set_completions(strings(items));
    }
    fn complete_selected(&self, index: i32) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let Some(i) = self.active_index() else {
            return;
        };
        let ws = self.workspaces.borrow();
        let w = &ws[i];
        let mut text = ui.get_sql().to_string();
        let cursor = (ui.get_cursor().max(0) as usize).min(text.len());
        let (start, items) = completion::candidates(w.config.kind, &w.schema, &text, cursor);
        if let Some(item) = usize::try_from(index).ok().and_then(|i| items.get(i))
            && text.is_char_boundary(cursor)
        {
            text.replace_range(start..cursor, item);
            ui.set_sql(text.into());
        }
        drop(ws);
        ui.set_completion_open(false);
        if let Some(t) = self.workspaces.borrow_mut()[i].active_mut()
            && let TabContent::Query(q) = &mut t.content
        {
            q.sql = ui.get_sql().to_string();
        }
    }
    fn export(self: &App) {
        let Some(ui) = self.ui.upgrade() else {
            return;
        };
        let Some((session, id, driver)) = self.context() else {
            return;
        };
        let Some(i) = self.active_index() else {
            return;
        };
        let ws = self.workspaces.borrow();
        if ws[i].busy() {
            return;
        }
        let Some(tab) = ws[i].active() else {
            return;
        };
        if tab.pending() > 0 {
            self.notice("Commit or discard pending changes before exporting.", true);
            return;
        }
        let kind = ws[i].config.kind;
        let data = match &tab.content {
            TabContent::Table(t) if t.view == 0 => Some((t.object.clone(), t.options.clone())),
            _ => None,
        };
        let grid = tab.grid().clone();
        let table = match &tab.content {
            TabContent::Table(t) => format!("{}.{}", t.object.schema, t.object.name),
            _ => "query_result".into(),
        };
        drop(ws);
        let format = export::Format::from_name(
            ["csv", "json", "sql"][ui.get_export_format().clamp(0, 2) as usize],
        )
        .unwrap();
        let Some(path) = rfd::FileDialog::new()
            .set_title("Export data")
            .set_file_name(format!("{}.{}", table, format.extension()))
            .add_filter(format.extension(), &[format.extension()])
            .save_file()
        else {
            return;
        };
        self.set_busy(&session, Some(id), true);
        let a = self.clone();
        runtime::spawn(
            async move {
                let (columns, rows) = if let Some((object, mut options)) = data {
                    options.limit = 3000;
                    options.offset = 0;
                    let mut rows = Vec::new();
                    let mut columns = Vec::new();
                    loop {
                        let result = driver.fetch_rows(&object, &options).await?;
                        let done = result.rows.len() < options.limit;
                        columns.clone_from(&result.columns);
                        rows.extend(result.rows);
                        if done {
                            break;
                        }
                        options.offset += options.limit;
                    }
                    (columns, rows)
                } else {
                    (grid.columns.clone(), grid.values())
                };
                let count = rows.len();
                tokio::task::spawn_blocking(move || {
                    export::write(&path, format, kind, &table, &columns, &rows)
                })
                .await??;
                Ok::<_, anyhow::Error>(count)
            },
            move |result| {
                a.set_busy(&session, Some(id), false);
                match result {
                    Ok(count) => a.notice(format!("Exported {count} rows."), false),
                    Err(e) => a.error(e),
                }
            },
        );
    }
    fn import(self: &App) {
        let Some((session, id, driver)) = self.context() else {
            return;
        };
        let Some(i) = self.active_index() else {
            return;
        };
        let ws = self.workspaces.borrow();
        if ws[i].busy() {
            return;
        }
        let Some(tab) = ws[i].active() else {
            return;
        };
        let TabContent::Table(t) = &tab.content else {
            return;
        };
        if tab.pending() > 0 {
            self.notice("Commit or discard pending changes before importing.", true);
            return;
        }
        let object = t.object.clone();
        let columns = t.data.columns.clone();
        drop(ws);
        let Some(path) = rfd::FileDialog::new()
            .set_title("Import CSV")
            .add_filter("CSV", &["csv"])
            .pick_file()
        else {
            return;
        };
        self.confirm(
            "Import CSV?",
            &format!(
                "Insert rows from {} into {} in one transaction? Empty fields become NULL.",
                path.display(),
                object.name
            ),
            "Import",
            move |a| {
                a.set_busy(&session, Some(id), true);
                let a = a.clone();
                runtime::spawn(
                    async move {
                        let changes =
                            tokio::task::spawn_blocking(move || export::read_csv(&path, &columns))
                                .await??;
                        driver.apply_changes(&object, &changes).await
                    },
                    move |result| {
                        a.set_busy(&session, Some(id), false);
                        match result {
                            Ok(count) => {
                                a.notice(format!("Imported {count} rows."), false);
                                a.reload_tab(&session, id);
                            }
                            Err(e) => a.error(e),
                        }
                    },
                );
            },
        );
    }
}
fn line_numbers(text: &str) -> String {
    (1..=text.bytes().filter(|b| *b == b'\n').count() + 1)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}
fn row_tsv(row: &[db::Cell]) -> String {
    row.iter()
        .map(|c| {
            let s = c.as_deref().unwrap_or("");
            if s.contains(['\t', '\n', '\r', '"']) {
                format!("\"{}\"", s.replace('"', "\"\""))
            } else {
                s.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\t")
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::platform::{
        EventLoopProxy, Platform, WindowAdapter,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    };
    use std::sync::mpsc;
    type Event = Box<dyn FnOnce() + Send>;
    struct TestPlatform {
        window: Rc<MinimalSoftwareWindow>,
        sender: mpsc::Sender<Event>,
    }
    struct Proxy(mpsc::Sender<Event>);
    impl EventLoopProxy for Proxy {
        fn quit_event_loop(&self) -> std::result::Result<(), slint::EventLoopError> {
            Ok(())
        }
        fn invoke_from_event_loop(
            &self,
            event: Event,
        ) -> std::result::Result<(), slint::EventLoopError> {
            self.0
                .send(event)
                .map_err(|_| slint::EventLoopError::EventLoopTerminated)
        }
    }
    impl Platform for TestPlatform {
        fn create_window_adapter(
            &self,
        ) -> std::result::Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            Ok(self.window.clone())
        }
        fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
            Some(Box::new(Proxy(self.sender.clone())))
        }
    }
    fn pump(receiver: &mpsc::Receiver<Event>, ready: impl Fn() -> bool) {
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        loop {
            while let Ok(event) = receiver.try_recv() {
                event();
            }
            slint::platform::update_timers_and_animations();
            if ready() {
                return;
            }
            assert!(Instant::now() < deadline, "UI operation timed out");
            if let Ok(event) = receiver.recv_timeout(std::time::Duration::from_millis(5)) {
                event();
            }
        }
    }
    #[test]
    fn slint_sqlite_workflow() {
        use slint::Model;
        let (sender, receiver) = mpsc::channel();
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(TestPlatform {
            window: window.clone(),
            sender,
        }))
        .unwrap();
        let ui = AppWindow::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        crate::platform::set_test_base(dir.path().into());
        let app = Rc::new(Controller {
            ui: ui.as_weak(),
            store: RefCell::new(Store::default()),
            workspaces: RefCell::new(Vec::new()),
            active: RefCell::new(None),
            next_id: std::cell::Cell::new(0),
            confirmation: RefCell::new(None),
            library_sql: RefCell::new(Vec::new()),
            connecting: RefCell::new(Default::default()),
            auth_retry: RefCell::new(None),
        import_batch: RefCell::new(Vec::new()),
        pending_export: RefCell::new(None),
        filter_rules: Rc::new(VecModel::default()),
        saved_rules: RefCell::new(Default::default()),
        rules_tab: std::cell::Cell::new(None),
        waiter: RefCell::new(None),
        cell_choices: RefCell::new(Vec::new()),
        });
        let cfg = ConnectionConfig {
            file_path: ":memory:".into(),
            ..ConnectionConfig::new(DbKind::Sqlite)
        };
        app.connect(cfg, None, None, false);
        pump(&receiver, || app.connecting.borrow().is_empty());
        assert_eq!(app.workspaces.borrow().len(), 1, "{}", ui.get_notice());
        app.new_query("CREATE TABLE notes(id INTEGER PRIMARY KEY, value TEXT NOT NULL); INSERT INTO notes VALUES (1,'first'); SELECT * FROM notes;".into(),"Setup");
        app.run_query(true);
        pump(&receiver, || !ui.get_busy());
        assert_eq!(ui.get_results().row_count(), 3, "{}", ui.get_notice());
        app.select_result(2);
        assert_eq!(ui.get_rows().row_count(), 1);
        assert!(!ui.get_editable());
        app.reload_objects(None);
        pump(&receiver, || !ui.get_busy());
        assert!(!app.workspaces.borrow()[0].objects.is_empty());
        app.object_action(0, "open");
        pump(&receiver, || !ui.get_busy());
        assert!(ui.get_editable());
        app.cell_action(0, 1, true);
        ui.set_cell_value("changed".into());
        app.command("cell-apply");
        assert_eq!(ui.get_pending(), 1);
        app.command("refresh");
        assert!(ui.get_confirm_dialog());
        assert_eq!(ui.get_pending(), 1);
        ui.set_confirm_dialog(false);
        app.confirmation.borrow_mut().take();
        app.command("view:1");
        pump(&receiver, || !ui.get_busy());
        assert_eq!(ui.get_pending(), 1);
        app.command("view:0");
        assert_eq!(ui.get_pending(), 1);
        app.commit();
        pump(&receiver, || !ui.get_busy());
        assert_eq!(ui.get_pending(), 0);
        assert_eq!(
            ui.get_rows()
                .row_data(0)
                .unwrap()
                .cells
                .row_data(1)
                .unwrap()
                .text,
            "changed"
        );
        app.command("add-row");
        app.field_edit(1, "second", false);
        assert_eq!(ui.get_pending(), 1);
        app.commit();
        pump(&receiver, || !ui.get_busy());
        assert_eq!(ui.get_rows().row_count(), 2);
        app.command("view:1");
        pump(&receiver, || !ui.get_busy());
        assert!(!ui.get_editable());
        assert!(ui.get_rows().row_count() > 0);
        app.command("view:0");
        pump(&receiver, || !ui.get_busy());
        ui.set_filter("id = 2".into());
        app.command("filter");
        pump(&receiver, || !ui.get_busy());
        assert_eq!(ui.get_rows().row_count(), 1);
        app.new_query("SELECT 1; SELECT 'café';".into(), "Query");
        ui.set_cursor(0);
        ui.set_anchor(0);
        app.run_query(false);
        pump(&receiver, || !ui.get_busy());
        assert_eq!(ui.get_results().row_count(), 1);
        app.run_query(true);
        pump(&receiver, || !ui.get_busy());
        assert_eq!(ui.get_results().row_count(), 2);
        ui.set_console_open(true);
        app.refresh_console();
        assert!(ui.get_console_entries().iter().any(|e| e.sql.contains("SELECT")));
        app.refresh_library();
        assert!(ui.get_library_items().row_count() > 0);
        ui.set_cursor(0);
        ui.set_anchor(0);
        app.tab_action(1, "switch");
        ui.set_filter("".into());
        app.command("filter");
        pump(&receiver, || !ui.get_busy());
        app.cell_action(0, 1, false);
        ui.set_inspector_open(true);
        ui.set_console_open(false);
        app.command("dismiss");
        window.set_size(slint::PhysicalSize::new(1280, 820));
        ui.show().unwrap();
        let mut pixels = vec![slint::Rgb8Pixel::default(); 1280 * 820];
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 1280);
        });
        assert!(
            pixels.iter().any(|p| p.r != 0 || p.g != 0 || p.b != 0),
            "Slint window rendered no pixels"
        );
        if let Ok(path) = std::env::var("PUEO_SNAPSHOT") {
            let mut encoder = png::Encoder::new(std::fs::File::create(path).unwrap(), 1280, 820);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(
                    &pixels
                        .iter()
                        .flat_map(|p| [p.r, p.g, p.b])
                        .collect::<Vec<_>>(),
                )
                .unwrap();
        }
        ui.hide().unwrap();
    }
    #[test]
    #[ignore = "requires PUEO_LOCAL_CONFIG pointing to an explicitly local PostgreSQL connection"]
    fn slint_local_postgres_workflow() {
        use slint::Model;
        let file = std::env::var("PUEO_LOCAL_CONFIG").expect("set PUEO_LOCAL_CONFIG");
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
        let cfg: ConnectionConfig = serde_json::from_value(value.clone()).unwrap();
        assert!(
            matches!(cfg.host.as_str(), "localhost" | "127.0.0.1" | "::1"),
            "test refuses non-local hosts"
        );
        assert!(!cfg.ssh.enabled, "test refuses SSH connections");
        assert_eq!(cfg.kind, DbKind::Postgres);
        let password = value
            .get("password")
            .and_then(|p| p.as_str())
            .map(str::to_owned);
        let (sender, receiver) = mpsc::channel();
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(TestPlatform { window, sender })).unwrap();
        let ui = AppWindow::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        crate::platform::set_test_base(dir.path().into());
        let app = Rc::new(Controller {
            ui: ui.as_weak(),
            store: RefCell::new(Store::default()),
            workspaces: RefCell::new(Vec::new()),
            active: RefCell::new(None),
            next_id: std::cell::Cell::new(0),
            confirmation: RefCell::new(None),
            library_sql: RefCell::new(Vec::new()),
            connecting: RefCell::new(Default::default()),
            auth_retry: RefCell::new(None),
        import_batch: RefCell::new(Vec::new()),
        pending_export: RefCell::new(None),
        filter_rules: Rc::new(VecModel::default()),
        saved_rules: RefCell::new(Default::default()),
        rules_tab: std::cell::Cell::new(None),
        waiter: RefCell::new(None),
        cell_choices: RefCell::new(Vec::new()),
        });
        app.connect(cfg, password, None, false);
        pump(&receiver, || app.connecting.borrow().is_empty());
        assert_eq!(app.workspaces.borrow().len(), 1, "{}", ui.get_notice());
        println!("Connected to localhost PostgreSQL using local TablePlus settings");
        let name = format!("pueo_slint_{}", uuid::Uuid::new_v4().simple());
        app.new_query(format!("CREATE TEMP TABLE {name}(id BIGSERIAL PRIMARY KEY, value TEXT NOT NULL UNIQUE, active BOOLEAN DEFAULT TRUE); INSERT INTO {name}(value) SELECT 'row ' || i FROM generate_series(1,650) i; SELECT count(*) FROM {name};"),"Isolated setup");
        app.run_query(true);
        pump(&receiver, || !ui.get_busy());
        assert_eq!(ui.get_results().row_count(), 3, "{}", ui.get_notice());
        app.reload_objects(None);
        pump(&receiver, || !ui.get_busy());
        let object_index = app.workspaces.borrow()[0]
            .objects
            .iter()
            .position(|o| o.name == name)
            .expect("temporary table listed");
        app.object_action(object_index as i32, "open");
        pump(&receiver, || !ui.get_busy());
        assert_eq!(ui.get_rows().row_count(), 300);
        assert!(ui.get_editable());
        assert!(ui.get_can_next());
        app.command("sort:0");
        pump(&receiver, || !ui.get_busy());
        app.command("next");
        pump(&receiver, || !ui.get_busy());
        assert!(ui.get_can_prev());
        app.command("next");
        pump(&receiver, || !ui.get_busy());
        assert_eq!(ui.get_rows().row_count(), 50);
        assert!(!ui.get_can_next());
        app.command("prev");
        pump(&receiver, || !ui.get_busy());
        app.cell_action(0, 1, true);
        ui.set_cell_value("changed via Slint".into());
        app.command("cell-apply");
        app.commit();
        pump(&receiver, || !ui.get_busy());
        assert_eq!(ui.get_pending(), 0);
        assert_eq!(
            ui.get_rows()
                .row_data(0)
                .unwrap()
                .cells
                .row_data(1)
                .unwrap()
                .text,
            "changed via Slint"
        );
        app.command("view:1");
        pump(&receiver, || !ui.get_busy());
        assert_eq!(ui.get_rows().row_count(), 3);
        assert!(!ui.get_editable());
        app.command("view:2");
        pump(&receiver, || !ui.get_busy());
        assert!(ui.get_rows().row_count() >= 2);
        app.command("view:0");
        pump(&receiver, || !ui.get_busy());
        ui.set_filter("value = 'changed via Slint'".into());
        app.command("filter");
        pump(&receiver, || !ui.get_busy());
        assert_eq!(ui.get_rows().row_count(), 1);
        app.command("add-row");
        app.field_edit(1, "inserted via Slint", false);
        app.field_edit(2, "true", false);
        app.commit();
        pump(&receiver, || !ui.get_busy());
        assert_eq!(ui.get_pending(), 0);
        ui.set_filter("".into());
        app.command("filter");
        pump(&receiver, || !ui.get_busy());
        app.cell_action(0, 1, false);
        app.command("delete-row");
        app.commit();
        pump(&receiver, || !ui.get_busy());
        assert_eq!(ui.get_pending(), 0);
        app.new_query("SELECT pg_sleep(20);".into(), "Cancellation");
        app.run_query(true);
        let start = Instant::now();
        pump(&receiver, || {
            start.elapsed() > std::time::Duration::from_millis(200)
        });
        app.command("stop");
        pump(&receiver, || !ui.get_busy());
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        app.new_query(format!("SELECT count(*) FROM {name};"), "Verify");
        app.run_query(true);
        pump(&receiver, || !ui.get_busy());
        assert_eq!(
            ui.get_rows()
                .row_data(0)
                .unwrap()
                .cells
                .row_data(0)
                .unwrap()
                .text,
            "650"
        );
        app.close_connection();
        assert!(app.workspaces.borrow().is_empty());
        println!(
            "Verified browsing, paging, edits, commit, insert, delete, structure, indexes, filter, multi-result queries and cancellation; temporary table removed on disconnect"
        );
    }
}
