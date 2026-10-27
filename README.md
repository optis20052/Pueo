<div align="center">

<img src="data/icons/hicolor/scalable/apps/io.github.pueo.Pueo.svg" width="128" height="128" alt="Pueo logo">

# Pueo

A native database client for GNOME, written in Rust with GTK 4 and libadwaita.

Supports **PostgreSQL**, **MySQL / MariaDB**, **SQLite** and **Redis**.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/screenshots/connections-dark.png">
  <img src="docs/screenshots/connections.png" alt="Pueo home page with saved connections in groups" width="860">
</picture>

</div>

## Screenshots

| Table browser with pending edits and the row inspector | Query editor with results |
| --- | --- |
| ![Table browser](docs/screenshots/table-editing.png) | ![Query editor](docs/screenshots/query-editor.png) |

![Dark mode with the console open](docs/screenshots/dark-console.png)

## Features

**Home page**
- Connections in **groups** with custom icons: a symbol, an emoji or your own image (copied to
  `~/.config/pueo/icons`). Groups also have a colour and can be collapsed.
- **Coloured rows** and **environment tags** (LOCAL / DEV / TEST / STAGING / PROD); a tag colours the row when
  no colour is set
- **Drag and drop**: reorder connections, move them between groups, reorder groups
- Right-click menus: Connect, Edit, Duplicate (passwords included), Move to Group, Delete; New Connection Here,
  Edit and Delete for groups. Deleting a group keeps its connections.
- Search across names, hosts, types and tags; "Open SQLite File…" connects without saving

**Connections**
- Saved connections, with passwords stored in the desktop keyring (Secret Service / GNOME Keyring)
- **SSH tunnels** through the system `ssh` client, so `~/.ssh/config`, the agent and `known_hosts` all apply.
  Supports agent/default keys, password, or a specific private key with an optional passphrase.
- **Connection rail** on the left edge of the single window:
  - The home button shows your saved connections.
  - Each open connection gets a coloured badge with its name. Click it or press Ctrl+1-9 to switch; each keeps
    its own tabs and state.
  - Right-click a badge for New Query / Close Connection, or middle-click it to close.
  - Opening a connection that is already open switches to it.
- **Database switcher** in the sidebar: reconnects to another database on the same server
- **Manage databases** (PostgreSQL, MySQL) from the switcher: New Database…, and each row's ⋯ menu (or
  right-click) to rename (PostgreSQL), delete (type the name to confirm) or back up / restore it
- **Backup & restore** a whole database from the save button in the top bar: `pg_dump` (custom archive or
  plain SQL) / `pg_restore` / `psql`, `mysqldump` / `mysql` (or the MariaDB tools), through the SSH tunnel
  when the connection has one. SQLite uses its built-in backup API. Cancel stops the running tool.
- `pueo file.db` opens a SQLite file directly

**Browsing**
- Virtualized sidebar tree that stays fast with tens of thousands of objects:
  - collapsible **Functions**, **Tables** and **Views** sections with live counts (Redis keys grouped by type).
    Functions start closed; which sections are open is remembered per connection
  - tables and views expand to their columns, with a key icon on the primary key and the column type
  - functions and procedures (PostgreSQL, MySQL) open their `CREATE` statement in a query tab; overloads
    show their argument types
- Redis lists up to 50,000 keys. Beyond that a banner appears, and pressing Enter in the search box runs
  `SCAN MATCH` on the server.
- **Pin** tables and views to the top of their section (remembered per connection and database)
- Right-click a table or view to open it, export / import, clone, truncate (optionally cascading) or delete it
- Tabbed table browser:
  - 300 rows per page with an exact row count
  - filter bar with `column operator value` rules, or a raw SQL `WHERE` clause
  - click a column header to sort on the server
  - Data / Structure / Indexes views
  - **Follow foreign keys**: hold Ctrl to highlight foreign-key values, then Ctrl+click one to open the
    referenced table in a new tab, filtered to that row

**Row inspector** (Ctrl+Shift+I): a right-hand panel showing the selected row as an editable form, with
dropdowns for enum/boolean columns, multi-line fields for long text and JSON, and Set NULL per field.

**Loading effect**: while a connection is working (opening a table, committing, running a query, loading
columns) a faint grid of memory cells appears behind the sidebar header, lit by a slow scan wave in the
connection's colour with the odd bit flipping. It fades in and out and holds still when animations are
turned off in the system settings.

**Console** (Ctrl+Shift+C): a bottom panel logging every query Pueo sends to the server, with time,
duration and errors in red. Statements are syntax highlighted. Filter by All Queries, Hide Meta Queries (the
lookups Pueo makes by itself) or Your Queries Only. The history is kept per connection (last 5000 queries) and
the text can be copied.

**Editing** (tables with a primary key; Redis hash, list, set, zset and string values)
- Double-click a cell to edit it (multi-line values get a text area; "Set NULL" is available)
- Add rows (Ctrl+I), duplicate rows, and delete rows (Delete key toggles), all from the right-click menu too
- Changed cells turn yellow, new rows green and deleted rows red and struck through. A bar shows how many
  changes are pending.
- **Commit** (Ctrl+S) applies everything in one transaction. If anything fails, nothing is changed and your
  edits are kept. **Discard** throws them away.
- You're asked before paging, filtering, reloading or closing a tab would lose pending edits

**Query editor**
- GtkSourceView editor with SQL highlighting, line numbers and light/dark schemes
- Autocomplete of keywords, tables and columns, including `alias.column` (it appears as you type; Ctrl+Space
  opens it)
- Ctrl+Enter runs the selection or the statement under the cursor; Ctrl+Shift+Enter runs everything
- **Stop** cancels the running query on the server: PostgreSQL cancel request, MySQL `KILL QUERY`, SQLite
  interrupt
- **History** of every query per connection, searchable, plus **saved queries** (Ctrl+S in a query tab)
- The Redis console takes one command per line, and replies are shown as grids

**Import / export**
- Export a whole table (respecting the current filter and sort) or a query result as **CSV, JSON or SQL
  INSERTs**
- Import a CSV file with a header row into a table, in one transaction; empty fields become NULL

## Build & run

Needs Rust ≥ 1.88, GTK ≥ 4.18 and libadwaita ≥ 1.8. On Ubuntu 25.10 / 26.04:

```sh
sudo apt install libgtk-4-dev libadwaita-1-dev libgtksourceview-5-dev libssl-dev openssh-client
cargo run --release
```

Without GtkSourceView the editor falls back to a plain text view with built-in highlighting:

```sh
cargo run --release --no-default-features
```

To install for your user:

```sh
cargo build --release
install -Dm755 target/release/pueo ~/.local/bin/pueo
install -Dm644 data/io.github.pueo.Pueo.desktop ~/.local/share/applications/io.github.pueo.Pueo.desktop
install -Dm644 data/icons/hicolor/scalable/apps/io.github.pueo.Pueo.svg \
  ~/.local/share/icons/hicolor/scalable/apps/io.github.pueo.Pueo.svg
```

The icon is also bundled into the binary (`data/resources.gresource.xml`, compiled by `build.rs`), so the
window and About dialog show it without installing.

## Keyboard shortcuts

| Shortcut | Action |
| --- | --- |
| Ctrl+N | New connection |
| Ctrl+0 | Show saved connections |
| Ctrl+1 … Ctrl+9 | Switch to open connection 1-9 |
| Ctrl+Shift+W | Close the current connection |
| Ctrl+T | New query tab |
| Ctrl+W | Close tab |
| Ctrl+Enter | Run statement at cursor / selection |
| Ctrl+Shift+Enter | Run everything |
| Ctrl+. | Stop running query |
| Ctrl+Space | Autocomplete |
| Ctrl+S | Commit table changes / save query |
| Ctrl+I | Add row |
| Ctrl+Shift+I | Row inspector |
| Ctrl+Shift+C | Console |
| Delete | Mark selected rows for deletion |
| Ctrl+R / F5 | Reload table or re-run query |
| Ctrl+F / Ctrl+P | Filter the current table, or search the sidebar |
| Ctrl+C | Copy selected rows (TSV) |
| Enter | Show row details |
| F9 | Toggle sidebar |

## Where things are stored

| What | Where |
| --- | --- |
| Connections | `~/.config/pueo/connections.json` |
| Saved queries | `~/.config/pueo/saved_queries.json` |
| Sidebar sections open per connection | `~/.config/pueo/sidebar.json` |
| Pinned tables and views | `~/.config/pueo/pins.json` |
| Custom group images | `~/.config/pueo/icons/` |
| Query history | `~/.local/share/pueo/history.jsonl` (compacted automatically) |
| Passwords | Desktop keyring, labelled "Pueo: …" / "Pueo SSH: …" |

Pueo used to be called Tabula. On first start it moves `~/.config/tabula` and `~/.local/share/tabula` to the
new locations, and passwords saved under the old name are moved in the keyring the first time they are used.

## Architecture

```
src/
  main.rs              app setup, CSS, global actions and accelerators
  runtime.rs           Tokio runtime + spawn() that hands results back to the GTK main loop
  config.rs            saved connections, SSH settings
  secrets.rs           keyring access via oo7
  ssh.rs               `ssh -L` tunnels with an askpass helper for passwords
  library.rs           query history and saved queries
  export.rs            CSV / JSON / SQL export, CSV import
  filter.rs            visual filter rules turned into a SQL WHERE body
  db/
    mod.rs             Driver trait, RowChange, SQL generation for edits, tunnel and logging wrappers, integration tests
    log.rs             per-connection query log shown in the console
    postgres.rs        tokio-postgres (simple query protocol → every value arrives as text)
    mysql.rs           mysql_async (one session so USE/SET/transactions persist)
    sqlite.rs          rusqlite (bundled) on spawn_blocking
    redis.rs           redis-rs multiplexed connection
  ui/
    main_window.rs     the single window: connection rail, pages, shortcut forwarding
    welcome.rs         saved-connection list (home page)
    connection_dialog.rs
    group_dialog.rs    create / edit a group: name, colour, icon
    icons.rs           group icons and the colour-swatch picker
    picker.rs          searchable popover for picking a database or schema
    workspace.rs       per-connection page: sidebar, database switcher, AdwTabView
    workspace/
      object_menu.rs   right-click menu for tables and views
    sidebar.rs         object tree: sections, columns, functions
    console.rs         bottom panel listing every query sent to the server
    loading_fx.rs      animated header background while queries run
    table_tab.rs       table browser, editing, commit, import/export
    filter_bar.rs      filter bar: column / operator / value rows or raw SQL
    row_inspector.rs   selected row as an editable form
    query_tab.rs       editor, results, history & saved queries
    data_grid.rs       editable GtkColumnView grid
    editor.rs          GtkSourceView / TextView editor, statement splitting
    completion.rs      autocomplete popover
    highlight.rs       fallback TextTag highlighter
```

All database I/O runs on a Tokio runtime. `runtime::spawn(future, callback)` runs the future on Tokio and calls
the callback on the GTK thread, so the UI never blocks. Every backend implements the same `Driver` trait, so
adding a database only means adding one file in `src/db/`.

## Tests

```sh
cargo test          # unit tests + SQLite integration test
```

Integration tests against live servers (`host:port:user:password`; the MySQL test expects a database named
`shop`):

```sh
PUEO_PG=localhost:5432:postgres:secret \
PUEO_MY=127.0.0.1:3306:root:secret \
PUEO_RD=localhost:6379::secret \
cargo test -- --include-ignored
```

The SSH test uses a bastion host with a PostgreSQL server behind it (user `postgres`, password `secret`):

```sh
PUEO_SSH='bastion:22:user:password:db-host:5432' \
PUEO_SSH_KEY='/path/to/key:passphrase' \
PUEO_SSH_OPTIONS='UserKnownHostsFile=/tmp/known_hosts' \
cargo test ssh_tunnel -- --include-ignored
```

`PUEO_SSH_OPTIONS` adds extra `-o` options to every tunnel, which is also handy for debugging.

## Roadmap

- Structure editing (add/alter columns, indexes) and a "copy as SQL" preview of pending changes
- Export/import for very large tables without holding all rows in memory
- More TLS options (CA file, client certificates, verify-full)
- More engines: MSSQL (tiberius), ClickHouse, MongoDB
