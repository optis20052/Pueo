mod backup;
mod config;
mod db;
mod export;
mod filter;
mod library;
mod runtime;
mod secrets;
mod ssh;
mod ui;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

pub const APP_ID: &str = "io.github.pueo.Pueo";

fn main() -> glib::ExitCode {
    // The app icon. GtkApplication adds /io/github/pueo/Pueo/icons to the icon theme.
    gio::resources_register_include!("pueo.gresource").expect("bundled resources");
    // Carry over settings saved under the app's old name.
    config::migrate_from_tabula();
    // Start the database runtime before any window needs it.
    runtime::runtime();

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    app.connect_startup(|app| {
        load_css();
        gtk::Window::set_default_icon_name(APP_ID);
        setup_app_actions(app);
    });
    app.connect_activate(|app| ui::main_window(app).present());
    // `pueo some.db` opens SQLite files directly.
    app.connect_open(|app, files, _| {
        for file in files {
            let Some(path) = file.path() else { continue };
            let mut cfg = config::ConnectionConfig::new(db::DbKind::Sqlite);
            cfg.file_path = path.to_string_lossy().into_owned();
            let win = ui::main_window(app);
            win.present();
            win.open_connection(cfg, Box::new(|| {}));
        }
    });
    app.run()
}

fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("ui/style.css"));
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("no display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn setup_app_actions(app: &adw::Application) {
    let quit = gio::SimpleAction::new("quit", None);
    let a = app.downgrade();
    quit.connect_activate(move |_, _| {
        if let Some(app) = a.upgrade() {
            app.quit();
        }
    });
    app.add_action(&quit);

    let about = gio::SimpleAction::new("about", None);
    let a = app.downgrade();
    about.connect_activate(move |_, _| {
        let Some(app) = a.upgrade() else { return };
        let dialog = adw::AboutDialog::builder()
            .application_name("Pueo")
            .application_icon(APP_ID)
            .version(env!("CARGO_PKG_VERSION"))
            .comments("A native database client for GNOME")
            .license_type(gtk::License::MitX11)
            .build();
        dialog.present(app.active_window().as_ref());
    });
    app.add_action(&about);

    let shortcuts = gio::SimpleAction::new("shortcuts", None);
    let a = app.downgrade();
    shortcuts.connect_activate(move |_, _| {
        let Some(app) = a.upgrade() else { return };
        let dialog = adw::ShortcutsDialog::new();
        let sections: [(&str, &[(&str, &str)]); 3] = [
            ("General", &[
                ("New connection", "<Control>n"),
                ("Show connections", "<Control>0"),
                ("Switch to connection 1-9", "<Control>1"),
                ("Close connection", "<Control><Shift>w"),
                ("New query tab", "<Control>t"),
                ("Close tab", "<Control>w"),
                ("Toggle sidebar", "F9"),
                ("Quit", "<Control>q"),
            ]),
            ("Queries", &[
                ("Run statement at cursor / selection", "<Control>Return"),
                ("Run everything", "<Control><Shift>Return"),
                ("Stop running query", "<Control>period"),
                ("Autocomplete", "<Control>space"),
                ("Save query", "<Control>s"),
            ]),
            ("Data", &[
                ("Reload", "<Control>r"),
                ("Filter / search", "<Control>f"),
                ("Add row", "<Control>i"),
                ("Row inspector", "<Control><Shift>i"),
                ("Console", "<Control><Shift>c"),
                ("Delete selected rows", "Delete"),
                ("Commit changes", "<Control>s"),
                ("Copy selected rows", "<Control>c"),
                ("Show row details", "Return"),
            ]),
        ];
        for (title, items) in sections {
            let section = adw::ShortcutsSection::new(Some(title));
            for (label, accel) in items {
                section.add(adw::ShortcutsItem::new(label, accel));
            }
            dialog.add(section);
        }
        dialog.present(app.active_window().as_ref());
    });
    app.add_action(&shortcuts);

    for (action, accels) in [
        ("app.quit", &["<Control>q"][..]),
        ("win.new-connection", &["<Control>n"]),
        ("win.new-query", &["<Control>t"]),
        ("win.close-tab", &["<Control>w"]),
        ("win.refresh", &["<Control>r", "F5"]),
        ("win.find", &["<Control>f", "<Control>p"]),
        ("win.toggle-sidebar", &["F9"]),
        ("win.save", &["<Control>s"]),
        ("win.insert-row", &["<Control>i"]),
        ("win.stop", &["<Control>period"]),
        ("win.close-connection", &["<Control><Shift>w"]),
        ("win.toggle-inspector", &["<Control><Shift>i"]),
        ("win.toggle-console", &["<Control><Shift>c"]),
    ] {
        app.set_accels_for_action(action, accels);
    }
    for n in 0..=9 {
        app.set_accels_for_action(&format!("win.switch-connection({n})"), &[&format!("<Control>{n}")]);
    }
}
