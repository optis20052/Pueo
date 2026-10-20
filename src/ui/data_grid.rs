use std::cell::{Cell as StdCell, RefCell};
use std::cmp::Ordering;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::{gdk, gio, glib, graphene};

use crate::db::{Cell, Choices, ColumnChoices, QueryResult};

type SortCallback = Box<dyn Fn(Option<(String, bool)>)>;
type EditCloser = Rc<dyn Fn(bool) -> Option<gtk::Stack>>;

const MAX_CELL_CHARS: usize = 300;

/// One row of the grid together with its edit state.
#[derive(Debug, Clone)]
pub struct GridRow {
    /// Values as loaded from the database; `None` for rows inserted in the grid.
    pub original: Option<Vec<Cell>>,
    pub current: Vec<Cell>,
    pub deleted: bool,
}

impl GridRow {
    pub fn is_dirty(&self) -> bool {
        self.deleted || self.original.as_ref() != Some(&self.current)
    }

    fn is_modified(&self, idx: usize) -> bool {
        self.original.as_ref().is_some_and(|o| o.get(idx) != self.current.get(idx))
    }
}

/// A spreadsheet-like view of a `QueryResult`, optionally editable.
pub struct DataGrid {
    pub widget: gtk::ScrolledWindow,
    view: gtk::ColumnView,
    store: gio::ListStore,
    selection: gtk::MultiSelection,
    columns: RefCell<Vec<String>>,
    server_sort: bool,
    editable: StdCell<bool>,
    on_sort: RefCell<Option<SortCallback>>,
    on_changed: RefCell<Option<Box<dyn Fn()>>>,
    on_selection: RefCell<Option<Box<dyn Fn()>>>,
    /// Feedback for the status line: (message, is_error).
    on_notice: RefCell<Option<Box<dyn Fn(&str, bool)>>>,
    /// Set while a row is swapped for a fresh copy, so selection listeners are not told.
    refreshing: StdCell<bool>,
    suppress_sort: StdCell<bool>,
    /// Last sort seen, to turn the third header click into "no sort".
    last_sort: RefCell<Option<(String, bool)>>,
    /// Primary-key column names; left empty when pasting or duplicating rows.
    key_names: RefCell<Vec<String>>,
    /// Columns edited with a dropdown of fixed values.
    choices: RefCell<Choices>,
    /// Foreign-key columns: Ctrl+clicking a value's text follows it.
    link_columns: RefCell<Vec<String>>,
    /// Called with (column, value) when a link is followed.
    on_follow: RefCell<Option<Box<dyn Fn(&str, &str)>>>,
    /// Ctrl is held: links are highlighted.
    ctrl: StdCell<bool>,
    /// The cell under the pointer and the pointer's x in it, to update its cursor when Ctrl changes.
    hover: RefCell<Option<(glib::WeakRef<gtk::Widget>, glib::WeakRef<gtk::ListItem>, usize, f64)>>,
    /// Watches Ctrl on the window while the grid is shown.
    window_keys: RefCell<Option<(glib::WeakRef<gtk::Widget>, gtk::EventControllerKey)>>,
    /// The single open inline editor: its close function (true = save) and its entry.
    active_edit: RefCell<Option<(EditCloser, glib::WeakRef<gtk::Entry>)>>,
    menu: gtk::PopoverMenu,
    blank_menu: gtk::PopoverMenu,
    actions: gio::SimpleActionGroup,
    /// Row position, column and cell widget the context menu was opened on.
    context: RefCell<Option<(u32, usize, glib::WeakRef<gtk::Widget>)>>,
}

fn display_text(value: &str) -> String {
    let mut s: String = value.chars().take(MAX_CELL_CHARS).collect();
    if s.contains(['\n', '\r', '\t']) {
        s = s.replace("\r\n", "↵").replace(['\n', '\r'], "↵").replace('\t', " ");
    }
    if value.chars().count() > MAX_CELL_CHARS {
        s.push('…');
    }
    s
}

/// Compare numerically when both sides are numbers, otherwise as text. NULLs sort first.
fn compare_cells(a: &Cell, b: &Cell) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, _) => Ordering::Less,
        (_, None) => Ordering::Greater,
        (Some(a), Some(b)) => match (a.parse::<f64>(), b.parse::<f64>()) {
            (Ok(x), Ok(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
            _ => a.cmp(b),
        },
    }
}

fn row_of(obj: &glib::Object) -> std::cell::Ref<'_, GridRow> {
    obj.downcast_ref::<glib::BoxedAnyObject>().expect("row object").borrow::<GridRow>()
}

fn row_mut(obj: &glib::Object) -> std::cell::RefMut<'_, GridRow> {
    obj.downcast_ref::<glib::BoxedAnyObject>().expect("row object").borrow_mut::<GridRow>()
}

/// Rows copied from a grid together with their column names, so pasting into another
/// tab or database can put each value in the column of the same name. Kept with the
/// exact clipboard text: if the clipboard changed since (a copy elsewhere), it's ignored.
struct CopiedRows {
    text: String,
    columns: Vec<String>,
    rows: Vec<Vec<Cell>>,
}

thread_local! {
    static COPIED: RefCell<Option<CopiedRows>> = const { RefCell::new(None) };
}

/// Match copied rows to `target` columns by name (exact, then ignoring case). Target
/// columns that weren't copied, and key columns, are left empty (NULL, so an insert
/// leaves them to the column default); copied columns the target lacks are dropped.
/// `None` when no column matches at all.
fn map_rows(columns: &[String], rows: &[Vec<Cell>], target: &[String], keys: &[usize]) -> Option<Vec<Vec<Cell>>> {
    let sources: Vec<Option<usize>> = target
        .iter()
        .enumerate()
        .map(|(i, name)| {
            if keys.contains(&i) {
                return None;
            }
            columns.iter().position(|c| c == name).or_else(|| columns.iter().position(|c| c.eq_ignore_ascii_case(name)))
        })
        .collect();
    if sources.iter().all(Option::is_none) {
        return None;
    }
    Some(rows.iter().map(|row| sources.iter().map(|src| src.and_then(|s| row.get(s).cloned().flatten())).collect()).collect())
}

/// Turn clipboard text (tab-separated, one row per line) into grid rows.
/// Rows with one value per column are placed as-is; rows with one value per
/// non-key column fill the non-key columns in order. Key columns are always left empty.
fn parse_rows(text: &str, ncols: usize, keys: &[usize]) -> Vec<Vec<Cell>> {
    let non_key: Vec<usize> = (0..ncols).filter(|i| !keys.contains(i)).collect();
    text.lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let values: Vec<Cell> =
                line.split('\t').map(|v| if v == "NULL" { None } else { Some(v.to_string()) }).collect();
            let mut row = vec![None; ncols];
            if values.len() == non_key.len() && values.len() < ncols {
                for (slot, v) in non_key.iter().zip(values) {
                    row[*slot] = v;
                }
            } else {
                for (i, v) in values.into_iter().take(ncols).enumerate() {
                    if !keys.contains(&i) {
                        row[i] = v;
                    }
                }
            }
            row
        })
        .collect()
}

/// Where to point a popover for a cell: its start, not its middle, since the last
/// column can stretch across the whole window.
fn cell_anchor(cell: &gtk::Widget, view: &gtk::ColumnView) -> Option<gdk::Rectangle> {
    let b = cell.compute_bounds(view)?;
    let width = (b.width() as i32).min(160);
    Some(gdk::Rectangle::new(b.x() as i32, b.y() as i32, width, b.height() as i32))
}

/// Whether a dropdown option matches the cell value; PostgreSQL returns booleans as t/f.
fn same_choice(option: &Cell, value: &Cell) -> bool {
    match (option.as_deref(), value.as_deref()) {
        (Some("true"), Some("t")) | (Some("false"), Some("f")) => true,
        _ => option == value,
    }
}

/// Show one cell's value and edit state on its label.
fn render_label(label: &gtk::Label, row: &GridRow, idx: usize) {
    for class in ["null-cell", "modified-cell", "inserted-cell", "deleted-cell"] {
        label.remove_css_class(class);
    }
    match row.current.get(idx).cloned().flatten() {
        Some(v) => label.set_text(&display_text(&v)),
        None => {
            label.set_text("NULL");
            label.add_css_class("null-cell");
        }
    }
    if row.deleted {
        label.add_css_class("deleted-cell");
    } else if row.original.is_none() {
        label.add_css_class("inserted-cell");
    } else if row.is_modified(idx) {
        label.add_css_class("modified-cell");
    }
}

fn tsv_escape(c: &Cell) -> String {
    match c {
        None => "NULL".into(),
        Some(s) => s.replace(['\t', '\n'], " "),
    }
}

fn new_row(original: Option<Vec<Cell>>, current: Vec<Cell>) -> glib::BoxedAnyObject {
    glib::BoxedAnyObject::new(GridRow { original, current, deleted: false })
}

impl DataGrid {
    /// `server_sort`: header clicks call the sort callback instead of sorting locally.
    pub fn new(server_sort: bool) -> Rc<Self> {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let view = gtk::ColumnView::builder()
            .show_column_separators(true)
            .show_row_separators(true)
            .reorderable(true)
            .css_classes(["data-grid"])
            .build();

        let model: gio::ListModel = if server_sort {
            store.clone().upcast()
        } else {
            gtk::SortListModel::new(Some(store.clone()), view.sorter()).upcast()
        };
        let selection = gtk::MultiSelection::new(Some(model));
        view.set_model(Some(&selection));

        let widget = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .hexpand(true)
            .build();

        let menu_model = gio::Menu::new();
        let view_section = gio::Menu::new();
        view_section.append(Some("Row Details"), Some("grid.details"));
        view_section.append(Some("Copy Cell"), Some("grid.copy-cell"));
        view_section.append(Some("Copy Rows"), Some("grid.copy"));
        view_section.append(Some("Paste Rows"), Some("grid.paste"));
        menu_model.append_section(None, &view_section);
        let edit_section = gio::Menu::new();
        edit_section.append(Some("Edit Cell"), Some("grid.edit"));
        edit_section.append(Some("Set NULL"), Some("grid.set-null"));
        edit_section.append(Some("Duplicate Rows"), Some("grid.duplicate"));
        edit_section.append(Some("Delete Rows"), Some("grid.delete"));
        menu_model.append_section(None, &edit_section);
        let menu = gtk::PopoverMenu::from_model(Some(&menu_model));
        menu.set_has_arrow(false);
        menu.set_halign(gtk::Align::Start);
        menu.set_parent(&view);
        // Right-click on the empty space below the rows: only what needs no row.
        let blank_model = gio::Menu::new();
        blank_model.append(Some("Paste Rows"), Some("grid.paste"));
        let blank_menu = gtk::PopoverMenu::from_model(Some(&blank_model));
        blank_menu.set_has_arrow(false);
        blank_menu.set_halign(gtk::Align::Start);
        blank_menu.set_parent(&view);

        let grid = Rc::new(DataGrid {
            widget,
            view,
            store,
            selection,
            columns: RefCell::new(Vec::new()),
            server_sort,
            editable: StdCell::new(false),
            on_sort: RefCell::new(None),
            on_changed: RefCell::new(None),
            on_selection: RefCell::new(None),
            on_notice: RefCell::new(None),
            refreshing: StdCell::new(false),
            suppress_sort: StdCell::new(false),
            last_sort: RefCell::new(None),
            key_names: RefCell::new(Vec::new()),
            link_columns: RefCell::new(Vec::new()),
            on_follow: RefCell::new(None),
            ctrl: StdCell::new(false),
            hover: RefCell::new(None),
            window_keys: RefCell::new(None),
            active_edit: RefCell::new(None),
            choices: RefCell::new(Choices::new()),
            menu,
            blank_menu,
            actions: gio::SimpleActionGroup::new(),
            context: RefCell::new(None),
        });
        grid.connect_signals();
        grid.watch_ctrl();
        grid.setup_actions();
        grid.update_action_state();
        grid
    }

    fn connect_signals(self: &Rc<Self>) {
        // Any click in the grid outside the open editor saves and closes it.
        let outside = gtk::GestureClick::builder().button(0).build();
        outside.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        outside.connect_pressed(move |_, _, x, y| {
            let Some(grid) = weak.upgrade() else { return };
            let entry = grid.active_edit.borrow().as_ref().and_then(|(_, e)| e.upgrade());
            let Some(entry) = entry else { return };
            let inside = grid
                .view
                .pick(x, y, gtk::PickFlags::DEFAULT)
                .is_some_and(|w| w == *entry.upcast_ref::<gtk::Widget>() || w.is_ancestor(&entry));
            if !inside {
                grid.close_active_edit(true);
            }
        });
        self.view.add_controller(outside);

        // Header clicks cycle ascending → descending → unsorted.
        let weak = Rc::downgrade(self);
        if let Some(sorter) = self.view.sorter() {
            sorter.connect_changed(move |sorter, _| {
                let Some(grid) = weak.upgrade() else { return };
                if grid.suppress_sort.get() {
                    return;
                }
                let Some(sorter) = sorter.downcast_ref::<gtk::ColumnViewSorter>() else { return };
                let mut order = sorter.primary_sort_column().map(|col| {
                    (
                        col.title().map(|t| t.to_string()).unwrap_or_default(),
                        sorter.primary_sort_order() == gtk::SortType::Ascending,
                    )
                });
                let previous = grid.last_sort.borrow().clone();
                if let (Some((prev_col, false)), Some((col, true))) = (&previous, &order)
                    && prev_col == col
                {
                    grid.suppress_sort.set(true);
                    grid.view.sort_by_column(None, gtk::SortType::Ascending);
                    grid.suppress_sort.set(false);
                    order = None;
                }
                *grid.last_sort.borrow_mut() = order.clone();
                if grid.server_sort
                    && let Some(cb) = grid.on_sort.borrow().as_ref()
                {
                    cb(order);
                }
            });
        }

        let controller = gtk::ShortcutController::new();
        let shortcut = |trigger: &str, grid: &Rc<Self>, f: fn(&Rc<Self>)| {
            let weak = Rc::downgrade(grid);
            let action = gtk::CallbackAction::new(move |_, _| {
                if let Some(grid) = weak.upgrade() {
                    f(&grid);
                }
                glib::Propagation::Stop
            });
            gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string(trigger), Some(action))
        };
        controller.add_shortcut(shortcut("<Control>c", self, |g| g.copy_selection()));
        controller.add_shortcut(shortcut("<Control>v", self, |g| g.paste_rows()));
        controller.add_shortcut(shortcut("Delete", self, |g| {
            if g.editable.get() {
                g.delete_selected();
            }
        }));
        self.view.add_controller(controller);

        // Right-click outside the cells (they claim their own clicks) offers Paste.
        let blank_click = gtk::GestureClick::builder().button(gdk::BUTTON_SECONDARY).build();
        let weak = Rc::downgrade(self);
        blank_click.connect_pressed(move |gesture, _, x, y| {
            let Some(grid) = weak.upgrade() else { return };
            if !grid.editable.get() {
                return;
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
            grid.blank_menu.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            grid.blank_menu.popup();
        });
        self.view.add_controller(blank_click);

        // Enter shows the full row.
        let weak: Weak<Self> = Rc::downgrade(self);
        self.view.connect_activate(move |_, pos| {
            if let Some(grid) = weak.upgrade() {
                grid.show_row_details(pos);
            }
        });

        let weak = Rc::downgrade(self);
        self.selection.connect_selection_changed(move |_, _, _| {
            let Some(grid) = weak.upgrade() else { return };
            if grid.refreshing.get() {
                return;
            }
            if let Some(cb) = grid.on_selection.borrow().as_ref() {
                cb();
            }
        });

        self.view.insert_action_group("grid", Some(&self.actions));
    }

    fn setup_actions(self: &Rc<Self>) {
        let add = |name: &str, f: fn(&Rc<Self>)| {
            let action = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(grid) = weak.upgrade() {
                    f(&grid);
                }
            });
            self.actions.add_action(&action);
        };
        add("details", |g| {
            if let Some((pos, _, _)) = g.context.borrow().clone() {
                g.show_row_details(pos);
            }
        });
        add("copy-cell", |g| {
            let Some((pos, idx, _)) = g.context.borrow().clone() else { return };
            if let Some(obj) = g.model().item(pos) {
                let value = row_of(&obj).current.get(idx).cloned().flatten().unwrap_or_default();
                g.view.clipboard().set_text(&value);
            }
        });
        add("copy", |g| g.copy_selection());
        add("edit", |g| {
            let Some((pos, idx, anchor)) = g.context.borrow().clone() else { return };
            if let Some(anchor) = anchor.upgrade() {
                g.start_edit(pos, idx, &anchor);
            }
        });
        add("set-null", |g| {
            let Some((pos, idx, _)) = g.context.borrow().clone() else { return };
            if let Some(obj) = g.model().item(pos) {
                g.set_cell(&obj, idx, None);
            }
        });
        add("duplicate", |g| {
            let rows = g.selected_rows().into_iter().map(|row| g.without_keys(row)).collect();
            g.append_rows(rows);
        });
        add("paste", |g| g.paste_rows());
        add("delete", |g| g.delete_selected());
    }

    fn update_action_state(&self) {
        let editable = self.editable.get();
        for name in ["edit", "set-null", "duplicate", "delete", "paste"] {
            if let Some(a) = self.actions.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                a.set_enabled(editable);
            }
        }
    }

    fn model(&self) -> gio::ListModel {
        self.selection.clone().upcast()
    }

    pub fn connect_sort_changed(&self, f: impl Fn(Option<(String, bool)>) + 'static) {
        *self.on_sort.borrow_mut() = Some(Box::new(f));
    }

    /// Called whenever the set of pending changes changes.
    /// Messages for the status line, e.g. why a paste did nothing.
    pub fn connect_notice(&self, f: impl Fn(&str, bool) + 'static) {
        *self.on_notice.borrow_mut() = Some(Box::new(f));
    }

    fn notice(&self, msg: &str, error: bool) {
        if let Some(f) = self.on_notice.borrow().as_ref() {
            f(msg, error);
        }
    }

    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        *self.on_changed.borrow_mut() = Some(Box::new(f));
    }

    fn notify_changed(&self) {
        if let Some(cb) = self.on_changed.borrow().as_ref() {
            cb();
        }
    }

    pub fn set_editable(&self, editable: bool) {
        self.editable.set(editable);
        self.update_action_state();
    }

    pub fn set_result(self: &Rc<Self>, result: &QueryResult) {
        if *self.columns.borrow() != result.columns {
            self.rebuild_columns(result);
        }
        let rows: Vec<glib::BoxedAnyObject> =
            result.rows.iter().map(|r| new_row(Some(r.clone()), r.clone())).collect();
        self.store.splice(0, self.store.n_items(), &rows);
        self.widget.vadjustment().set_value(0.0);
        self.notify_changed();
    }

    pub fn clear(self: &Rc<Self>) {
        self.set_result(&QueryResult::default());
    }

    pub fn columns(&self) -> Vec<String> {
        self.columns.borrow().clone()
    }

    /// Current values of all rows not marked as deleted, in display order.
    pub fn rows(&self) -> Vec<Vec<Cell>> {
        let model = self.model();
        (0..model.n_items())
            .filter_map(|i| model.item(i))
            .filter(|o| !row_of(o).deleted)
            .map(|o| row_of(&o).current.clone())
            .collect()
    }

    // ---------- editing ----------

    fn all_rows(&self) -> Vec<glib::Object> {
        (0..self.store.n_items()).filter_map(|i| self.store.item(i)).collect()
    }

    /// Rows with pending changes.
    pub fn changes(&self) -> Vec<GridRow> {
        self.all_rows().iter().map(|o| row_of(o).clone()).filter(GridRow::is_dirty).collect()
    }

    pub fn pending_count(&self) -> usize {
        self.all_rows().iter().filter(|o| row_of(o).is_dirty()).count()
    }

    pub fn has_changes(&self) -> bool {
        self.pending_count() > 0
    }

    pub fn discard_changes(&self) {
        let rows: Vec<glib::BoxedAnyObject> = self
            .all_rows()
            .iter()
            .filter_map(|o| row_of(o).original.clone())
            .map(|orig| new_row(Some(orig.clone()), orig))
            .collect();
        self.store.splice(0, self.store.n_items(), &rows);
        self.notify_changed();
    }

    /// Re-bind a row after its state changed. GTK keeps the existing binding when the same
    /// object is re-inserted, so the row is replaced by a fresh copy.
    fn refresh(&self, obj: &glib::Object) {
        let Some(pos) = self.store.find(obj) else { return };
        let model = self.model();
        let was_selected = (0..model.n_items())
            .find(|&i| model.item(i).as_ref() == Some(obj))
            .is_some_and(|i| self.selection.is_selected(i));
        let copy = glib::BoxedAnyObject::new(row_of(obj).clone()).upcast::<glib::Object>();
        self.refreshing.set(true);
        self.store.splice(pos, 1, &[copy.clone()]);
        // Keep the row selected (replacing it would otherwise drop the selection).
        if was_selected && let Some(i) = (0..model.n_items()).find(|&i| model.item(i).as_ref() == Some(&copy)) {
            self.selection.select_item(i, false);
        }
        self.refreshing.set(false);
    }

    // ---------- API for the row inspector ----------

    pub fn is_editable(&self) -> bool {
        self.editable.get()
    }

    pub fn connect_selection_changed(&self, f: impl Fn() + 'static) {
        *self.on_selection.borrow_mut() = Some(Box::new(f));
    }

    /// Selected rows, in display order.
    pub fn selected_positions(&self) -> Vec<u32> {
        let bitset = self.selection.selection();
        (0..bitset.size()).map(|i| bitset.nth(i as u32)).collect()
    }

    pub fn row_at(&self, pos: u32) -> Option<GridRow> {
        self.model().item(pos).map(|o| row_of(&o).clone())
    }

    /// Set one cell of the row shown at `pos` (a pending change, like editing in the grid).
    pub fn set_cell_at(&self, pos: u32, idx: usize, value: Cell) {
        if let Some(obj) = self.model().item(pos) {
            self.set_cell(&obj, idx, value);
        }
    }

    pub fn choices_for(&self, column: &str) -> Option<ColumnChoices> {
        self.choices.borrow().get(column).cloned()
    }

    fn set_cell(&self, obj: &glib::Object, idx: usize, value: Cell) {
        {
            let mut row = row_mut(obj);
            if row.deleted || row.current.get(idx) == Some(&value) {
                return;
            }
            row.current[idx] = value;
        }
        self.refresh(obj);
        self.notify_changed();
    }

    pub fn set_choices(&self, choices: Choices) {
        *self.choices.borrow_mut() = choices;
    }

    /// Mark foreign-key columns, whose values can be followed with Ctrl+click.
    pub fn set_link_columns(&self, names: Vec<String>) {
        if *self.link_columns.borrow() == names {
            return;
        }
        *self.link_columns.borrow_mut() = names;
        // Re-bind the visible cells so they pick up the link style.
        let n = self.store.n_items();
        self.store.items_changed(0, n, n);
    }

    fn is_link_column(&self, idx: usize) -> bool {
        self.columns.borrow().get(idx).is_some_and(|c| self.link_columns.borrow().contains(c))
    }

    /// Show or hide the links (CSS on the view) as Ctrl goes down or up.
    fn set_ctrl(&self, held: bool) {
        if self.ctrl.replace(held) == held {
            return;
        }
        if held {
            self.view.add_css_class("show-links");
        } else {
            self.view.remove_css_class("show-links");
        }
        self.update_hover_cursor();
    }

    /// A hand over a link's text while Ctrl is held, the normal cursor otherwise.
    fn update_hover_cursor(&self) {
        let hover = self.hover.borrow().clone();
        let Some((cell, item, idx, x)) = hover else { return };
        let (Some(cell), Some(item)) = (cell.upgrade(), item.upgrade()) else { return };
        let on_link = self.ctrl.get() && self.link_at(item.position(), idx, &cell, x).is_some();
        cell.set_cursor_from_name(on_link.then_some("pointer"));
    }

    /// Watch Ctrl on the whole window (not just when the grid has focus) while the grid is mapped.
    fn watch_ctrl(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.widget.connect_map(move |w| {
            let (Some(grid), Some(root)) = (weak.upgrade(), w.root()) else { return };
            let root = root.upcast::<gtk::Widget>();
            let keys = gtk::EventControllerKey::new();
            keys.set_propagation_phase(gtk::PropagationPhase::Capture);
            let is_ctrl = |k: gdk::Key| k == gdk::Key::Control_L || k == gdk::Key::Control_R;
            let wk = Rc::downgrade(&grid);
            keys.connect_key_pressed(move |_, key, _, _| {
                if let (Some(g), true) = (wk.upgrade(), is_ctrl(key)) {
                    g.set_ctrl(true);
                }
                glib::Propagation::Proceed
            });
            let wk = Rc::downgrade(&grid);
            keys.connect_key_released(move |_, key, _, _| {
                if let (Some(g), true) = (wk.upgrade(), is_ctrl(key)) {
                    g.set_ctrl(false);
                }
            });
            root.add_controller(keys.clone());
            if let Some((old_root, old)) = grid.window_keys.replace(Some((root.downgrade(), keys))) {
                if let Some(r) = old_root.upgrade() {
                    r.remove_controller(&old);
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.widget.connect_unmap(move |_| {
            let Some(grid) = weak.upgrade() else { return };
            if let Some((root, keys)) = grid.window_keys.take() {
                if let Some(r) = root.upgrade() {
                    r.remove_controller(&keys);
                }
            }
            // The key release may never arrive (tab switched with Ctrl+PageDown, window left).
            grid.set_ctrl(false);
        });
    }

    /// The (column, value) of the link in cell (`pos`, `idx`) if `x` (in `cell` coordinates)
    /// is on its text; clicks beside the text are left for editing.
    fn link_at(&self, pos: u32, idx: usize, cell: &gtk::Widget, x: f64) -> Option<(String, String)> {
        let column = self.columns.borrow().get(idx).cloned()?;
        if !self.link_columns.borrow().contains(&column) {
            return None;
        }
        let value = self.model().item(pos).and_then(|obj| row_of(&obj).current.get(idx).cloned().flatten())?;
        let label = cell.downcast_ref::<gtk::Stack>()?.child_by_name("label")?;
        let point = cell.compute_point(&label, &graphene::Point::new(x as f32, 0.0))?;
        let label = label.downcast::<gtk::Label>().ok()?;
        let (offset, _) = label.layout_offsets();
        let (_, text) = label.layout().pixel_extents();
        let start = (offset + text.x()) as f32;
        (point.x() >= start && point.x() <= start + text.width() as f32).then_some((column, value))
    }

    pub fn connect_follow(&self, f: impl Fn(&str, &str) + 'static) {
        *self.on_follow.borrow_mut() = Some(Box::new(f));
    }

    /// Follow the link in cell (`pos`, `idx`) if `x` is on its text. Returns whether it did.
    fn follow_link(&self, pos: u32, idx: usize, cell: &gtk::Widget, x: f64) -> bool {
        let Some((column, value)) = self.link_at(pos, idx, cell, x) else { return false };
        match self.on_follow.borrow().as_ref() {
            Some(f) => {
                f(&column, &value);
                true
            }
            None => false,
        }
    }

    pub fn set_key_names(&self, names: Vec<String>) {
        *self.key_names.borrow_mut() = names;
    }

    fn key_indices(&self) -> Vec<usize> {
        let columns = self.columns.borrow();
        self.key_names.borrow().iter().filter_map(|k| columns.iter().position(|c| c == k)).collect()
    }

    /// A copy of `row` with the primary-key columns emptied, so the database assigns them.
    fn without_keys(&self, mut row: Vec<Cell>) -> Vec<Cell> {
        for i in self.key_indices() {
            if let Some(cell) = row.get_mut(i) {
                *cell = None;
            }
        }
        row
    }

    /// Paste tab-separated rows from the clipboard as new rows (keys left empty).
    pub fn paste_rows(self: &Rc<Self>) {
        if !self.editable.get() {
            self.notice("Can't paste rows: this table has no primary key, so it's read-only", true);
            return;
        }
        let clipboard = self.view.clipboard();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let text = clipboard.read_text_future().await.ok().flatten();
            let Some(grid) = weak.upgrade() else { return };
            let Some(text) = text else {
                grid.notice("Nothing to paste: the clipboard has no text", true);
                return;
            };
            let target = grid.columns.borrow().clone();
            let keys = grid.key_indices();
            // Copied from a grid in Pueo: match by column name. Otherwise (or when no
            // column names match), take the text's columns in order.
            let matched = COPIED.with_borrow(|copied| {
                copied.as_ref().filter(|c| c.text == text).and_then(|c| map_rows(&c.columns, &c.rows, &target, &keys))
            });
            let by_name = matched.is_some();
            let rows = matched.unwrap_or_else(|| parse_rows(&text, target.len(), &keys));
            if rows.is_empty() {
                grid.notice("Nothing to paste: the clipboard has no rows", true);
                return;
            }
            let n = rows.len();
            grid.append_rows(rows);
            let how = if by_name { ", matched by column name" } else { "" };
            grid.notice(&format!("Pasted {n} row{}{how} (not saved yet)", if n == 1 { "" } else { "s" }), false);
        });
    }

    /// Add new (uncommitted) rows in one go, then select them and scroll to the first.
    fn append_rows(&self, rows: Vec<Vec<Cell>>) {
        if rows.is_empty() {
            return;
        }
        let objects: Vec<glib::Object> = rows.into_iter().map(|values| new_row(None, values).upcast()).collect();
        self.store.splice(self.store.n_items(), 0, &objects);
        self.notify_changed();

        // Find where they landed (the view may be sorted) in a single pass.
        let added: std::collections::HashSet<&glib::Object> = objects.iter().collect();
        let model = self.model();
        let selected = gtk::Bitset::new_empty();
        for i in 0..model.n_items() {
            if model.item(i).is_some_and(|o| added.contains(&o)) {
                selected.add(i);
            }
        }
        if selected.is_empty() {
            return;
        }
        let first = selected.minimum();
        self.selection.set_selection(&selected, &gtk::Bitset::new_range(0, model.n_items()));
        // Wait for the pending-changes bar to slide in, which shrinks the view.
        let view = self.view.downgrade();
        glib::timeout_add_local_once(std::time::Duration::from_millis(350), move || {
            if let Some(view) = view.upgrade() {
                view.scroll_to(first, None, gtk::ListScrollFlags::FOCUS, None);
            }
        });
    }

    pub fn add_row(&self) {
        if self.editable.get() && !self.columns.borrow().is_empty() {
            self.append_rows(vec![vec![None; self.columns.borrow().len()]]);
        }
    }

    /// Mark the selected rows as deleted (toggles). Rows inserted in the grid are removed.
    pub fn delete_selected(&self) {
        if !self.editable.get() {
            return;
        }
        let bitset = self.selection.selection();
        let model = self.model();
        let objs: Vec<glib::Object> = (0..bitset.size()).filter_map(|i| model.item(bitset.nth(i as u32))).collect();
        for obj in objs {
            let inserted = row_of(&obj).original.is_none();
            if inserted {
                if let Some(pos) = self.store.find(&obj) {
                    self.store.remove(pos);
                }
            } else {
                let deleted = !row_of(&obj).deleted;
                row_mut(&obj).deleted = deleted;
                self.refresh(&obj);
            }
        }
        self.notify_changed();
    }

    /// Edit a cell in place. Enter saves, Tab/Shift+Tab save and move sideways, Escape cancels,
    /// clicking away saves. Long or multi-line values open the larger text editor instead.
    fn start_edit(self: &Rc<Self>, pos: u32, idx: usize, anchor: &gtk::Widget) {
        if !self.editable.get() {
            return;
        }
        let Some(obj) = self.model().item(pos) else { return };
        if row_of(&obj).deleted {
            return;
        }
        let value = row_of(&obj).current.get(idx).cloned().flatten();
        // Enum / boolean columns get a dropdown instead of free text.
        let column = self.columns.borrow().get(idx).cloned().unwrap_or_default();
        let choices = self.choices.borrow().get(&column).cloned();
        if let Some(choices) = choices {
            self.close_active_edit(true);
            self.start_choice_edit(&obj, idx, anchor, &choices, value);
            return;
        }
        let long = value.as_deref().is_some_and(|v| v.contains('\n') || v.chars().count() > 120);
        let Some(stack) = anchor.downcast_ref::<gtk::Stack>().filter(|_| !long) else {
            self.start_edit_popover(pos, idx, anchor);
            return;
        };
        if stack.child_by_name("edit").is_some() {
            return;
        }
        // Only one editor at a time.
        self.close_active_edit(true);

        let entry = gtk::Entry::builder()
            .text(value.as_deref().unwrap_or(""))
            .placeholder_text(if value.is_none() { "NULL" } else { "" })
            .hexpand(true)
            .css_classes(["cell-editor"])
            .build();
        stack.add_named(&entry, Some("edit"));
        stack.set_visible_child(&entry);
        entry.grab_focus();
        // The list may move focus to the row after this click; take it back.
        let e = entry.downgrade();
        glib::idle_add_local_once(move || {
            if let Some(e) = e.upgrade()
                && e.is_mapped()
            {
                e.grab_focus();
            }
        });

        // Close the editor once; `commit` saves the text. Returns the cell stack for navigation.
        let done = Rc::new(StdCell::new(false));
        let finish: Rc<dyn Fn(bool) -> Option<gtk::Stack>> = {
            let (weak, stack, entry, obj) = (Rc::downgrade(self), stack.downgrade(), entry.downgrade(), obj.clone());
            let original = value.clone();
            Rc::new(move |commit| {
                if done.replace(true) {
                    return None;
                }
                let (grid, stack, entry) = (weak.upgrade()?, stack.upgrade()?, entry.upgrade()?);
                let text = entry.text().to_string();
                stack.set_visible_child_name("label");
                let s = stack.clone();
                glib::idle_add_local_once(move || {
                    if entry.parent().as_ref() == Some(s.upcast_ref()) {
                        s.remove(&entry);
                    }
                });
                // An untouched NULL stays NULL instead of becoming ''.
                let unchanged = original.as_deref().unwrap_or("") == text;
                if commit && !unchanged {
                    // Update in place and redraw this cell only: no row rebind, so the
                    // editors of neighbouring cells stay alive for Tab navigation.
                    row_mut(&obj).current[idx] = Some(text);
                    if let Some(label) = stack.child_by_name("label").and_downcast::<gtk::Label>() {
                        render_label(&label, &row_of(&obj), idx);
                    }
                    grid.notify_changed();
                }
                Some(stack)
            })
        };

        *self.active_edit.borrow_mut() = Some((finish.clone(), entry.downgrade()));

        let f = finish.clone();
        let view = self.view.downgrade();
        entry.connect_activate(move |_| {
            f(true);
            if let Some(v) = view.upgrade() {
                v.grab_focus();
            }
        });
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let (f, weak, view) = (finish.clone(), Rc::downgrade(self), self.view.downgrade());
        keys.connect_key_pressed(move |_, key, _, state| match key {
            gdk::Key::Escape => {
                f(false);
                if let Some(v) = view.upgrade() {
                    v.grab_focus();
                }
                glib::Propagation::Stop
            }
            gdk::Key::Tab | gdk::Key::ISO_Left_Tab => {
                let back = key == gdk::Key::ISO_Left_Tab || state.contains(gdk::ModifierType::SHIFT_MASK);
                if let (Some(stack), Some(grid)) = (f(true), weak.upgrade()) {
                    // Continue in the neighbouring cell (in on-screen column order).
                    let cell = stack.parent();
                    let neighbour =
                        if back { cell.and_then(|c| c.prev_sibling()) } else { cell.and_then(|c| c.next_sibling()) };
                    if let Some(next) = neighbour.and_then(|c| c.first_child()) {
                        let next_idx = next.widget_name().strip_prefix("cell-").and_then(|n| n.parse().ok());
                        if let Some(next_idx) = next_idx {
                            grid.start_edit(pos, next_idx, &next);
                        }
                    }
                }
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        entry.add_controller(keys);
        let focus = gtk::EventControllerFocus::new();
        let f = finish.clone();
        focus.connect_leave(move |_| {
            f(true);
        });
        entry.add_controller(focus);
    }

    /// A dropdown of allowed values anchored to the cell (enum, boolean, CHECK ... IN).
    fn start_choice_edit(
        self: &Rc<Self>,
        obj: &glib::Object,
        idx: usize,
        anchor: &gtk::Widget,
        choices: &ColumnChoices,
        current: Cell,
    ) {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Browse)
            .css_classes(["choice-list"])
            .build();
        // Row data: Some(value) or None for NULL.
        let mut options: Vec<Cell> = choices.values.iter().cloned().map(Some).collect();
        if choices.nullable {
            options.push(None);
        }
        for option in &options {
            // Menu-style row: small tick column, then the value in the grid's font.
            let row_box = gtk::Box::builder().spacing(6).build();
            let check = gtk::Image::builder().icon_name("object-select-symbolic").pixel_size(12).build();
            check.set_opacity(if same_choice(option, &current) { 1.0 } else { 0.0 });
            row_box.append(&check);
            let label = gtk::Label::builder()
                .label(option.as_deref().unwrap_or("NULL"))
                .xalign(0.0)
                .hexpand(true)
                .max_width_chars(48)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["cell"])
                .build();
            // Show values in full; only very long ones are shortened.
            label.set_width_chars(label.label().chars().count().min(48) as i32);
            if option.is_none() {
                label.add_css_class("null-cell");
            }
            row_box.append(&label);
            list.append(&row_box);
        }
        let search = gtk::SearchEntry::builder().placeholder_text("Search values").visible(options.len() > 8).build();

        // At least as wide as the cell, otherwise as wide as the longest value.
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .width_request(anchor.width().clamp(120, 320))
            .build();
        content.append(&search);
        // Short lists go in directly: a scrolled area keeps a minimum height of its own,
        // which left an empty strip under two or three values.
        if options.len() > 8 {
            let scroll = gtk::ScrolledWindow::builder()
                .child(&list)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .propagate_natural_height(true)
                .max_content_height(320)
                .propagate_natural_width(true)
                .build();
            content.append(&scroll);
        } else {
            content.append(&list);
        }
        let popover = gtk::Popover::builder()
            .child(&content)
            .position(gtk::PositionType::Bottom)
            .has_arrow(false)
            .css_classes(["choice-popover"])
            .build();

        let options = Rc::new(options);
        let o = options.clone();
        let s = search.downgrade();
        list.set_filter_func(move |row| {
            let needle = s.upgrade().map(|s| s.text().to_lowercase()).unwrap_or_default();
            needle.is_empty()
                || o.get(row.index() as usize)
                    .is_some_and(|v| v.as_deref().unwrap_or("null").to_lowercase().contains(&needle))
        });
        let l = list.downgrade();
        search.connect_search_changed(move |_| {
            if let Some(l) = l.upgrade() {
                l.invalidate_filter();
            }
        });

        let weak = Rc::downgrade(self);
        let (p, o, obj) = (popover.downgrade(), options.clone(), obj.clone());
        let anchor_weak = anchor.downgrade();
        list.connect_row_activated(move |_, row| {
            let Some(value) = o.get(row.index() as usize).cloned() else { return };
            if let Some(g) = weak.upgrade() {
                // Same in-place update as the text editor: no row rebind.
                row_mut(&obj).current[idx] = value;
                if let Some(label) = anchor_weak
                    .upgrade()
                    .and_downcast::<gtk::Stack>()
                    .and_then(|s| s.child_by_name("label"))
                    .and_downcast::<gtk::Label>()
                {
                    render_label(&label, &row_of(&obj), idx);
                }
                g.notify_changed();
            }
            if let Some(p) = p.upgrade() {
                p.popdown();
            }
        });
        // Enter in the search box picks the first match.
        let l = list.downgrade();
        search.connect_activate(move |_| {
            let Some(l) = l.upgrade() else { return };
            l.invalidate_filter();
            let mut i = 0;
            while let Some(row) = l.row_at_index(i) {
                if row.is_child_visible() {
                    row.activate();
                    break;
                }
                i += 1;
            }
        });

        popover.set_parent(&self.view);
        popover.set_pointing_to(cell_anchor(anchor, &self.view).as_ref());
        let view = self.view.downgrade();
        popover.connect_closed(move |p| {
            let p = p.clone();
            let view = view.clone();
            glib::idle_add_local_once(move || {
                p.unparent();
                if let Some(v) = view.upgrade() {
                    v.grab_focus();
                }
            });
        });
        popover.popup();
        // Start on the current value so arrow keys move from there.
        let selected = options.iter().position(|v| same_choice(v, &current)).unwrap_or(0);
        if let Some(row) = list.row_at_index(selected as i32) {
            list.select_row(Some(&row));
            // Focus once the popover is on screen, or arrow keys start from the top.
            let (row, search) = (row.downgrade(), search.downgrade());
            glib::idle_add_local_once(move || {
                match search.upgrade().filter(|s| s.is_visible()) {
                    Some(s) => {
                        s.grab_focus();
                    }
                    None => {
                        if let Some(r) = row.upgrade() {
                            r.grab_focus();
                        }
                    }
                }
            });
        }
    }

    /// Close the open inline editor, saving its text when `save` is true.
    fn close_active_edit(&self, save: bool) {
        let active = self.active_edit.borrow_mut().take();
        if let Some((close, _)) = active {
            close(save);
        }
    }

    /// The larger editor for long or multi-line values.
    fn start_edit_popover(self: &Rc<Self>, pos: u32, idx: usize, anchor: &gtk::Widget) {
        if !self.editable.get() {
            return;
        }
        let Some(obj) = self.model().item(pos) else { return };
        if row_of(&obj).deleted {
            return;
        }
        let value = row_of(&obj).current.get(idx).cloned().flatten();
        let multiline = value.as_deref().is_some_and(|v| v.contains('\n') || v.chars().count() > 120);

        let popover = gtk::Popover::builder().position(gtk::PositionType::Bottom).build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        let column = self.columns.borrow().get(idx).cloned().unwrap_or_default();
        content.append(
            &gtk::Label::builder()
                .label(&column)
                .xalign(0.0)
                .css_classes(["caption-heading", "dim-label"])
                .build(),
        );

        // Commit a value and close the popover.
        let commit: Rc<dyn Fn(Cell)> = {
            let weak = Rc::downgrade(self);
            let p = popover.downgrade();
            let obj = obj.clone();
            Rc::new(move |v| {
                if let Some(g) = weak.upgrade() {
                    g.set_cell(&obj, idx, v);
                }
                if let Some(p) = p.upgrade() {
                    p.popdown();
                }
            })
        };

        // Returns the edited text.
        let get_text: Rc<dyn Fn() -> String> = if multiline {
            let text = gtk::TextView::builder()
                .monospace(true)
                .wrap_mode(gtk::WrapMode::WordChar)
                .top_margin(6)
                .bottom_margin(6)
                .left_margin(6)
                .right_margin(6)
                .build();
            text.buffer().set_text(value.as_deref().unwrap_or(""));
            let scroll = gtk::ScrolledWindow::builder()
                .child(&text)
                .min_content_width(420)
                .min_content_height(180)
                .css_classes(["card"])
                .build();
            content.append(&scroll);
            let get: Rc<dyn Fn() -> String> = {
                let text = text.clone();
                Rc::new(move || {
                    let b = text.buffer();
                    b.text(&b.start_iter(), &b.end_iter(), false).to_string()
                })
            };
            let keys = gtk::EventControllerKey::new();
            let (commit2, get2) = (commit.clone(), get.clone());
            keys.connect_key_pressed(move |_, key, _, state| {
                if matches!(key, gdk::Key::Return | gdk::Key::KP_Enter)
                    && state.contains(gdk::ModifierType::CONTROL_MASK)
                {
                    commit2(Some(get2()));
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            text.add_controller(keys);
            get
        } else {
            let entry = gtk::Entry::builder()
                .text(value.as_deref().unwrap_or(""))
                .placeholder_text(if value.is_none() { "NULL" } else { "" })
                .width_chars(36)
                .css_classes(["monospace"])
                .build();
            content.append(&entry);
            let commit2 = commit.clone();
            entry.connect_activate(move |e| commit2(Some(e.text().to_string())));
            Rc::new(move || entry.text().to_string())
        };

        let buttons = gtk::Box::builder().spacing(6).build();
        let null_btn = gtk::Button::builder().label("Set NULL").css_classes(["flat"]).build();
        let spacer = gtk::Box::builder().hexpand(true).build();
        let cancel = gtk::Button::with_label("Cancel");
        let save = gtk::Button::builder()
            .label("Save")
            .tooltip_text(if multiline { "Ctrl+Enter" } else { "Enter" })
            .css_classes(["suggested-action"])
            .build();
        buttons.append(&null_btn);
        buttons.append(&spacer);
        buttons.append(&cancel);
        buttons.append(&save);
        content.append(&buttons);
        popover.set_child(Some(&content));

        let c = commit.clone();
        null_btn.connect_clicked(move |_| c(None));
        let p = popover.downgrade();
        cancel.connect_clicked(move |_| {
            if let Some(p) = p.upgrade() {
                p.popdown();
            }
        });
        save.connect_clicked(move |_| commit(Some(get_text())));

        popover.set_parent(&self.view);
        popover.set_pointing_to(cell_anchor(anchor, &self.view).as_ref());
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    fn show_context_menu(&self, pos: u32, idx: usize, anchor: &gtk::Widget, x: f64, y: f64) {
        if !self.selection.is_selected(pos) {
            self.selection.select_item(pos, true);
        }
        *self.context.borrow_mut() = Some((pos, idx, anchor.downgrade()));
        let point = anchor
            .compute_point(&self.view, &graphene::Point::new(x as f32, y as f32))
            .unwrap_or_else(|| graphene::Point::new(0.0, 0.0));
        self.menu.set_pointing_to(Some(&gdk::Rectangle::new(point.x() as i32, point.y() as i32, 1, 1)));
        self.menu.popup();
    }

    // ---------- columns ----------

    fn rebuild_columns(self: &Rc<Self>, result: &QueryResult) {
        self.suppress_sort.set(true);
        self.view.sort_by_column(None, gtk::SortType::Ascending);
        *self.last_sort.borrow_mut() = None;
        while let Some(col) = self.view.columns().item(0).and_downcast::<gtk::ColumnViewColumn>() {
            self.view.remove_column(&col);
        }
        for (idx, name) in result.columns.iter().enumerate() {
            let col = gtk::ColumnViewColumn::new(Some(name), Some(self.cell_factory(idx)));
            col.set_resizable(true);
            col.set_fixed_width(Self::estimate_width(name, result, idx));
            let sorter = if self.server_sort {
                gtk::CustomSorter::new(|_, _| gtk::Ordering::Equal)
            } else {
                gtk::CustomSorter::new(move |a, b| {
                    compare_cells(&row_of(a).current[idx], &row_of(b).current[idx]).into()
                })
            };
            col.set_sorter(Some(&sorter));
            col.set_expand(idx + 1 == result.columns.len());
            self.view.append_column(&col);
        }
        *self.columns.borrow_mut() = result.columns.clone();
        self.suppress_sort.set(false);
    }

    /// Pick a column width from the header and the first rows, in pixels.
    fn estimate_width(name: &str, result: &QueryResult, idx: usize) -> i32 {
        let longest = result
            .rows
            .iter()
            .take(100)
            .map(|r| r[idx].as_deref().map_or(4, |s| s.chars().take(60).count()))
            .max()
            .unwrap_or(0)
            .max(name.chars().count() + 2);
        (longest as i32 * 8 + 24).clamp(70, 420)
    }

    fn cell_factory(self: &Rc<Self>, idx: usize) -> gtk::SignalListItemFactory {
        let factory = gtk::SignalListItemFactory::new();
        let weak = Rc::downgrade(self);
        factory.connect_setup(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let label = gtk::Label::builder()
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .single_line_mode(true)
                .css_classes(["cell"])
                .build();
            // A stack so the cell can swap its label for an inline editor.
            let stack = gtk::Stack::builder().hhomogeneous(false).vhomogeneous(false).build();
            stack.add_named(&label, Some("label"));
            stack.set_widget_name(&format!("cell-{idx}"));
            let click = gtk::GestureClick::builder().button(0).build();
            let item_ref = item.downgrade();
            let weak_click = weak.clone();
            click.connect_pressed(move |gesture, n_press, x, y| {
                let (Some(grid), Some(item)) = (weak_click.upgrade(), item_ref.upgrade()) else { return };
                let Some(widget) = gesture.widget() else { return };
                // Clicks inside an open editor belong to the editor.
                if widget.downcast_ref::<gtk::Stack>().is_some_and(|s| s.child_by_name("edit").is_some()) {
                    return;
                }
                let pos = item.position();
                match (gesture.current_button(), n_press) {
                    (3, _) => {
                        gesture.set_state(gtk::EventSequenceState::Claimed);
                        grid.show_context_menu(pos, idx, &widget, x, y);
                    }
                    // Ctrl+click on the value's text of a foreign key: open the referenced row.
                    (1, 1)
                        if gesture.current_event_state().contains(gdk::ModifierType::CONTROL_MASK)
                            && grid.follow_link(pos, idx, &widget, x) =>
                    {
                        gesture.set_state(gtk::EventSequenceState::Claimed);
                    }
                    (1, 2) if grid.editable.get() => {
                        gesture.set_state(gtk::EventSequenceState::Claimed);
                        grid.start_edit(pos, idx, &widget);
                    }
                    _ => {}
                }
            });
            stack.add_controller(click);
            // Track the hovered cell, for the hand cursor over links while Ctrl is held.
            let motion = gtk::EventControllerMotion::new();
            let (weak_m, item_m) = (weak.clone(), item.downgrade());
            motion.connect_motion(move |m, x, _| {
                let (Some(grid), Some(widget)) = (weak_m.upgrade(), m.widget()) else { return };
                *grid.hover.borrow_mut() = Some((widget.downgrade(), item_m.clone(), idx, x));
                grid.set_ctrl(m.current_event_state().contains(gdk::ModifierType::CONTROL_MASK));
                grid.update_hover_cursor();
            });
            let weak_m = weak.clone();
            motion.connect_leave(move |m| {
                let Some(grid) = weak_m.upgrade() else { return };
                if let Some(w) = m.widget() {
                    w.set_cursor(None);
                }
                grid.hover.borrow_mut().take();
            });
            stack.add_controller(motion);
            item.set_child(Some(&stack));
        });
        let weak = Rc::downgrade(self);
        factory.connect_bind(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let stack = item.child().and_downcast::<gtk::Stack>().unwrap();
            // A recycled cell must not keep an editor from another row; save it first.
            if let Some(editor) = stack.child_by_name("edit") {
                if let Some(grid) = weak.upgrade() {
                    grid.close_active_edit(true);
                }
                if editor.parent().is_some() {
                    stack.set_visible_child_name("label");
                    stack.remove(&editor);
                }
            }
            let label = stack.child_by_name("label").and_downcast::<gtk::Label>().unwrap();
            let Some(obj) = item.item() else { return };
            render_label(&label, &row_of(&obj), idx);
            let is_link = weak.upgrade().is_some_and(|g| g.is_link_column(idx)) && row_of(&obj).current[idx].is_some();
            if is_link {
                label.add_css_class("link-cell");
            } else {
                label.remove_css_class("link-cell");
            }
        });
        factory
    }

    // ---------- copy & details ----------

    fn selected_rows(&self) -> Vec<Vec<Cell>> {
        let bitset = self.selection.selection();
        let model = self.model();
        (0..bitset.size())
            .filter_map(|i| model.item(bitset.nth(i as u32)))
            .map(|obj| row_of(&obj).current.clone())
            .collect()
    }

    pub fn copy_selection(&self) {
        let rows = self.selected_rows();
        if rows.is_empty() {
            return;
        }
        let text = rows
            .iter()
            .map(|r| r.iter().map(tsv_escape).collect::<Vec<_>>().join("\t"))
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(display) = gdk::Display::default() {
            display.clipboard().set_text(&text);
        }
        let columns = self.columns.borrow().clone();
        COPIED.with_borrow_mut(|copied| *copied = Some(CopiedRows { text, columns, rows }));
    }

    fn show_row_details(&self, pos: u32) {
        let Some(obj) = self.model().item(pos) else { return };
        let row = row_of(&obj).current.clone();
        let columns = self.columns.borrow().clone();

        let group = adw::PreferencesGroup::new();
        for (name, value) in columns.iter().zip(row.iter()) {
            let action_row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(name))
                .subtitle(glib::markup_escape_text(value.as_deref().unwrap_or("NULL")))
                .subtitle_selectable(true)
                .css_classes(["property"])
                .build();
            let copy = gtk::Button::builder()
                .icon_name("edit-copy-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text("Copy value")
                .css_classes(["flat"])
                .build();
            let value = value.clone().unwrap_or_default();
            copy.connect_clicked(move |b| b.clipboard().set_text(&value));
            action_row.add_suffix(&copy);
            group.add(&action_row);
        }
        let page = adw::PreferencesPage::new();
        page.add(&group);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&page));

        let dialog = adw::Dialog::builder()
            .title(format!("Row {}", pos + 1))
            .content_width(560)
            .content_height(600)
            .child(&toolbar)
            .build();
        dialog.present(Some(&self.widget));
    }
}

impl Drop for DataGrid {
    fn drop(&mut self) {
        self.menu.unparent();
        self.blank_menu.unparent();
    }
}

#[cfg(test)]
mod tests {
    use super::{map_rows, parse_rows};

    #[test]
    fn maps_copied_rows_by_column_name() {
        let s = |v: &str| Some(v.to_string());
        let cols = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        let copied = cols(&["id", "name", "gone"]);
        let rows = vec![vec![s("1"), s("Ada"), s("x")], vec![s("2"), None, s("y")]];
        // Reordered target with a new column; "gone" is dropped, "email" stays empty, the key is cleared.
        assert_eq!(
            map_rows(&copied, &rows, &cols(&["email", "Name", "id"]), &[2]),
            Some(vec![vec![None, s("Ada"), None], vec![None, None, None]])
        );
        // Nothing in common: fall back to the plain text.
        assert_eq!(map_rows(&copied, &rows, &cols(&["a", "b"]), &[]), None);
    }

    #[test]
    fn parses_pasted_rows() {
        let s = |v: &str| Some(v.to_string());
        // Full rows copied from the grid: the key (column 0) is dropped.
        assert_eq!(
            parse_rows("1\tAda\tNULL\r\n2\tLinus\tx\n\n", 3, &[0]),
            vec![vec![None, s("Ada"), None], vec![None, s("Linus"), s("x")]]
        );
        // Rows without the key fill the other columns.
        assert_eq!(parse_rows("Grace\ty", 3, &[0]), vec![vec![None, s("Grace"), s("y")]]);
        // Short rows are padded with NULL.
        assert_eq!(parse_rows("9", 3, &[]), vec![vec![s("9"), None, None]]);
    }
}
