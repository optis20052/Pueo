//! Backing up a whole database to a file and restoring one, with a progress dialog
//! that can cancel the running tool.

use std::cell::Cell;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::{gio, glib};
use tokio::sync::Notify;

use super::object_menu::option_check;
use super::{Workspace, credentials};
use crate::backup::{self, BackupOptions, Format, RestoreOptions};
use crate::db::DbKind;
use crate::runtime;

impl Workspace {
    /// Back up `database`: options (format, structure only), then where to save it.
    pub(super) fn start_backup(self: &Rc<Self>, database: String) {
        let kind = self.kind();
        if kind == DbKind::Mysql && database.is_empty() {
            self.show_error("No Database Selected", "Switch to a database first, then back it up.");
            return;
        }
        if kind == DbKind::Sqlite {
            self.choose_backup_file(database, BackupOptions { format: Format::File, schema_only: false });
            return;
        }
        let tool = if kind == DbKind::Postgres { "pg_dump" } else { "mysqldump" };
        let dialog = adw::AlertDialog::new(
            Some(&format!("Back Up “{database}”")),
            Some(&format!("Save the whole database to a file with {tool}.")),
        );
        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
        let format = (kind == DbKind::Postgres).then(|| {
            let row = adw::ComboRow::builder()
                .title("Format")
                .model(&gtk::StringList::new(&["Custom archive (.dump)", "Plain SQL (.sql)"]))
                .build();
            list.append(&row);
            row
        });
        let schema_only = adw::SwitchRow::builder().title("Structure Only").subtitle("Leave out the rows").build();
        list.append(&schema_only);
        dialog.set_extra_child(Some(&list));
        dialog.add_responses(&[("cancel", "Cancel"), ("continue", "Choose File…")]);
        dialog.set_response_appearance("continue", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("continue"));
        dialog.set_close_response("cancel");

        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            let Some(ws) = weak.upgrade() else { return };
            if response != "continue" {
                return;
            }
            let format = match &format {
                Some(row) if row.selected() == 0 => Format::Custom,
                _ => Format::Sql,
            };
            ws.choose_backup_file(database.clone(), BackupOptions { format, schema_only: schema_only.is_active() });
        });
        super::super::pointer_cursors(&dialog);
        dialog.present(Some(&self.widget));
    }

    fn choose_backup_file(self: &Rc<Self>, database: String, opts: BackupOptions) {
        let stem = match self.kind() {
            DbKind::Sqlite => {
                let path = PathBuf::from(&self.config.borrow().file_path);
                path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "database".into())
            }
            _ => database.clone(),
        };
        let stamp = glib::DateTime::now_local()
            .and_then(|t| t.format("%Y-%m-%d-%H%M"))
            .map(|s| s.to_string())
            .unwrap_or_default();
        let dialog = gtk::FileDialog::builder()
            .title("Save Backup")
            .initial_name(format!("{}-{stamp}.{}", stem.replace(['/', ':'], "_"), opts.format.extension()))
            .build();
        let weak = Rc::downgrade(self);
        dialog.save(self.window().as_ref(), None::<&gio::Cancellable>, move |res| {
            if let (Some(ws), Ok(file)) = (weak.upgrade(), res)
                && let Some(path) = file.path()
            {
                ws.run_backup(database.clone(), path, opts);
            }
        });
    }

    fn run_backup(self: &Rc<Self>, database: String, path: PathBuf, opts: BackupOptions) {
        let cfg = self.config();
        let shown = path.display().to_string();
        let label = if cfg.kind == DbKind::Sqlite { shown.clone() } else { format!("“{database}”") };
        self.run_with_progress(
            "Backing Up…",
            &format!("Saving {label}. Large databases can take a while."),
            move |cancel| async move {
                let (password, ssh_secret) = credentials(&cfg).await;
                backup::backup(&cfg, password, ssh_secret, &database, &path, opts, cancel).await
            },
            move |ws| ws.show_message("Backup Complete", &format!("Saved to {shown}.")),
            "Backup Failed",
        );
    }

    /// Restore a backup file into `database`: pick the file, then confirm.
    pub(super) fn start_restore(self: &Rc<Self>, database: String) {
        let kind = self.kind();
        if kind == DbKind::Mysql && database.is_empty() {
            self.show_error("No Database Selected", "Switch to the database to restore into first.");
            return;
        }
        if database == self.current_database() && self.has_changes() {
            self.show_error("Unsaved Changes", "Commit or discard pending changes before restoring.");
            return;
        }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        let backups = gtk::FileFilter::new();
        backups.set_name(Some("Backups"));
        let patterns: &[&str] = match kind {
            DbKind::Sqlite => &["*.db", "*.sqlite", "*.sqlite3", "*.backup"],
            DbKind::Postgres => &["*.dump", "*.sql", "*.backup"],
            _ => &["*.sql"],
        };
        for p in patterns {
            backups.add_pattern(p);
        }
        filters.append(&backups);
        let all = gtk::FileFilter::new();
        all.set_name(Some("All Files"));
        all.add_pattern("*");
        filters.append(&all);
        let dialog = gtk::FileDialog::builder().title("Restore Backup").filters(&filters).build();
        let weak = Rc::downgrade(self);
        dialog.open(self.window().as_ref(), None::<&gio::Cancellable>, move |res| {
            if let (Some(ws), Ok(file)) = (weak.upgrade(), res)
                && let Some(path) = file.path()
            {
                ws.confirm_restore(database.clone(), path);
            }
        });
    }

    fn confirm_restore(self: &Rc<Self>, database: String, path: PathBuf) {
        let kind = self.kind();
        let format = match Format::detect(kind, &path) {
            Ok(f) => f,
            Err(e) => return self.show_error("Can’t Restore This File", &format!("{e:#}")),
        };
        let file_name = file_name(&path);
        let (heading, body) = match kind {
            DbKind::Sqlite => (
                "Restore this database?".to_string(),
                format!("Its contents will be replaced by “{file_name}”. This cannot be undone."),
            ),
            _ => (
                format!("Restore into “{database}”?"),
                format!(
                    "The tables, rows and functions in “{file_name}” are added to this database. \
                     Objects that already exist may clash or be changed. This cannot be undone."
                ),
            ),
        };
        let dialog = adw::AlertDialog::new(Some(&heading), Some(&body));
        let clean = (format == Format::Custom).then(|| {
            option_check("Clean", "Drop each object in the backup before recreating it, replacing what is there")
        });
        if let Some((_, row)) = &clean {
            dialog.set_extra_child(Some(row));
        }
        dialog.add_responses(&[("cancel", "Cancel"), ("restore", "Restore")]);
        dialog.set_response_appearance("restore", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            let Some(ws) = weak.upgrade() else { return };
            if response != "restore" {
                return;
            }
            let opts = RestoreOptions { clean: clean.as_ref().is_some_and(|(c, _)| c.is_active()) };
            ws.run_restore(database.clone(), path.clone(), opts);
        });
        super::super::pointer_cursors(&dialog);
        dialog.present(Some(&self.widget));
    }

    fn run_restore(self: &Rc<Self>, database: String, path: PathBuf, opts: RestoreOptions) {
        let cfg = self.config();
        let is_current = database == self.current_database();
        let file_name = file_name(&path);
        self.run_with_progress(
            "Restoring…",
            &format!("Loading “{file_name}”. Large backups can take a while."),
            move |cancel| async move {
                let (password, ssh_secret) = credentials(&cfg).await;
                backup::restore(&cfg, password, ssh_secret, &database, &path, opts, cancel).await
            },
            move |ws| {
                if is_current {
                    ws.load_objects(None);
                    ws.reload_tables(|_| true);
                }
                ws.show_message("Restore Complete", &format!("“{file_name}” was restored."));
            },
            "Restore Failed",
        );
    }

    // ---------- running ----------

    /// Run `task` on the runtime behind a dialog whose Cancel stops it.
    fn run_with_progress<F, Fut>(
        self: &Rc<Self>,
        heading: &str,
        body: &str,
        task: F,
        done: impl FnOnce(&Rc<Self>) + 'static,
        failure: &'static str,
    ) where
        F: FnOnce(Arc<Notify>) -> Fut,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let cancel = Arc::new(Notify::new());
        let cancelled = Rc::new(Cell::new(false));
        let finished = Rc::new(Cell::new(false));
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.set_extra_child(Some(
            &adw::Spinner::builder().width_request(32).height_request(32).halign(gtk::Align::Center).build(),
        ));
        dialog.add_response("cancel", "Cancel");
        dialog.set_close_response("cancel");
        let (c, flag, fin) = (cancel.clone(), cancelled.clone(), finished.clone());
        dialog.connect_response(None, move |_, _| {
            // Closing the dialog at the end also answers "cancel".
            if !fin.get() {
                flag.set(true);
                c.notify_one();
            }
        });
        super::super::pointer_cursors(&dialog);
        dialog.present(Some(&self.widget));

        let weak = Rc::downgrade(self);
        runtime::spawn(task(cancel), move |res| {
            finished.set(true);
            if !cancelled.get() {
                dialog.force_close();
            }
            let Some(ws) = weak.upgrade() else { return };
            match res {
                Ok(()) => done(&ws),
                Err(_) if cancelled.get() => {}
                Err(e) => ws.show_error(failure, &format!("{e:#}")),
            }
        });
    }

    fn show_message(&self, heading: &str, body: &str) {
        let dialog = adw::AlertDialog::new(Some(heading), Some(body));
        dialog.add_response("ok", "OK");
        dialog.present(Some(&self.widget));
    }

    fn window(&self) -> Option<gtk::Window> {
        self.widget.root().and_downcast::<gtk::Window>()
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}
