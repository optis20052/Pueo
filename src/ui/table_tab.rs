use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use adw::prelude::*;
use gtk::{gio, glib};

use super::data_grid::{DataGrid, GridRow};
use super::filter_bar::FilterBar;
use super::row_inspector::RowInspector;
use crate::db::{self, DbObject, FetchOptions, ForeignKey, ObjectKind, RowChange, SharedDriver};
use crate::export::{self, Format};
use crate::runtime;

const PAGE_SIZE: usize = 300;
const EXPORT_BATCH: usize = 5000;

/// A commit error worth reading: the database's message plus a hint for bad values.
fn describe_commit_error(e: &anyhow::Error) -> String {
    let msg = format!("{e:#}");
    let msg = msg.trim_start_matches("db error: ").trim_start_matches("ERROR: ");
    let lower = msg.to_lowercase();
    let bad_value = ["invalid input syntax", "invalid input value", "incorrect ", "out of range", "malformed"]
        .iter()
        .any(|p| lower.contains(p));
    let mut text = format!("Nothing was saved: {msg}");
    if bad_value {
        text.push_str(" Tip: to clear a value, right-click the cell and choose Set NULL.");
    }
    text
}

thread_local! {
    /// Whether the row inspector is open; shared so new tabs match the last choice.
    static INSPECTOR_OPEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Turn edited grid rows into database changes. `pk` names the key columns.
pub fn build_changes(columns: &[String], pk: &[String], rows: Vec<GridRow>) -> Vec<RowChange> {
    let pairs = |vals: &[db::Cell]| columns.iter().cloned().zip(vals.iter().cloned()).collect::<Vec<_>>();
    let key_of = |vals: &[db::Cell]| {
        pk.iter()
            .filter_map(|k| columns.iter().position(|c| c == k).map(|i| (k.clone(), vals[i].clone())))
            .collect::<Vec<_>>()
    };
    let mut changes = Vec::new();
    for r in rows {
        match (&r.original, r.deleted) {
            (None, true) => {}
            (None, false) => changes.push(RowChange::Insert {
                values: pairs(&r.current).into_iter().filter(|(_, v)| v.is_some()).collect(),
            }),
            (Some(orig), true) => changes.push(RowChange::Delete { key: key_of(orig) }),
            (Some(orig), false) => {
                let set: Vec<_> = columns
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| orig[*i] != r.current[*i])
                    .map(|(i, c)| (c.clone(), r.current[i].clone()))
                    .collect();
                if !set.is_empty() {
                    changes.push(RowChange::Update { key: key_of(orig), set, row: pairs(&r.current) });
                }
            }
        }
    }
    // Deletes first so that re-inserting a deleted key in the same commit works.
    changes.sort_by_key(|c| match c {
        RowChange::Delete { .. } => 0,
        RowChange::Update { .. } => 1,
        RowChange::Insert { .. } => 2,
    });
    changes
}

pub struct TableTab {
    pub widget: gtk::Box,
    pub object: DbObject,
    driver: SharedDriver,
    grid: Rc<DataGrid>,
    inspector: Rc<RowInspector>,
    inspector_button: gtk::ToggleButton,
    structure: Rc<DataGrid>,
    indexes: Rc<DataGrid>,
    data_button: gtk::ToggleButton,
    structure_button: gtk::ToggleButton,
    stack: gtk::Stack,
    filter: Rc<FilterBar>,
    status: gtk::Label,
    page_label: gtk::Label,
    prev: gtk::Button,
    next: gtk::Button,
    spinner: adw::Spinner,
    add_button: gtk::Button,
    delete_button: gtk::Button,
    lock: gtk::Image,
    pending: gtk::Revealer,
    pending_label: gtk::Label,
    commit_error: gtk::Label,
    /// Shown in the status line after the next load instead of the timing.
    notice: RefCell<Option<String>>,
    commit_button: gtk::Button,
    actions: gio::SimpleActionGroup,
    offset: Cell<usize>,
    total: Cell<Option<u64>>,
    order: RefCell<Option<(String, bool)>>,
    pk: RefCell<Option<Vec<String>>>,
    foreign_keys: RefCell<Vec<ForeignKey>>,
    /// Opens a table filtered to one value: (table, column, value).
    on_open_reference: RefCell<Option<Box<dyn Fn(DbObject, &str, &str)>>>,
    structure_loaded: Cell<bool>,
    indexes_loaded: Cell<bool>,
    /// Incremented on every load so stale responses are ignored.
    generation: Cell<u64>,
}

impl TableTab {
    /// `initial_filter`: start filtered to rows where (column) = (value).
    pub fn new(driver: SharedDriver, object: DbObject, initial_filter: Option<(&str, &str)>) -> Rc<Self> {
        let is_key = object.kind == ObjectKind::Key;

        // Filter bar (SQL only): visual rules or raw WHERE.
        let filter = FilterBar::new(driver.kind());
        filter.widget.set_visible(!is_key);

        let grid = DataGrid::new(!is_key);
        let structure = DataGrid::new(false);
        let indexes = DataGrid::new(false);
        let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();
        stack.add_named(&grid.widget, Some("data"));
        stack.add_named(&structure.widget, Some("structure"));
        stack.add_named(&indexes.widget, Some("indexes"));

        // Pending changes bar.
        let pending_label = gtk::Label::builder().xalign(0.0).hexpand(true).build();
        // Why the last commit failed; shown right where the changes are.
        let commit_error = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .selectable(true)
            .visible(false)
            .css_classes(["error", "caption"])
            .build();
        let pending_text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        pending_text.append(&pending_label);
        pending_text.append(&commit_error);
        // Buttons keep their size when the error message wraps onto more lines.
        let discard = gtk::Button::builder()
            .label("Discard")
            .action_name("tab.discard")
            .valign(gtk::Align::Center)
            .build();
        let commit_button = gtk::Button::builder()
            .label("Commit")
            .tooltip_text("Save changes to the database (Ctrl+S)")
            .action_name("tab.commit")
            .css_classes(["suggested-action"])
            .valign(gtk::Align::Center)
            .build();
        let pending_box = gtk::Box::builder().spacing(8).css_classes(["pending-bar"]).build();
        pending_box.append(&gtk::Image::builder().icon_name("document-edit-symbolic").valign(gtk::Align::Center).build());
        pending_box.append(&pending_text);
        pending_box.append(&discard);
        pending_box.append(&commit_button);
        let pending = gtk::Revealer::builder()
            .child(&pending_box)
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .build();

        // Footer.
        let data_btn = gtk::ToggleButton::builder().label("Data").active(true).build();
        let structure_btn = gtk::ToggleButton::builder()
            .label(if is_key { "Info" } else { "Structure" })
            .group(&data_btn)
            .build();
        // Redis keys have no indexes.
        let indexes_btn = gtk::ToggleButton::builder().label("Indexes").group(&data_btn).visible(!is_key).build();
        let toggle = gtk::Box::builder().css_classes(["linked"]).build();
        toggle.append(&data_btn);
        toggle.append(&structure_btn);
        toggle.append(&indexes_btn);

        let add_button = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add Row (Ctrl+I)")
            .action_name("tab.add-row")
            .css_classes(["flat"])
            .build();
        let delete_button = gtk::Button::builder()
            .icon_name("list-remove-symbolic")
            .tooltip_text("Delete Selected Rows (Delete)")
            .action_name("tab.delete-rows")
            .css_classes(["flat"])
            .build();
        let lock = gtk::Image::builder()
            .icon_name("changes-prevent-symbolic")
            .css_classes(["dim-label"])
            .visible(false)
            .build();

        let status = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        let spinner = adw::Spinner::builder().visible(false).build();

        let menu = gio::Menu::new();
        let export_section = gio::Menu::new();
        export_section.append(Some("Export as CSV…"), Some("tab.export::csv"));
        export_section.append(Some("Export as JSON…"), Some("tab.export::json"));
        export_section.append(Some("Export as SQL…"), Some("tab.export::sql"));
        menu.append_section(Some("Export (all rows matching the filter)"), &export_section);
        if !is_key {
            let import_section = gio::Menu::new();
            import_section.append(Some("Import CSV…"), Some("tab.import"));
            menu.append_section(None, &import_section);
        }
        let menu_button = gtk::MenuButton::builder()
            .icon_name("document-send-symbolic")
            .tooltip_text("Export / Import")
            .menu_model(&menu)
            .css_classes(["flat"])
            .build();

        let prev = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("Previous page")
            .css_classes(["flat"])
            .build();
        let next = gtk::Button::builder()
            .icon_name("go-next-symbolic")
            .tooltip_text("Next page")
            .css_classes(["flat"])
            .build();
        let page_label = gtk::Label::builder().css_classes(["caption", "numeric"]).build();
        let refresh = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Reload (Ctrl+R)")
            .css_classes(["flat"])
            .build();

        let footer = gtk::Box::builder().spacing(6).css_classes(["statusbar"]).build();
        footer.append(&toggle);
        // Filter bar toggle; highlighted while a filter is applied (even when hidden).
        let filter_button = gtk::ToggleButton::builder()
            .icon_name("edit-find-symbolic")
            .tooltip_text("Filter Rows (Ctrl+F)")
            .visible(!is_key)
            .css_classes(["flat"])
            .build();
        filter
            .widget
            .bind_property("reveal-child", &filter_button, "active")
            .bidirectional()
            .sync_create()
            .build();
        footer.append(&filter_button);
        footer.append(&add_button);
        footer.append(&delete_button);
        footer.append(&lock);
        footer.append(&spinner);
        footer.append(&status);
        footer.append(&menu_button);
        footer.append(&prev);
        footer.append(&page_label);
        footer.append(&next);
        // Row inspector toggle (right-hand form with all columns of the selected row).
        let inspector_button = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-right-symbolic")
            .tooltip_text("Row Inspector (Ctrl+Shift+I)")
            .active(INSPECTOR_OPEN.get())
            .css_classes(["flat"])
            .build();
        footer.append(&inspector_button);
        footer.append(&refresh);

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&filter.widget);
        if !is_key {
            widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        }
        let inspector = RowInspector::new(&grid);
        inspector.widget.set_visible(INSPECTOR_OPEN.get());
        let split = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&stack)
            .end_child(&inspector.widget)
            .resize_start_child(true)
            .resize_end_child(false)
            .shrink_end_child(false)
            .vexpand(true)
            .build();
        inspector_button
            .bind_property("active", &inspector.widget, "visible")
            .sync_create()
            .build();
        // New tables open with the inspector as it was last left.
        inspector_button.connect_toggled(|b| INSPECTOR_OPEN.set(b.is_active()));
        widget.append(&split);
        // Place the divider so the inspector starts about 340 px wide.
        let s = split.downgrade();
        split.connect_realize(move |_| {
            if let Some(s) = s.upgrade() {
                let s2 = s.clone();
                glib::idle_add_local_once(move || {
                    let width = s2.width();
                    if width > 700 {
                        s2.set_position(width - 340);
                    }
                });
            }
        });
        widget.append(&pending);
        widget.append(&footer);

        let tab = Rc::new(TableTab {
            widget,
            object,
            driver,
            grid,
            inspector,
            inspector_button,
            structure,
            indexes,
            data_button: data_btn.clone(),
            structure_button: structure_btn.clone(),
            stack,
            filter,
            status,
            page_label,
            prev,
            next,
            spinner,
            add_button,
            delete_button,
            lock,
            pending,
            pending_label,
            commit_error,
            notice: RefCell::new(None),
            commit_button,
            actions: gio::SimpleActionGroup::new(),
            offset: Cell::new(0),
            total: Cell::new(None),
            order: RefCell::new(None),
            pk: RefCell::new(None),
            foreign_keys: RefCell::new(Vec::new()),
            on_open_reference: RefCell::new(None),
            structure_loaded: Cell::new(false),
            indexes_loaded: Cell::new(false),
            generation: Cell::new(0),
        });
        tab.setup_actions();
        if let Some((column, value)) = initial_filter {
            tab.filter.preset(column, value);
            filter_button.add_css_class("filter-on");
        }

        let weak = Rc::downgrade(&tab);
        let apply_filter = move || {
            if let Some(t) = weak.upgrade() {
                t.guard(|t| {
                    t.offset.set(0);
                    t.load();
                });
            }
        };
        let weak_filter = Rc::downgrade(&tab.filter);
        tab.filter.connect_apply(move || {
            if let Some(f) = weak_filter.upgrade() {
                if f.is_active() {
                    filter_button.add_css_class("filter-on");
                } else {
                    filter_button.remove_css_class("filter-on");
                }
            }
            apply_filter();
        });

        let weak = Rc::downgrade(&tab);
        tab.prev.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.guard(|t| {
                    t.offset.set(t.offset.get().saturating_sub(PAGE_SIZE));
                    t.load();
                });
            }
        });
        let weak = Rc::downgrade(&tab);
        tab.next.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.guard(|t| {
                    t.offset.set(t.offset.get() + PAGE_SIZE);
                    t.load();
                });
            }
        });
        let weak = Rc::downgrade(&tab);
        refresh.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.reload();
            }
        });
        let weak = Rc::downgrade(&tab);
        tab.grid.connect_sort_changed(move |order| {
            if let Some(t) = weak.upgrade() {
                t.guard(move |t| {
                    *t.order.borrow_mut() = order;
                    t.offset.set(0);
                    t.load();
                });
            }
        });
        let weak = Rc::downgrade(&tab);
        tab.grid.connect_changed(move || {
            if let Some(t) = weak.upgrade() {
                t.update_pending();
                t.inspector.refresh();
            }
        });
        let weak = Rc::downgrade(&tab);
        tab.grid.connect_follow(move |column, value| {
            let Some(t) = weak.upgrade() else { return };
            let fk = t.foreign_keys.borrow().iter().find(|fk| fk.column == column).cloned();
            if let (Some(fk), Some(open)) = (fk, t.on_open_reference.borrow().as_ref()) {
                open(fk.target, &fk.target_column, value);
            }
        });
        let weak = Rc::downgrade(&tab);
        tab.grid.connect_notice(move |msg, error| {
            let Some(t) = weak.upgrade() else { return };
            if error {
                t.show_error(&anyhow::anyhow!("{msg}"));
            } else {
                t.flash(msg);
            }
        });
        let weak = Rc::downgrade(&tab);
        tab.grid.connect_selection_changed(move || {
            if let Some(t) = weak.upgrade() {
                t.inspector.show_selection();
            }
        });
        let weak = Rc::downgrade(&tab);
        data_btn.connect_toggled(move |b| {
            if let (Some(t), true) = (weak.upgrade(), b.is_active()) {
                t.show_view("data");
            }
        });
        let weak = Rc::downgrade(&tab);
        structure_btn.connect_toggled(move |b| {
            if let (Some(t), true) = (weak.upgrade(), b.is_active()) {
                t.show_view("structure");
            }
        });
        let weak = Rc::downgrade(&tab);
        indexes_btn.connect_toggled(move |b| {
            if let (Some(t), true) = (weak.upgrade(), b.is_active()) {
                t.show_view("indexes");
            }
        });

        tab.update_edit_state();
        // The first page waits for the key (for stable ordering) and the dropdown choices.
        tab.load_primary_key();
        tab
    }

    fn setup_actions(self: &Rc<Self>) {
        let simple = |name: &str, f: fn(&Rc<Self>)| {
            let action = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(t) = weak.upgrade() {
                    f(&t);
                }
            });
            self.actions.add_action(&action);
        };
        simple("commit", |t| t.commit());
        simple("discard", |t| t.grid.discard_changes());
        simple("add-row", |t| t.add_row());
        simple("delete-rows", |t| t.grid.delete_selected());
        simple("import", |t| t.choose_import());

        let export = gio::SimpleAction::new("export", Some(glib::VariantTy::STRING));
        let weak = Rc::downgrade(self);
        export.connect_activate(move |_, param| {
            let (Some(t), Some(fmt)) = (weak.upgrade(), param.and_then(|p| p.str().and_then(Format::from_name)))
            else {
                return;
            };
            t.choose_export(fmt);
        });
        self.actions.add_action(&export);
        self.widget.insert_action_group("tab", Some(&self.actions));
    }

    fn set_action_enabled(&self, name: &str, enabled: bool) {
        if let Some(a) = self.actions.lookup_action(name).and_downcast::<gio::SimpleAction>() {
            a.set_enabled(enabled);
        }
    }

    fn editable(&self) -> bool {
        self.pk.borrow().is_some()
    }

    fn update_edit_state(&self) {
        let on_data = self.stack.visible_child_name().as_deref() == Some("data");
        let editable = self.editable();
        self.grid.set_editable(editable);
        for name in ["add-row", "delete-rows"] {
            self.set_action_enabled(name, editable && on_data);
        }
        self.add_button.set_visible(editable);
        self.delete_button.set_visible(editable);
        self.set_action_enabled("import", self.object.kind == ObjectKind::Table);
        self.lock.set_visible(!editable);
        self.lock.set_tooltip_text(Some(match self.object.kind {
            // Routines open as source in a query tab, never as a table.
            ObjectKind::View | ObjectKind::Function | ObjectKind::Procedure => "Read-only: views cannot be edited",
            ObjectKind::Key => "Read-only: this Redis type cannot be edited",
            ObjectKind::Table => "Read-only: the table has no primary key",
        }));
        self.update_pending();
    }

    /// The chosen sort, or the primary key so rows keep their place after edits
    /// (without ORDER BY, PostgreSQL moves updated rows to the end).
    fn effective_order(&self) -> Option<(String, bool)> {
        self.order.borrow().clone().or_else(|| {
            let pk = self.pk.borrow();
            pk.as_ref().filter(|_| self.object.kind != ObjectKind::Key).and_then(|p| p.first()).map(|c| (c.clone(), true))
        })
    }

    /// A short success message in the status line (replaced by the next load).
    fn flash(&self, msg: &str) {
        self.status.remove_css_class("error");
        self.status.set_tooltip_text(None);
        self.status.set_text(msg);
    }

    fn update_pending(&self) {
        // Any new edit (or discarding) makes the last commit error stale.
        self.commit_error.set_visible(false);
        let n = self.grid.pending_count();
        self.pending.set_reveal_child(n > 0);
        self.pending_label
            .set_text(&format!("{n} pending {}", if n == 1 { "change" } else { "changes" }));
        self.set_action_enabled("commit", n > 0);
        self.set_action_enabled("discard", n > 0);
    }

    /// Called when a foreign-key value is double-clicked, to open the row it points at.
    pub fn connect_open_reference(&self, f: impl Fn(DbObject, &str, &str) + 'static) {
        *self.on_open_reference.borrow_mut() = Some(Box::new(f));
    }

    pub fn has_changes(&self) -> bool {
        self.grid.has_changes()
    }

    /// Run `then` now, or after the user agrees to discard pending changes.
    pub fn guard(self: &Rc<Self>, then: impl FnOnce(&Rc<Self>) + 'static) {
        let n = self.grid.pending_count();
        if n == 0 {
            then(self);
            return;
        }
        let dialog = adw::AlertDialog::new(
            Some("Discard Changes?"),
            Some(&format!(
                "{n} pending {} to “{}” will be lost.",
                if n == 1 { "change" } else { "changes" },
                self.object.name
            )),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("discard", "Discard")]);
        dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        let weak = Rc::downgrade(self);
        let then = RefCell::new(Some(then));
        dialog.connect_response(None, move |_, response| {
            if response != "discard" {
                return;
            }
            if let (Some(t), Some(then)) = (weak.upgrade(), then.borrow_mut().take()) {
                t.grid.discard_changes();
                then(&t);
            }
        });
        dialog.present(Some(&self.widget));
    }

    pub fn reload(self: &Rc<Self>) {
        self.guard(|t| {
            t.structure_loaded.set(false);
            t.indexes_loaded.set(false);
            match t.stack.visible_child_name().as_deref() {
                Some("structure") => t.load_structure(),
                Some("indexes") => t.load_indexes(),
                _ => {}
            }
            t.load();
        });
    }

    pub fn focus_filter(&self) {
        if self.filter.widget.is_visible() {
            self.filter.show();
        }
    }

    pub fn toggle_inspector(&self) {
        self.inspector_button.set_active(!self.inspector_button.is_active());
    }

    /// Switch to the Structure view.
    pub fn show_structure(&self) {
        self.structure_button.set_active(true);
    }

    /// Paste copied rows as new rows (Ctrl+V while focus is outside the grid).
    pub fn paste_rows(self: &Rc<Self>) {
        self.data_button.set_active(true);
        self.grid.paste_rows();
    }

    pub fn add_row(self: &Rc<Self>) {
        if self.editable() {
            self.data_button.set_active(true);
            self.grid.add_row();
        }
    }

    fn set_busy(&self, busy: bool) {
        self.spinner.set_visible(busy);
        self.prev.set_sensitive(!busy && self.offset.get() > 0);
        self.next.set_sensitive(!busy && self.has_next_page());
        self.commit_button.set_sensitive(!busy);
    }

    fn has_next_page(&self) -> bool {
        match self.total.get() {
            Some(total) => (self.offset.get() + PAGE_SIZE) < total as usize,
            None => false,
        }
    }

    fn show_error(&self, e: &anyhow::Error) {
        self.status.add_css_class("error");
        self.status.set_text(&format!("{e:#}"));
        self.status.set_tooltip_text(Some(&format!("{e:#}")));
    }

    fn filter_text(&self) -> Option<String> {
        self.filter.where_clause()
    }

    fn load_primary_key(self: &Rc<Self>) {
        let driver = self.driver.clone();
        let obj = self.object.clone();
        let weak = Rc::downgrade(self);
        runtime::spawn(
            async move { (driver.primary_key(&obj).await, driver.column_choices(&obj).await) },
            move |(pk, choices)| {
                let Some(t) = weak.upgrade() else { return };
                t.grid.set_choices(choices.unwrap_or_default());
                *t.pk.borrow_mut() = pk.ok().flatten();
                t.grid.set_key_names(t.pk.borrow().clone().unwrap_or_default());
                t.update_edit_state();
                t.load();
            },
        );
        self.load_foreign_keys();
    }

    /// Foreign keys come from the Structure view's query, which then needn't run again.
    fn load_foreign_keys(self: &Rc<Self>) {
        if self.object.kind != ObjectKind::Table || !self.driver.kind().is_sql() {
            return;
        }
        let driver = self.driver.clone();
        let obj = self.object.clone();
        let weak = Rc::downgrade(self);
        runtime::spawn(async move { driver.structure(&obj).await }, move |res| {
            let Some(t) = weak.upgrade() else { return };
            let Ok(r) = res else { return };
            let fks = db::parse_foreign_keys(t.driver.kind(), &r);
            t.grid.set_link_columns(fks.iter().map(|fk| fk.column.clone()).collect());
            *t.foreign_keys.borrow_mut() = fks;
            if !t.structure_loaded.get() {
                t.structure.set_result(&r);
                t.structure_loaded.set(true);
            }
        });
    }


    fn load(self: &Rc<Self>) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.set_busy(true);
        self.status.remove_css_class("error");
        self.status.set_tooltip_text(None);

        let filter = self.filter_text();
        let opts = FetchOptions {
            limit: PAGE_SIZE,
            offset: self.offset.get(),
            filter: filter.clone(),
            order_by: self.effective_order(),
        };
        let driver = self.driver.clone();
        let obj = self.object.clone();
        let weak = Rc::downgrade(self);
        runtime::spawn(
            async move {
                let start = Instant::now();
                let (rows, count) =
                    tokio::join!(driver.fetch_rows(&obj, &opts), driver.count_rows(&obj, filter.as_deref()));
                (rows, count.ok(), start.elapsed())
            },
            move |(rows, count, elapsed)| {
                let Some(t) = weak.upgrade() else { return };
                if t.generation.get() != generation {
                    return;
                }
                t.total.set(count);
                match rows {
                    Ok(result) => {
                        t.grid.set_result(&result);
                        t.filter.set_columns(&result.columns);
                        let first = if result.rows.is_empty() { 0 } else { t.offset.get() + 1 };
                        let last = t.offset.get() + result.rows.len();
                        t.page_label.set_text(&match count {
                            Some(total) => format!("{first}-{last} of {total}"),
                            None => format!("{first}-{last}"),
                        });
                        let timing = format!("{} columns · {} ms", result.columns.len(), elapsed.as_millis());
                        t.status.set_text(&t.notice.borrow_mut().take().unwrap_or(timing));
                    }
                    Err(e) => {
                        t.grid.clear();
                        t.page_label.set_text("");
                        t.show_error(&e);
                    }
                }
                t.set_busy(false);
            },
        );
    }

    /// Switch between the Data, Structure and Indexes views (loading the latter on first use).
    fn show_view(self: &Rc<Self>, name: &str) {
        self.stack.set_visible_child_name(name);
        match name {
            "structure" if !self.structure_loaded.get() => self.load_structure(),
            "indexes" if !self.indexes_loaded.get() => self.load_indexes(),
            _ => {}
        }
        self.update_edit_state();
    }

    fn load_indexes(self: &Rc<Self>) {
        let driver = self.driver.clone();
        let obj = self.object.clone();
        let weak = Rc::downgrade(self);
        runtime::spawn(async move { driver.indexes(&obj).await }, move |res| {
            let Some(t) = weak.upgrade() else { return };
            match res {
                Ok(r) => {
                    t.indexes.set_result(&r);
                    t.indexes_loaded.set(true);
                }
                Err(e) => t.show_error(&e),
            }
        });
    }

    fn load_structure(self: &Rc<Self>) {
        let driver = self.driver.clone();
        let obj = self.object.clone();
        let weak = Rc::downgrade(self);
        runtime::spawn(async move { driver.structure(&obj).await }, move |res| {
            let Some(t) = weak.upgrade() else { return };
            match res {
                Ok(r) => {
                    t.structure.set_result(&r);
                    t.structure_loaded.set(true);
                }
                Err(e) => t.show_error(&e),
            }
        });
    }

    pub fn commit(self: &Rc<Self>) {
        let pk = self.pk.borrow().clone();
        let Some(pk) = pk else { return };
        let changes = build_changes(&self.grid.columns(), &pk, self.grid.changes());
        if changes.is_empty() {
            self.grid.discard_changes();
            return;
        }
        self.set_busy(true);
        let driver = self.driver.clone();
        let obj = self.object.clone();
        let weak = Rc::downgrade(self);
        let count = changes.len();
        runtime::spawn(async move { driver.apply_changes(&obj, &changes).await }, move |res| {
            let Some(t) = weak.upgrade() else { return };
            t.set_busy(false);
            match res {
                Ok(_) => {
                    *t.notice.borrow_mut() =
                        Some(format!("Committed {count} {}", if count == 1 { "change" } else { "changes" }));
                    // Pending edits are now in the database; reload without asking.
                    t.grid.discard_changes();
                    t.load();
                }
                Err(e) => {
                    t.commit_error.set_text(&describe_commit_error(&e));
                    t.commit_error.set_visible(true);
                    // Don't leave an older "Committed ..." next to the error.
                    t.status.set_text("");
                }
            }
        });
    }

    fn window(&self) -> Option<gtk::Window> {
        self.widget.root().and_downcast::<gtk::Window>()
    }

    fn choose_export(self: &Rc<Self>, format: Format) {
        let dialog = gtk::FileDialog::builder()
            .title("Export Table")
            .initial_name(format!("{}.{}", self.object.name.replace(['/', ':'], "_"), format.extension()))
            .build();
        let weak = Rc::downgrade(self);
        dialog.save(self.window().as_ref(), None::<&gio::Cancellable>, move |res| {
            if let (Some(t), Ok(file)) = (weak.upgrade(), res)
                && let Some(path) = file.path()
            {
                t.export_to(path, format);
            }
        });
    }

    pub fn export_to(self: &Rc<Self>, path: PathBuf, format: Format) {
        let driver = self.driver.clone();
        let obj = self.object.clone();
        let filter = self.filter_text();
        let order_by = self.effective_order();
        self.spinner.set_visible(true);
        let weak = Rc::downgrade(self);
        let shown = path.display().to_string();
        runtime::spawn(
            async move {
                let mut rows = Vec::new();
                let mut columns: Vec<String>;
                let mut offset = 0;
                loop {
                    let opts =
                        FetchOptions { limit: EXPORT_BATCH, offset, filter: filter.clone(), order_by: order_by.clone() };
                    let r = driver.fetch_rows(&obj, &opts).await?;
                    let n = r.rows.len();
                    columns = r.columns;
                    rows.extend(r.rows);
                    if n < EXPORT_BATCH {
                        break;
                    }
                    offset += EXPORT_BATCH;
                }
                let kind = driver.kind();
                let table = format!("{}.{}", obj.schema, obj.name);
                let count = rows.len();
                tokio::task::spawn_blocking(move || export::write(&path, format, kind, &table, &columns, &rows))
                    .await??;
                anyhow::Ok(count)
            },
            move |res| {
                let Some(t) = weak.upgrade() else { return };
                t.spinner.set_visible(false);
                match res {
                    Ok(n) => t.flash(&format!("Exported {n} rows to {shown}")),
                    Err(e) => t.show_error(&e.context("Export failed")),
                }
            },
        );
    }

    fn choose_import(self: &Rc<Self>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("CSV files"));
        filter.add_pattern("*.csv");
        filter.add_mime_type("text/csv");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder().title("Import CSV").filters(&filters).build();
        let weak = Rc::downgrade(self);
        dialog.open(self.window().as_ref(), None::<&gio::Cancellable>, move |res| {
            if let (Some(t), Ok(file)) = (weak.upgrade(), res)
                && let Some(path) = file.path()
            {
                t.guard(move |t| t.import_from(path));
            }
        });
    }

    pub fn import_from(self: &Rc<Self>, path: PathBuf) {
        let columns = self.grid.columns();
        let driver = self.driver.clone();
        let obj = self.object.clone();
        self.spinner.set_visible(true);
        let weak = Rc::downgrade(self);
        runtime::spawn(
            async move {
                let changes = tokio::task::spawn_blocking(move || export::read_csv(&path, &columns)).await??;
                let n = changes.len();
                driver.apply_changes(&obj, &changes).await?;
                anyhow::Ok(n)
            },
            move |res| {
                let Some(t) = weak.upgrade() else { return };
                t.spinner.set_visible(false);
                match res {
                    Ok(n) => {
                        *t.notice.borrow_mut() = Some(format!("Imported {n} rows"));
                        t.load();
                    }
                    Err(e) => t.show_error(&e.context("Import failed, nothing was inserted")),
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> db::Cell {
        Some(v.to_string())
    }

    #[test]
    fn builds_changes() {
        let cols = vec!["id".to_string(), "name".to_string(), "note".to_string()];
        let pk = vec!["id".to_string()];
        let rows = vec![
            GridRow { original: Some(vec![s("1"), s("a"), None]), current: vec![s("1"), s("b"), None], deleted: false },
            GridRow { original: Some(vec![s("2"), s("x"), None]), current: vec![s("2"), s("x"), None], deleted: true },
            GridRow { original: None, current: vec![None, s("new"), None], deleted: false },
            GridRow { original: None, current: vec![s("9"), None, None], deleted: true },
        ];
        let changes = build_changes(&cols, &pk, rows);
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0], RowChange::Delete { key: vec![("id".into(), s("2"))] });
        match &changes[1] {
            RowChange::Update { key, set, .. } => {
                assert_eq!(key, &vec![("id".to_string(), s("1"))]);
                assert_eq!(set, &vec![("name".to_string(), s("b"))]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(changes[2], RowChange::Insert { values: vec![("name".into(), s("new"))] });
    }
}
