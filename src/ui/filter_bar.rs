//! Filter bar: rows of [column] [operator] [value], or raw SQL.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::db::DbKind;
use crate::filter::{self, Condition, Op};

struct Rule {
    widget: gtk::Box,
    column: gtk::DropDown,
    op: gtk::DropDown,
    value: gtk::Entry,
    remove: gtk::Button,
}

pub struct FilterBar {
    /// Slides open under the tab; hidden until the user asks for a filter.
    pub widget: gtk::Revealer,
    kind: DbKind,
    columns: gtk::StringList,
    rules_box: gtk::Box,
    rules: RefCell<Vec<Rc<Rule>>>,
    stack: gtk::Stack,
    raw: gtk::Entry,
    sql_toggle: gtk::ToggleButton,
    match_all: gtk::DropDown,
    apply_button: gtk::Button,
    /// The WHERE body currently applied to the table.
    applied: RefCell<Option<String>>,
    on_apply: RefCell<Option<Rc<dyn Fn()>>>,
}

fn string_dropdown(model: &gtk::StringList) -> gtk::DropDown {
    let dd = gtk::DropDown::builder().model(model).build();
    dd.set_expression(Some(gtk::PropertyExpression::new(
        gtk::StringObject::static_type(),
        None::<&gtk::Expression>,
        "string",
    )));
    dd
}

impl FilterBar {
    pub fn new(kind: DbKind) -> Rc<Self> {
        let columns = gtk::StringList::new(&[]);
        let rules_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
        let raw = gtk::Entry::builder()
            .placeholder_text("WHERE clause, e.g.  id > 100 AND name ILIKE '%foo%'")
            .hexpand(true)
            .css_classes(["monospace"])
            .build();
        let stack = gtk::Stack::builder().hexpand(true).vhomogeneous(false).build();
        stack.add_named(&rules_box, Some("visual"));
        stack.add_named(&raw, Some("raw"));

        let match_all = gtk::DropDown::from_strings(&["Match all", "Match any"]);
        match_all.set_tooltip_text(Some("How rules are combined"));
        match_all.set_valign(gtk::Align::Start);
        let sql_toggle = gtk::ToggleButton::builder()
            .label("SQL")
            .tooltip_text("Write the WHERE clause yourself")
            .valign(gtk::Align::Start)
            .css_classes(["flat"])
            .build();
        // Only shown while the rules differ from what is applied; Enter applies too.
        let apply_button = gtk::Button::builder()
            .label("Apply")
            .tooltip_text("Apply Filter (Enter)")
            .valign(gtk::Align::Start)
            .visible(false)
            .css_classes(["suggested-action"])
            .build();
        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Remove Filter (Esc)")
            .valign(gtk::Align::Start)
            .css_classes(["flat", "circular"])
            .build();

        let content = gtk::Box::builder()
            .spacing(6)
            .margin_start(8)
            .margin_end(8)
            .margin_top(6)
            .margin_bottom(6)
            .css_classes(["filter-bar"])
            .build();
        content.append(&stack);
        content.append(&match_all);
        content.append(&apply_button);
        content.append(&sql_toggle);
        content.append(&close);
        let widget = gtk::Revealer::builder()
            .child(&content)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .reveal_child(false)
            .build();

        let bar = Rc::new(FilterBar {
            widget,
            kind,
            columns,
            rules_box,
            rules: RefCell::new(Vec::new()),
            stack,
            raw,
            sql_toggle,
            match_all,
            apply_button,
            applied: RefCell::new(None),
            on_apply: RefCell::new(None),
        });
        bar.add_rule(None);

        let weak = Rc::downgrade(&bar);
        bar.apply_button.connect_clicked(move |_| {
            if let Some(b) = weak.upgrade() {
                b.apply();
            }
        });
        let weak = Rc::downgrade(&bar);
        close.connect_clicked(move |_| {
            if let Some(b) = weak.upgrade() {
                b.close();
            }
        });
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(&bar);
        keys.connect_key_pressed(move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape
                && let Some(b) = weak.upgrade()
            {
                b.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        content.add_controller(keys);

        let weak = Rc::downgrade(&bar);
        bar.raw.connect_activate(move |_| {
            if let Some(b) = weak.upgrade() {
                b.apply();
            }
        });
        let weak = Rc::downgrade(&bar);
        bar.raw.connect_changed(move |_| {
            if let Some(b) = weak.upgrade() {
                b.refresh_dirty();
            }
        });
        let weak = Rc::downgrade(&bar);
        bar.match_all.connect_selected_notify(move |_| {
            if let Some(b) = weak.upgrade() {
                b.refresh_dirty();
            }
        });
        let weak = Rc::downgrade(&bar);
        bar.sql_toggle.connect_toggled(move |t| {
            let Some(b) = weak.upgrade() else { return };
            if t.is_active() {
                // Start the raw editor from the current rules.
                if b.raw.text().is_empty()
                    && let Some(sql) = b.visual_where()
                {
                    b.raw.set_text(&sql);
                }
                b.stack.set_visible_child_name("raw");
                b.raw.grab_focus();
            } else {
                b.stack.set_visible_child_name("visual");
            }
            b.update_match_visibility();
            b.refresh_dirty();
        });
        bar.update_match_visibility();
        bar
    }

    pub fn connect_apply(&self, f: impl Fn() + 'static) {
        *self.on_apply.borrow_mut() = Some(Rc::new(f));
    }

    /// A filter is currently applied to the table.
    pub fn is_active(&self) -> bool {
        self.applied.borrow().is_some()
    }

    /// Slide the bar open and focus the first value.
    pub fn show(&self) {
        self.widget.set_reveal_child(true);
        self.focus();
    }

    fn apply(&self) {
        *self.applied.borrow_mut() = self.where_clause();
        self.refresh_dirty();
        let cb = self.on_apply.borrow().clone();
        if let Some(cb) = cb {
            cb();
        }
    }

    /// Remove the filter and hide the bar.
    fn close(self: &Rc<Self>) {
        self.clear();
        self.widget.set_reveal_child(false);
        if self.is_active() {
            self.apply();
        } else {
            self.refresh_dirty();
        }
    }

    fn refresh_dirty(&self) {
        let dirty = self.where_clause() != *self.applied.borrow();
        self.apply_button.set_visible(dirty);
    }

    /// Start with the rule `column = value` applied and the bar open, before the table
    /// has loaded (the column list is filled in later and keeps the selection).
    pub fn preset(self: &Rc<Self>, column: &str, value: &str) {
        self.clear();
        self.set_columns(&[column.to_string()]);
        if let Some(rule) = self.rules.borrow().first() {
            rule.column.set_selected(0);
            rule.op.set_selected(Op::ALL.iter().position(|o| *o == Op::Eq).unwrap_or(0) as u32);
            rule.value.set_text(value);
        }
        *self.applied.borrow_mut() = self.where_clause();
        self.widget.set_reveal_child(true);
        self.refresh_dirty();
    }

    /// Update the column choices, keeping each rule's column where possible.
    pub fn set_columns(&self, names: &[String]) {
        let current: Vec<String> = self.columns.snapshot().iter().filter_map(|o| o.downcast_ref::<gtk::StringObject>().map(|s| s.string().to_string())).collect();
        if current == names {
            return;
        }
        let selected: Vec<Option<String>> = self.rules.borrow().iter().map(|r| selected_string(&r.column)).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        self.columns.splice(0, self.columns.n_items(), &refs);
        for (rule, sel) in self.rules.borrow().iter().zip(selected) {
            let pos = sel.and_then(|s| names.iter().position(|n| *n == s)).unwrap_or(0);
            rule.column.set_selected(pos as u32);
        }
    }

    pub fn focus(&self) {
        if self.sql_toggle.is_active() {
            self.raw.grab_focus();
        } else if let Some(rule) = self.rules.borrow().first() {
            rule.value.grab_focus();
        }
    }

    /// The WHERE body to use, or `None` for no filter.
    pub fn where_clause(&self) -> Option<String> {
        if self.sql_toggle.is_active() {
            Some(self.raw.text().trim().to_string()).filter(|s| !s.is_empty())
        } else {
            self.visual_where()
        }
    }

    fn visual_where(&self) -> Option<String> {
        let conditions: Vec<Condition> = self
            .rules
            .borrow()
            .iter()
            .filter_map(|r| {
                Some(Condition {
                    column: selected_string(&r.column)?,
                    op: Op::ALL[r.op.selected() as usize],
                    value: r.value.text().to_string(),
                })
            })
            .collect();
        filter::build_where(self.kind, &conditions, self.match_all.selected() == 0)
    }

    fn clear(self: &Rc<Self>) {
        self.raw.set_text("");
        while self.rules.borrow().len() > 1 {
            let rule = self.rules.borrow_mut().pop().unwrap();
            self.rules_box.remove(&rule.widget);
        }
        if let Some(rule) = self.rules.borrow().first() {
            rule.value.set_text("");
            rule.op.set_selected(0);
        }
        self.update_match_visibility();
    }

    fn update_match_visibility(&self) {
        let rules = self.rules.borrow();
        self.match_all.set_visible(!self.sql_toggle.is_active() && rules.len() > 1);
        for r in rules.iter() {
            r.remove.set_sensitive(rules.len() > 1);
        }
    }

    /// Add a rule after `after` (or at the end).
    fn add_rule(self: &Rc<Self>, after: Option<&Rc<Rule>>) {
        let column = string_dropdown(&self.columns);
        column.set_enable_search(true);
        column.set_tooltip_text(Some("Column"));
        column.set_size_request(180, -1);
        let ops: Vec<&str> = Op::ALL.iter().map(|o| o.label()).collect();
        let op = gtk::DropDown::from_strings(&ops);
        op.set_tooltip_text(Some("Operator"));
        op.set_size_request(150, -1);
        let value = gtk::Entry::builder()
            .placeholder_text("Value")
            .hexpand(true)
            .css_classes(["monospace"])
            .build();
        let remove = gtk::Button::builder()
            .icon_name("list-remove-symbolic")
            .tooltip_text("Remove Rule")
            .css_classes(["flat", "circular"])
            .build();
        let add = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add Rule")
            .css_classes(["flat", "circular"])
            .build();
        let widget = gtk::Box::builder().spacing(6).build();
        widget.append(&column);
        widget.append(&op);
        widget.append(&value);
        widget.append(&remove);
        widget.append(&add);

        let rule = Rc::new(Rule { widget, column, op, value, remove });

        let wr = Rc::downgrade(&rule);
        rule.op.connect_selected_notify(move |dd| {
            let Some(r) = wr.upgrade() else { return };
            let op = Op::ALL[dd.selected() as usize];
            r.value.set_sensitive(op.needs_value());
            r.value.set_placeholder_text(Some(if op == Op::In { "a, b, c" } else { "Value" }));
        });
        let weak = Rc::downgrade(self);
        rule.value.connect_changed(move |_| {
            if let Some(b) = weak.upgrade() {
                b.refresh_dirty();
            }
        });
        let weak = Rc::downgrade(self);
        rule.column.connect_selected_notify(move |_| {
            if let Some(b) = weak.upgrade() {
                b.refresh_dirty();
            }
        });
        let weak = Rc::downgrade(self);
        rule.op.connect_selected_notify(move |_| {
            if let Some(b) = weak.upgrade() {
                b.refresh_dirty();
            }
        });
        let weak = Rc::downgrade(self);
        rule.value.connect_activate(move |_| {
            if let Some(b) = weak.upgrade() {
                b.apply();
            }
        });
        let (weak, wr) = (Rc::downgrade(self), Rc::downgrade(&rule));
        add.connect_clicked(move |_| {
            if let (Some(b), Some(r)) = (weak.upgrade(), wr.upgrade()) {
                b.add_rule(Some(&r));
            }
        });
        let (weak, wr) = (Rc::downgrade(self), Rc::downgrade(&rule));
        rule.remove.connect_clicked(move |_| {
            let (Some(b), Some(r)) = (weak.upgrade(), wr.upgrade()) else { return };
            if b.rules.borrow().len() <= 1 {
                return;
            }
            b.rules_box.remove(&r.widget);
            b.rules.borrow_mut().retain(|x| !Rc::ptr_eq(x, &r));
            b.update_match_visibility();
            // Removing a rule changes the result right away.
            let weak = Rc::downgrade(&b);
            glib::idle_add_local_once(move || {
                if let Some(b) = weak.upgrade() {
                    b.apply();
                }
            });
        });

        let mut rules = self.rules.borrow_mut();
        match after.and_then(|a| rules.iter().position(|x| Rc::ptr_eq(x, a))) {
            Some(i) => {
                self.rules_box.insert_child_after(&rule.widget, Some(&rules[i].widget));
                // Start with the same column as the rule it was added from.
                rule.column.set_selected(rules[i].column.selected());
                rules.insert(i + 1, rule.clone());
            }
            None => {
                self.rules_box.append(&rule.widget);
                rules.push(rule.clone());
            }
        }
        drop(rules);
        self.update_match_visibility();
        if after.is_some() {
            rule.value.grab_focus();
        }
    }
}

fn selected_string(dd: &gtk::DropDown) -> Option<String> {
    dd.selected_item().and_downcast::<gtk::StringObject>().map(|s| s.string().to_string())
}
