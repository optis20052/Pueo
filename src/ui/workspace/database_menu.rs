//! Managing the server's databases from the database switcher:
//! create, rename (PostgreSQL), delete, back up and restore.

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use super::object_menu::option_check;
use super::Workspace;
use crate::db::{self, DbKind};

type DatabaseAction = fn(&Rc<Workspace>, &str);

impl Workspace {
    /// Add "New Database…" and each database's "⋯" menu to the switcher.
    pub(super) fn setup_database_management(self: &Rc<Self>) {
        if !self.kind().manages_databases() {
            return;
        }
        let new_database = gtk::Button::builder()
            .child(&adw::ButtonContent::builder().icon_name("list-add-symbolic").label("New Database…").build())
            .css_classes(["flat"])
            .build();
        new_database.set_cursor_from_name(Some("pointer"));
        let weak = Rc::downgrade(self);
        new_database.connect_clicked(move |_| {
            if let Some(ws) = weak.upgrade() {
                ws.db_picker.popover.popdown();
                ws.confirm_create_database();
            }
        });
        self.db_picker.add_footer(&gtk::Separator::new(gtk::Orientation::Horizontal));
        self.db_picker.add_footer(&new_database);

        let weak = Rc::downgrade(self);
        self.db_picker.connect_row_menu(move |name, anchor| {
            if let Some(ws) = weak.upgrade() {
                ws.show_database_menu(name, &anchor);
            }
        });
    }

    fn show_database_menu(self: &Rc<Self>, name: String, anchor: &gtk::Widget) {
        let menu = gio::Menu::new();
        let copy = gio::Menu::new();
        copy.append(Some("Copy Name"), Some("db.copy-name"));
        menu.append_section(None, &copy);
        let manage = gio::Menu::new();
        if self.kind() == DbKind::Postgres {
            manage.append(Some("Rename…"), Some("db.rename"));
        }
        manage.append(Some("Delete…"), Some("db.delete"));
        let backup = gio::Menu::new();
        backup.append(Some("Back Up…"), Some("db.backup"));
        backup.append(Some("Restore…"), Some("db.restore"));
        menu.append_section(None, &backup);
        menu.append_section(None, &manage);

        let actions = gio::SimpleActionGroup::new();
        let simple: [(&str, DatabaseAction); 5] = [
            ("backup", |ws, name| ws.start_backup(name.to_string())),
            ("restore", |ws, name| ws.start_restore(name.to_string())),
            ("copy-name", |_, name| {
                if let Some(display) = gdk::Display::default() {
                    display.clipboard().set_text(name);
                }
            }),
            ("rename", |ws, name| ws.confirm_rename_database(name)),
            ("delete", |ws, name| ws.confirm_drop_database(name)),
        ];
        for (action_name, f) in simple {
            let action = gio::SimpleAction::new(action_name, None);
            let (weak, name) = (Rc::downgrade(self), name.clone());
            action.connect_activate(move |_, _| {
                if let Some(ws) = weak.upgrade() {
                    ws.db_picker.popover.popdown();
                    f(&ws, &name);
                }
            });
            actions.add_action(&action);
        }

        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.insert_action_group("db", Some(&actions));
        popover.set_parent(anchor);
        popover.connect_closed(|p| {
            // Unparent after the chosen action has run.
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        super::super::pointer_cursors(&popover);
        popover.popup();
    }

    // ---------- dialogs ----------

    fn confirm_create_database(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::new(Some("New Database"), Some("Create an empty database on this server."));
        let name = adw::EntryRow::builder().title("Name").build();
        let switch_to = adw::SwitchRow::builder()
            .title("Switch to It")
            .subtitle("Open the new database once it’s created")
            .active(true)
            .build();
        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
        list.append(&name);
        list.append(&switch_to);
        dialog.set_extra_child(Some(&list));
        dialog.add_responses(&[("cancel", "Cancel"), ("create", "Create")]);
        dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
        dialog.set_response_enabled("create", false);
        dialog.set_default_response(Some("create"));
        dialog.set_close_response("cancel");
        enable_when(&dialog, "create", &name, |text| !text.is_empty());

        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            let Some(ws) = weak.upgrade() else { return };
            if response != "create" {
                return;
            }
            let new_name = name.text().trim().to_string();
            let switch = switch_to.is_active();
            let sql = db::create_database_sql(ws.kind(), &new_name);
            ws.run_statements(vec![sql], None, "Could not create the database", move |ws| {
                if switch {
                    ws.switch_database(new_name);
                } else {
                    ws.load_databases();
                }
            });
        });
        super::super::pointer_cursors(&dialog);
        dialog.present(Some(&self.widget));
    }

    fn confirm_rename_database(self: &Rc<Self>, old: &str) {
        let dialog = adw::AlertDialog::new(
            Some(&format!("Rename “{old}”")),
            Some("Nobody may be connected to the database while it is renamed."),
        );
        let name = adw::EntryRow::builder().title("New Name").text(old).build();
        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
        list.append(&name);
        dialog.set_extra_child(Some(&list));
        dialog.add_responses(&[("cancel", "Cancel"), ("rename", "Rename")]);
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        dialog.set_response_enabled("rename", false);
        dialog.set_default_response(Some("rename"));
        dialog.set_close_response("cancel");
        let unchanged = old.to_string();
        enable_when(&dialog, "rename", &name, move |text| !text.is_empty() && text != unchanged);

        let (weak, old) = (Rc::downgrade(self), old.to_string());
        dialog.connect_response(None, move |_, response| {
            let Some(ws) = weak.upgrade() else { return };
            if response != "rename" {
                return;
            }
            let sql = db::rename_database_sql(ws.kind(), &old, name.text().trim());
            ws.run_statements(vec![sql], None, "Could not rename the database", |ws| ws.load_databases());
        });
        super::super::pointer_cursors(&dialog);
        dialog.present(Some(&self.widget));
    }

    fn confirm_drop_database(self: &Rc<Self>, name: &str) {
        let kind = self.kind();
        let dialog = adw::AlertDialog::new(
            Some(&format!("Delete database “{name}”?")),
            Some("Every table, view and row in it will be permanently removed. This cannot be undone."),
        );
        let options = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
        // Typing the name guards against deleting the wrong one of several similar databases.
        let confirm = adw::EntryRow::builder().title(format!("Type “{name}” to confirm")).build();
        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
        list.append(&confirm);
        options.append(&list);
        let force = (kind == DbKind::Postgres).then(|| {
            option_check("Disconnect other sessions", "Close every other connection to it first (PostgreSQL 13 or later)")
        });
        if let Some((_, row)) = &force {
            options.append(row);
        }
        dialog.set_extra_child(Some(&options));
        dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_response_enabled("delete", false);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let expected = name.to_string();
        enable_when(&dialog, "delete", &confirm, move |text| text == expected);

        let (weak, name) = (Rc::downgrade(self), name.to_string());
        dialog.connect_response(None, move |_, response| {
            let Some(ws) = weak.upgrade() else { return };
            if response != "delete" {
                return;
            }
            let force = force.as_ref().is_some_and(|(c, _)| c.is_active());
            let sql = db::drop_database_sql(ws.kind(), &name, force);
            ws.run_statements(vec![sql], None, "Could not delete the database", |ws| ws.load_databases());
        });
        super::super::pointer_cursors(&dialog);
        dialog.present(Some(&self.widget));
    }
}

/// Enable `response` only while the entry's trimmed text passes `valid`. Enter in the
/// entry activates the dialog's default response, when enabled.
fn enable_when(dialog: &adw::AlertDialog, response: &'static str, entry: &adw::EntryRow, valid: impl Fn(&str) -> bool + 'static) {
    entry.set_activates_default(true);
    let d = dialog.downgrade();
    entry.connect_changed(move |e| {
        if let Some(d) = d.upgrade() {
            d.set_response_enabled(response, valid(e.text().trim()));
        }
    });
}
