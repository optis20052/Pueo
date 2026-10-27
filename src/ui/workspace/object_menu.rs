//! The sidebar's right-click menu for tables and views: open,
//! export / import, clone, truncate (optionally cascading) and delete.

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use super::{Tab, Workspace};
use crate::db::{self, DbKind, DbObject, ObjectKind};
use crate::runtime;

type ObjectAction = fn(&Rc<Workspace>, &DbObject);

impl Workspace {
    pub(super) fn show_object_menu(self: &Rc<Self>, obj: DbObject, anchor: &gtk::Widget, x: f64, y: f64) {
        let kind = self.kind();
        let is_table = obj.kind == ObjectKind::Table && kind.is_sql();
        let is_relation = matches!(obj.kind, ObjectKind::Table | ObjectKind::View) && kind.is_sql();

        let menu = gio::Menu::new();
        let open = gio::Menu::new();
        if is_relation {
            open.append(Some("Open in New Tab"), Some("obj.open-new"));
            open.append(Some("Open Structure"), Some("obj.open-structure"));
        } else {
            open.append(Some("Open"), Some("obj.open"));
        }
        open.append(Some("Copy Name"), Some("obj.copy-name"));
        menu.append_section(None, &open);

        let pin = gio::Menu::new();
        pin.append(Some(if self.sidebar.is_pinned(&obj) { "Unpin" } else { "Pin to Top" }), Some("obj.pin"));
        menu.append_section(None, &pin);

        if is_relation {
            let transfer = gio::Menu::new();
            let export = gio::Menu::new();
            export.append(Some("CSV…"), Some("obj.export::csv"));
            export.append(Some("JSON…"), Some("obj.export::json"));
            export.append(Some("SQL…"), Some("obj.export::sql"));
            transfer.append_submenu(Some("Export"), &export);
            if is_table {
                transfer.append(Some("Import…"), Some("obj.import"));
            }
            menu.append_section(None, &transfer);

            let manage = gio::Menu::new();
            if is_table {
                manage.append(Some("Clone…"), Some("obj.clone"));
                manage.append(Some("Truncate…"), Some("obj.truncate"));
            }
            manage.append(Some("Delete…"), Some("obj.delete"));
            menu.append_section(None, &manage);
        }

        let actions = gio::SimpleActionGroup::new();
        let simple: [(&str, ObjectAction); 9] = [
            ("pin", |ws, obj| ws.sidebar.toggle_pin(obj)),
            ("open", |ws, obj| ws.open_object(obj.clone())),
            ("open-new", |ws, obj| ws.open_object_tab(obj.clone(), true)),
            ("open-structure", |ws, obj| {
                ws.open_object(obj.clone());
                ws.current_tab(|tab| {
                    if let Tab::Table(t) = tab {
                        t.show_structure();
                    }
                });
            }),
            ("copy-name", |_, obj| {
                if let Some(display) = gdk::Display::default() {
                    display.clipboard().set_text(&obj.name);
                }
            }),
            ("import", |ws, obj| ws.run_tab_action(obj, "tab.import", None)),
            ("clone", |ws, obj| ws.confirm_clone(obj)),
            ("truncate", |ws, obj| ws.confirm_truncate(obj)),
            ("delete", |ws, obj| ws.confirm_drop(obj)),
        ];
        for (name, f) in simple {
            let action = gio::SimpleAction::new(name, None);
            let (weak, obj) = (Rc::downgrade(self), obj.clone());
            action.connect_activate(move |_, _| {
                if let Some(ws) = weak.upgrade() {
                    f(&ws, &obj);
                }
            });
            actions.add_action(&action);
        }
        let export = gio::SimpleAction::new("export", Some(glib::VariantTy::STRING));
        let (weak, target) = (Rc::downgrade(self), obj.clone());
        export.connect_activate(move |_, param| {
            if let Some(ws) = weak.upgrade() {
                ws.run_tab_action(&target, "tab.export", param.cloned());
            }
        });
        actions.add_action(&export);

        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.insert_action_group("obj", Some(&actions));
        popover.set_parent(anchor);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.connect_closed(|p| {
            // Unparent after the chosen action has run.
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        super::super::pointer_cursors(&popover);
        popover.popup();
    }

    /// Open (or focus) the table's tab and run one of its actions there (export, import).
    fn run_tab_action(self: &Rc<Self>, obj: &DbObject, action: &str, param: Option<glib::Variant>) {
        self.open_object(obj.clone());
        self.current_tab(|tab| {
            if let Tab::Table(t) = tab {
                let _ = t.widget.activate_action(action, param.as_ref());
            }
        });
    }

    // ---------- dialogs ----------

    fn confirm_truncate(self: &Rc<Self>, obj: &DbObject) {
        let kind = self.kind();
        let dialog = adw::AlertDialog::new(
            Some(&format!("Truncate “{}”?", obj.name)),
            Some("Every row in this table will be permanently deleted. This cannot be undone."),
        );
        let options = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
        let cascade = match kind {
            DbKind::Postgres => Some(option_check(
                "Cascade",
                "Also truncate every table with a foreign key to this one",
            )),
            DbKind::Mysql => Some(option_check(
                "Ignore foreign key checks",
                "MySQL can’t cascade a truncate; this empties the table even if other rows reference it",
            )),
            _ => None,
        };
        let restart = (kind == DbKind::Postgres)
            .then(|| option_check("Restart identity", "Reset the table’s sequences, so new ids start from 1 again"));
        for (_, row) in cascade.iter().chain(restart.iter()) {
            options.append(row);
        }
        if options.first_child().is_some() {
            dialog.set_extra_child(Some(&options));
        }
        dialog.add_responses(&[("cancel", "Cancel"), ("truncate", "Truncate")]);
        dialog.set_response_appearance("truncate", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        let (weak, obj) = (Rc::downgrade(self), obj.clone());
        dialog.connect_response(None, move |_, response| {
            let Some(ws) = weak.upgrade() else { return };
            if response != "truncate" {
                return;
            }
            let cascade = cascade.as_ref().is_some_and(|(c, _)| c.is_active());
            let restart = restart.as_ref().is_some_and(|(c, _)| c.is_active());
            let (stmts, finally) = db::truncate_sql(ws.kind(), &obj, cascade, restart);
            let target = obj.clone();
            ws.run_statements(stmts, finally, "Could not truncate the table", move |ws| {
                // A cascade empties other tables too: reload every open table then.
                ws.reload_tables(|t| cascade || *t == target);
            });
        });
        super::super::pointer_cursors(&dialog);
        dialog.present(Some(&self.widget));
    }

    fn confirm_drop(self: &Rc<Self>, obj: &DbObject) {
        let kind = self.kind();
        let what = if obj.kind == ObjectKind::View { "View" } else { "Table" };
        let dialog = adw::AlertDialog::new(
            Some(&format!("Delete {} “{}”?", what.to_lowercase(), obj.name)),
            Some(&format!("The {} and all its data will be permanently removed. This cannot be undone.", what.to_lowercase())),
        );
        let cascade = (kind == DbKind::Postgres).then(|| {
            option_check("Cascade", "Also drop views and foreign keys that depend on it")
        });
        if let Some((_, row)) = &cascade {
            dialog.set_extra_child(Some(row));
        }
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        let (weak, obj) = (Rc::downgrade(self), obj.clone());
        dialog.connect_response(None, move |_, response| {
            let Some(ws) = weak.upgrade() else { return };
            if response != "delete" {
                return;
            }
            let cascade = cascade.as_ref().is_some_and(|(c, _)| c.is_active());
            let sql = db::drop_sql(ws.kind(), &obj, cascade);
            let target = obj.clone();
            ws.run_statements(vec![sql], None, "Could not delete", move |ws| {
                ws.close_object_tabs(&target);
                ws.load_objects(None);
            });
        });
        super::super::pointer_cursors(&dialog);
        dialog.present(Some(&self.widget));
    }

    fn confirm_clone(self: &Rc<Self>, obj: &DbObject) {
        let dialog = adw::AlertDialog::new(
            Some(&format!("Clone “{}”", obj.name)),
            Some("Create a new table with the same columns, in the same schema."),
        );
        let name = adw::EntryRow::builder().title("New Table Name").text(format!("{}_copy", obj.name)).build();
        let with_data = adw::SwitchRow::builder().title("Copy Data").subtitle("Also copy every row").active(true).build();
        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
        list.append(&name);
        list.append(&with_data);
        dialog.set_extra_child(Some(&list));
        dialog.add_responses(&[("cancel", "Cancel"), ("clone", "Clone")]);
        dialog.set_response_appearance("clone", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("clone"));
        dialog.set_close_response("cancel");
        // No empty name.
        let d = dialog.downgrade();
        name.connect_changed(move |e| {
            if let Some(d) = d.upgrade() {
                d.set_response_enabled("clone", !e.text().trim().is_empty());
            }
        });

        let (weak, obj) = (Rc::downgrade(self), obj.clone());
        dialog.connect_response(None, move |_, response| {
            let Some(ws) = weak.upgrade() else { return };
            if response != "clone" {
                return;
            }
            let new_name = name.text().trim().to_string();
            let stmts = db::clone_sql(ws.kind(), &obj, &new_name, with_data.is_active());
            let created = DbObject { name: new_name, kind: ObjectKind::Table, signature: String::new(), ..obj.clone() };
            ws.run_statements(stmts, None, "Could not clone the table", move |ws| {
                ws.load_objects(None);
                ws.open_object(created);
            });
        });
        super::super::pointer_cursors(&dialog);
        dialog.present(Some(&self.widget));
    }

    // ---------- running ----------

    /// Run `stmts` one by one on the connection, then `finally` whatever happened.
    /// Errors are shown under `failure`; `done` runs after success.
    pub(super) fn run_statements(
        self: &Rc<Self>,
        stmts: Vec<String>,
        finally: Option<String>,
        failure: &'static str,
        done: impl FnOnce(&Rc<Self>) + 'static,
    ) {
        let driver = self.driver();
        let weak = Rc::downgrade(self);
        runtime::spawn(
            async move {
                let mut result = Ok(());
                for sql in &stmts {
                    if let Err(e) = driver.execute(sql).await {
                        result = Err(e);
                        break;
                    }
                }
                if let Some(sql) = finally {
                    let cleanup = driver.execute(&sql).await;
                    result = result.and(cleanup.map(|_| ()));
                }
                result
            },
            move |res| {
                let Some(ws) = weak.upgrade() else { return };
                match res {
                    Ok(()) => done(&ws),
                    Err(e) => ws.show_error(failure, &format!("{e:#}")),
                }
            },
        );
    }

    pub(super) fn reload_tables(&self, which: impl Fn(&DbObject) -> bool) {
        let tabs: Vec<_> = self
            .tabs
            .borrow()
            .iter()
            .filter_map(|(_, tab)| match tab {
                Tab::Table(t) if which(&t.object) => Some(t.clone()),
                _ => None,
            })
            .collect();
        for t in tabs {
            t.reload();
        }
    }

    /// Close the tabs showing `obj` (it no longer exists), without asking about edits.
    fn close_object_tabs(&self, obj: &DbObject) {
        let pages: Vec<adw::TabPage> = self
            .tabs
            .borrow()
            .iter()
            .filter_map(|(page, tab)| matches!(tab, Tab::Table(t) if t.object == *obj).then(|| page.clone()))
            .collect();
        // Unregister first, so closing doesn't offer to save edits to a dropped table.
        self.tabs.borrow_mut().retain(|(page, _)| !pages.contains(page));
        for page in pages {
            self.tab_view.close_page(&page);
        }
    }
}

/// A check box with a bold title and a dim explanation: the check, and the row to place.
pub(super) fn option_check(title: &str, description: &str) -> (gtk::CheckButton, gtk::Box) {
    let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).hexpand(true).build();
    text.append(&gtk::Label::builder().label(title).xalign(0.0).css_classes(["heading"]).build());
    text.append(
        &gtk::Label::builder()
            .label(description)
            .xalign(0.0)
            .wrap(true)
            // Fill the dialog's width; wrap only at its edge.
            .hexpand(true)
            .width_chars(1)
            .max_width_chars(80)
            .css_classes(["dim-label", "caption"])
            .build(),
    );
    let check = gtk::CheckButton::builder().child(&text).hexpand(true).build();
    let row = gtk::Box::builder().build();
    row.append(&check);
    (check, row)
}
