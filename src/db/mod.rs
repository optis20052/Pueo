pub mod log;
mod mysql;
mod postgres;
mod redis;
mod sqlite;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::ConnectionConfig;
use crate::ssh;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DbKind {
    Postgres,
    Mysql,
    Sqlite,
    Redis,
}

impl DbKind {
    pub const ALL: [DbKind; 4] = [DbKind::Postgres, DbKind::Mysql, DbKind::Sqlite, DbKind::Redis];

    pub fn label(self) -> &'static str {
        match self {
            DbKind::Postgres => "PostgreSQL",
            DbKind::Mysql => "MySQL / MariaDB",
            DbKind::Sqlite => "SQLite",
            DbKind::Redis => "Redis",
        }
    }

    /// The database's logo, a bundled symbolic icon (see data/resources.gresource.xml).
    pub fn icon_name(self) -> &'static str {
        match self {
            DbKind::Postgres => "pueo-postgresql-symbolic",
            DbKind::Mysql => "pueo-mysql-symbolic",
            DbKind::Sqlite => "pueo-sqlite-symbolic",
            DbKind::Redis => "pueo-redis-symbolic",
        }
    }

    pub fn css_class(self) -> &'static str {
        match self {
            DbKind::Postgres => "badge-pg",
            DbKind::Mysql => "badge-my",
            DbKind::Sqlite => "badge-sl",
            DbKind::Redis => "badge-rd",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            DbKind::Postgres => 5432,
            DbKind::Mysql => 3306,
            DbKind::Sqlite => 0,
            DbKind::Redis => 6379,
        }
    }

    pub fn is_sql(self) -> bool {
        self != DbKind::Redis
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ObjectKind {
    #[default]
    Table,
    View,
    Key,
    Function,
    Procedure,
}

impl ObjectKind {
    pub fn icon_name(self) -> &'static str {
        match self {
            ObjectKind::Table => "view-grid-symbolic",
            ObjectKind::View => "view-reveal-symbolic",
            ObjectKind::Key => "dialog-password-symbolic",
            ObjectKind::Function | ObjectKind::Procedure => "pueo-function-symbolic",
        }
    }

    /// Functions and procedures: opened as source code rather than browsed as rows.
    pub fn is_routine(self) -> bool {
        matches!(self, ObjectKind::Function | ObjectKind::Procedure)
    }
}

/// A browsable object in the sidebar: a table, a view, a function or (for Redis) a key.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DbObject {
    /// Schema / database name. For Redis this is the key type.
    pub schema: String,
    pub name: String,
    pub kind: ObjectKind,
    /// Argument types of a function ("uuid, text"), which tell overloads apart. Empty otherwise.
    pub signature: String,
}

#[derive(Debug, Clone, Default)]
pub struct ObjectList {
    pub objects: Vec<DbObject>,
    /// More objects exist than were listed (Redis key limit).
    pub truncated: bool,
}

#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    pub limit: usize,
    pub offset: usize,
    /// Raw `WHERE` clause body typed by the user.
    pub filter: Option<String>,
    /// Column name and ascending flag.
    pub order_by: Option<(String, bool)>,
}

pub type Cell = Option<String>;

#[derive(Debug, Clone, Default)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    /// Rows affected for statements that do not return rows.
    pub affected: Option<u64>,
    pub elapsed: Duration,
}

impl QueryResult {
    pub fn has_rows(&self) -> bool {
        !self.columns.is_empty()
    }

    /// Build a result from string-ish rows; handy for metadata queries.
    pub fn from_rows(columns: &[&str], rows: Vec<Vec<Cell>>) -> Self {
        QueryResult {
            columns: columns.iter().map(|c| c.to_string()).collect(),
            rows,
            ..Default::default()
        }
    }
}

/// A column whose values come from a fixed list (enum, boolean, CHECK ... IN).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnChoices {
    pub values: Vec<String>,
    pub nullable: bool,
}

pub type Choices = std::collections::HashMap<String, ColumnChoices>;

/// A pending edit made in the data grid. Values are `(column, value)` pairs.
#[derive(Debug, Clone, PartialEq)]
pub enum RowChange {
    Insert { values: Vec<(String, Cell)> },
    /// `key` identifies the row (original primary-key values), `set` holds the changed columns
    /// and `row` the complete new row.
    Update { key: Vec<(String, Cell)>, set: Vec<(String, Cell)>, row: Vec<(String, Cell)> },
    Delete { key: Vec<(String, Cell)> },
}

#[async_trait]
pub trait Driver: Send + Sync {
    fn kind(&self) -> DbKind;
    /// Human-readable description of what we are connected to (server version etc).
    async fn server_info(&self) -> Result<String>;
    /// `pattern` narrows the listing server-side where that matters (Redis `SCAN MATCH`).
    async fn list_objects(&self, pattern: Option<&str>) -> Result<ObjectList>;
    async fn fetch_rows(&self, obj: &DbObject, opts: &FetchOptions) -> Result<QueryResult>;
    async fn count_rows(&self, obj: &DbObject, filter: Option<&str>) -> Result<u64>;
    async fn structure(&self, obj: &DbObject) -> Result<QueryResult>;
    /// Execute arbitrary user input; one result per statement.
    async fn execute(&self, sql: &str) -> Result<Vec<QueryResult>>;
    /// Databases that can be switched to. Empty when switching is not supported.
    async fn list_databases(&self) -> Result<Vec<String>>;
    /// `(table, column)` pairs used for autocompletion.
    async fn list_columns(&self) -> Result<Vec<(String, String)>>;
    /// Columns identifying a row, or `None` when the object is read-only.
    async fn primary_key(&self, obj: &DbObject) -> Result<Option<Vec<String>>>;
    /// Apply grid edits atomically; returns the number of affected rows.
    async fn apply_changes(&self, obj: &DbObject, changes: &[RowChange]) -> Result<u64>;
    /// Ask the server to abort the statement currently running on this connection.
    async fn cancel(&self) -> Result<()>;
    /// Indexes of a table: name, columns, unique, primary, type, definition.
    async fn indexes(&self, _obj: &DbObject) -> Result<QueryResult> {
        Ok(QueryResult::default())
    }
    /// Columns of `obj` limited to a fixed set of values, for dropdown editing.
    async fn column_choices(&self, _obj: &DbObject) -> Result<Choices> {
        Ok(Choices::new())
    }
    /// The `CREATE` statement of a function or procedure.
    async fn definition(&self, _obj: &DbObject) -> Result<String> {
        anyhow::bail!("This database has no stored functions")
    }
}

pub type SharedDriver = Arc<dyn Driver>;

pub async fn connect(
    cfg: &ConnectionConfig,
    password: Option<String>,
    ssh_secret: Option<String>,
) -> Result<SharedDriver> {
    if cfg.ssh.enabled && cfg.kind != DbKind::Sqlite {
        let tunnel = ssh::open(&cfg.ssh, ssh_secret, &cfg.host, cfg.port).await?;
        let mut local = cfg.clone();
        local.host = "127.0.0.1".into();
        local.port = tunnel.local_port;
        let inner = connect_direct(&local, password).await?;
        return Ok(Arc::new(Tunneled { inner, _tunnel: tunnel }));
    }
    connect_direct(cfg, password).await
}

async fn connect_direct(cfg: &ConnectionConfig, password: Option<String>) -> Result<SharedDriver> {
    let driver: SharedDriver = match cfg.kind {
        DbKind::Postgres => Arc::new(postgres::PgDriver::connect(cfg, password).await?),
        DbKind::Mysql => Arc::new(mysql::MyDriver::connect(cfg, password).await?),
        DbKind::Sqlite => Arc::new(sqlite::SqliteDriver::connect(cfg).await?),
        DbKind::Redis => Arc::new(redis::RedisDriver::connect(cfg, password).await?),
    };
    Ok(driver)
}

/// A driver whose connection goes through an SSH tunnel that must stay open as long as it.
struct Tunneled {
    inner: SharedDriver,
    _tunnel: ssh::Tunnel,
}

#[async_trait]
impl Driver for Tunneled {
    fn kind(&self) -> DbKind {
        self.inner.kind()
    }
    async fn server_info(&self) -> Result<String> {
        Ok(format!("{} · SSH", self.inner.server_info().await?))
    }
    async fn list_objects(&self, pattern: Option<&str>) -> Result<ObjectList> {
        self.inner.list_objects(pattern).await
    }
    async fn fetch_rows(&self, obj: &DbObject, opts: &FetchOptions) -> Result<QueryResult> {
        self.inner.fetch_rows(obj, opts).await
    }
    async fn count_rows(&self, obj: &DbObject, filter: Option<&str>) -> Result<u64> {
        self.inner.count_rows(obj, filter).await
    }
    async fn structure(&self, obj: &DbObject) -> Result<QueryResult> {
        self.inner.structure(obj).await
    }
    async fn execute(&self, sql: &str) -> Result<Vec<QueryResult>> {
        self.inner.execute(sql).await
    }
    async fn list_databases(&self) -> Result<Vec<String>> {
        self.inner.list_databases().await
    }
    async fn list_columns(&self) -> Result<Vec<(String, String)>> {
        self.inner.list_columns().await
    }
    async fn primary_key(&self, obj: &DbObject) -> Result<Option<Vec<String>>> {
        self.inner.primary_key(obj).await
    }
    async fn apply_changes(&self, obj: &DbObject, changes: &[RowChange]) -> Result<u64> {
        self.inner.apply_changes(obj, changes).await
    }
    async fn cancel(&self) -> Result<()> {
        self.inner.cancel().await
    }
    async fn column_choices(&self, obj: &DbObject) -> Result<Choices> {
        self.inner.column_choices(obj).await
    }
    async fn indexes(&self, obj: &DbObject) -> Result<QueryResult> {
        self.inner.indexes(obj).await
    }
    async fn definition(&self, obj: &DbObject) -> Result<String> {
        self.inner.definition(obj).await
    }
}

/// Record everything `driver` sends to the server into `log` (the Console panel).
pub fn logged(driver: SharedDriver, log: Arc<log::QueryLog>) -> SharedDriver {
    Arc::new(Logged { inner: driver, log })
}

/// Tags each call with its kind, so drivers record their queries into the right place.
struct Logged {
    inner: SharedDriver,
    log: Arc<log::QueryLog>,
}

impl Logged {
    fn sink(&self, kind: log::LogKind) -> log::Sink {
        log::Sink { log: self.log.clone(), kind }
    }
}

#[async_trait]
impl Driver for Logged {
    fn kind(&self) -> DbKind {
        self.inner.kind()
    }
    async fn server_info(&self) -> Result<String> {
        let _busy = self.log.busy();
        log::scoped(self.sink(log::LogKind::Meta), self.inner.server_info()).await
    }
    async fn list_objects(&self, pattern: Option<&str>) -> Result<ObjectList> {
        let _busy = self.log.busy();
        log::scoped(self.sink(log::LogKind::Meta), self.inner.list_objects(pattern)).await
    }
    async fn fetch_rows(&self, obj: &DbObject, opts: &FetchOptions) -> Result<QueryResult> {
        let _busy = self.log.busy();
        log::scoped(self.sink(log::LogKind::Data), self.inner.fetch_rows(obj, opts)).await
    }
    async fn count_rows(&self, obj: &DbObject, filter: Option<&str>) -> Result<u64> {
        let _busy = self.log.busy();
        log::scoped(self.sink(log::LogKind::Data), self.inner.count_rows(obj, filter)).await
    }
    async fn structure(&self, obj: &DbObject) -> Result<QueryResult> {
        let _busy = self.log.busy();
        log::scoped(self.sink(log::LogKind::Meta), self.inner.structure(obj)).await
    }
    async fn execute(&self, sql: &str) -> Result<Vec<QueryResult>> {
        let _busy = self.log.busy();
        log::scoped(self.sink(log::LogKind::User), self.inner.execute(sql)).await
    }
    async fn list_databases(&self) -> Result<Vec<String>> {
        let _busy = self.log.busy();
        log::scoped(self.sink(log::LogKind::Meta), self.inner.list_databases()).await
    }
    async fn list_columns(&self) -> Result<Vec<(String, String)>> {
        let _busy = self.log.busy();
        log::scoped(self.sink(log::LogKind::Meta), self.inner.list_columns()).await
    }
    async fn primary_key(&self, obj: &DbObject) -> Result<Option<Vec<String>>> {
        let _busy = self.log.busy();
        log::scoped(self.sink(log::LogKind::Meta), self.inner.primary_key(obj)).await
    }
    async fn apply_changes(&self, obj: &DbObject, changes: &[RowChange]) -> Result<u64> {
        let _busy = self.log.busy();
        log::scoped(self.sink(log::LogKind::Data), self.inner.apply_changes(obj, changes)).await
    }
    async fn cancel(&self) -> Result<()> {
        self.inner.cancel().await
    }
    async fn column_choices(&self, obj: &DbObject) -> Result<Choices> {
        let _busy = self.log.busy();
        log::scoped(self.sink(log::LogKind::Meta), self.inner.column_choices(obj)).await
    }
    async fn indexes(&self, obj: &DbObject) -> Result<QueryResult> {
        let _busy = self.log.busy();
        log::scoped(self.sink(log::LogKind::Meta), self.inner.indexes(obj)).await
    }
    async fn definition(&self, obj: &DbObject) -> Result<String> {
        let _busy = self.log.busy();
        log::scoped(self.sink(log::LogKind::Meta), self.inner.definition(obj)).await
    }
}

// ---------- SQL helpers shared by the SQL drivers ----------

/// Build `WHERE ... ORDER BY ... LIMIT ... OFFSET ...` for SQL dialects.
pub(crate) fn select_tail(opts: &FetchOptions, quote: impl Fn(&str) -> String) -> String {
    let mut sql = String::new();
    if let Some(f) = opts.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        sql.push_str(" WHERE ");
        sql.push_str(f);
    }
    if let Some((col, asc)) = &opts.order_by {
        sql.push_str(&format!(" ORDER BY {} {}", quote(col), if *asc { "ASC" } else { "DESC" }));
    }
    sql.push_str(&format!(" LIMIT {} OFFSET {}", opts.limit, opts.offset));
    sql
}

pub(crate) fn where_clause(filter: Option<&str>) -> String {
    match filter.map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => format!(" WHERE {f}"),
        None => String::new(),
    }
}

pub(crate) fn quote_double(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

pub(crate) fn quote_backtick(ident: &str) -> String {
    format!("`{}`", ident.replace('`', "``"))
}

pub(crate) fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub fn quote_ident(kind: DbKind, ident: &str) -> String {
    match kind {
        DbKind::Mysql => quote_backtick(ident),
        _ => quote_double(ident),
    }
}

/// A SQL literal for a cell. Text literals are coerced to the column type by every engine.
pub fn quote_value(kind: DbKind, cell: &Cell) -> String {
    match cell {
        None => "NULL".into(),
        // MySQL treats backslashes in string literals as escapes by default.
        Some(v) if kind == DbKind::Mysql => quote_literal(&v.replace('\\', "\\\\")),
        Some(v) => quote_literal(v),
    }
}

pub fn qualified_name(kind: DbKind, obj: &DbObject) -> String {
    format!("{}.{}", quote_ident(kind, &obj.schema), quote_ident(kind, &obj.name))
}

fn key_condition(kind: DbKind, key: &[(String, Cell)]) -> String {
    key.iter()
        .map(|(col, v)| match v {
            None => format!("{} IS NULL", quote_ident(kind, col)),
            Some(_) => format!("{} = {}", quote_ident(kind, col), quote_value(kind, v)),
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

pub fn insert_sql(kind: DbKind, table: &str, values: &[(String, Cell)]) -> String {
    if values.is_empty() {
        return match kind {
            DbKind::Mysql => format!("INSERT INTO {table} () VALUES ()"),
            _ => format!("INSERT INTO {table} DEFAULT VALUES"),
        };
    }
    format!(
        "INSERT INTO {table} ({}) VALUES ({})",
        values.iter().map(|(c, _)| quote_ident(kind, c)).collect::<Vec<_>>().join(", "),
        values.iter().map(|(_, v)| quote_value(kind, v)).collect::<Vec<_>>().join(", ")
    )
}

/// Empty a table: the statements, and one to run afterwards even if they fail.
/// `cascade`: also empty tables referencing it (PostgreSQL), or skip foreign key
/// checks (MySQL, which has no TRUNCATE … CASCADE). `restart_identity` (PostgreSQL):
/// reset owned sequences; MySQL always resets AUTO_INCREMENT on TRUNCATE.
pub fn truncate_sql(kind: DbKind, obj: &DbObject, cascade: bool, restart_identity: bool) -> (Vec<String>, Option<String>) {
    let table = qualified_name(kind, obj);
    match kind {
        DbKind::Postgres => {
            let mut sql = format!("TRUNCATE TABLE {table}");
            if restart_identity {
                sql.push_str(" RESTART IDENTITY");
            }
            if cascade {
                sql.push_str(" CASCADE");
            }
            (vec![sql], None)
        }
        DbKind::Mysql if cascade => (
            vec!["SET FOREIGN_KEY_CHECKS = 0".into(), format!("TRUNCATE TABLE {table}")],
            Some("SET FOREIGN_KEY_CHECKS = 1".into()),
        ),
        DbKind::Mysql => (vec![format!("TRUNCATE TABLE {table}")], None),
        // SQLite has no TRUNCATE; deleting every row is its equivalent.
        _ => (vec![format!("DELETE FROM {table}")], None),
    }
}

/// Drop a table or view. `cascade` (PostgreSQL) also drops what depends on it.
pub fn drop_sql(kind: DbKind, obj: &DbObject, cascade: bool) -> String {
    let what = if obj.kind == ObjectKind::View { "VIEW" } else { "TABLE" };
    let mut sql = format!("DROP {what} {}", qualified_name(kind, obj));
    if cascade && kind == DbKind::Postgres {
        sql.push_str(" CASCADE");
    }
    sql
}

/// Copy a table's structure (and optionally its rows) to `new_name` in the same schema.
pub fn clone_sql(kind: DbKind, obj: &DbObject, new_name: &str, with_data: bool) -> Vec<String> {
    let source = qualified_name(kind, obj);
    let target = qualified_name(kind, &DbObject { name: new_name.to_string(), ..obj.clone() });
    let mut stmts = match kind {
        DbKind::Postgres => vec![format!("CREATE TABLE {target} (LIKE {source} INCLUDING ALL)")],
        DbKind::Mysql => vec![format!("CREATE TABLE {target} LIKE {source}")],
        // SQLite: columns and types only (no constraints), so copy the data in the same statement.
        _ => {
            let filter = if with_data { "" } else { " WHERE 0" };
            return vec![format!("CREATE TABLE {target} AS SELECT * FROM {source}{filter}")];
        }
    };
    if with_data {
        stmts.push(format!("INSERT INTO {target} SELECT * FROM {source}"));
    }
    stmts
}

/// Translate grid edits into SQL statements.
pub fn change_statements(kind: DbKind, obj: &DbObject, changes: &[RowChange]) -> anyhow::Result<Vec<String>> {
    let table = qualified_name(kind, obj);
    changes
        .iter()
        .map(|change| {
            Ok(match change {
                RowChange::Insert { values } => insert_sql(kind, &table, values),
                RowChange::Update { key, set, .. } => {
                    anyhow::ensure!(!key.is_empty(), "Cannot update a row without a primary key");
                    format!(
                        "UPDATE {table} SET {} WHERE {}",
                        set.iter()
                            .map(|(c, v)| format!("{} = {}", quote_ident(kind, c), quote_value(kind, v)))
                            .collect::<Vec<_>>()
                            .join(", "),
                        key_condition(kind, key)
                    )
                }
                RowChange::Delete { key } => {
                    anyhow::ensure!(!key.is_empty(), "Cannot delete a row without a primary key");
                    format!("DELETE FROM {table} WHERE {}", key_condition(kind, key))
                }
            })
        })
        .collect()
}

/// Parse `'a', 'b', 'it''s'` into ["a", "b", "it's"] (also accepts "double" quotes).
pub(crate) fn parse_quoted_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\'' && c != '"' {
            continue;
        }
        let mut value = String::new();
        while let Some(ch) = chars.next() {
            if ch == c {
                if chars.peek() == Some(&c) {
                    value.push(c);
                    chars.next();
                } else {
                    break;
                }
            } else if ch == '\\' {
                if let Some(n) = chars.next() {
                    value.push(n);
                }
            } else {
                value.push(ch);
            }
        }
        out.push(value);
    }
    out
}

/// Render bytes as UTF-8 when possible, otherwise as a hex blob.
pub(crate) fn bytes_to_string(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_owned(),
        Err(_) => {
            let mut out = String::with_capacity(2 + b.len() * 2);
            out.push_str("0x");
            for byte in b.iter().take(4096) {
                out.push_str(&format!("{byte:02X}"));
            }
            if b.len() > 4096 {
                out.push('…');
            }
            out
        }
    }
}

/// A column of a table that points at a row of another table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    pub column: String,
    /// The referenced table (always a table, never a view).
    pub target: DbObject,
    pub target_column: String,
}

/// Read the foreign keys out of a `Driver::structure` result: its `column` and `references`
/// columns, where references look like `schema.table(col)` (SQLite: `table(col)`, in schema
/// `main`). A column in several foreign keys uses the first.
pub fn parse_foreign_keys(kind: DbKind, structure: &QueryResult) -> Vec<ForeignKey> {
    let pos = |name: &str| structure.columns.iter().position(|c| c == name);
    let (Some(col_idx), Some(ref_idx)) = (pos("column"), pos("references")) else { return Vec::new() };
    structure
        .rows
        .iter()
        .filter_map(|row| {
            let column = row.get(col_idx)?.clone()?;
            let first = row.get(ref_idx)?.as_deref()?.split(", ").next()?.trim();
            let open = first.rfind('(')?;
            let target_column = first[open + 1..].strip_suffix(')')?.to_string();
            let table = &first[..open];
            let (schema, name) = match kind {
                DbKind::Sqlite => ("main", table),
                _ => table.split_once('.')?,
            };
            if name.is_empty() || target_column.is_empty() || target_column == "?" {
                return None;
            }
            let target = DbObject { schema: schema.into(), name: name.into(), ..Default::default() };
            Some(ForeignKey { column, target, target_column })
        })
        .collect()
}

/// Integration tests against live servers. Run with e.g.
/// `PUEO_PG=localhost:5432:postgres:secret cargo test -- --ignored`
/// (format `host:port:user:password`; also PUEO_MY and PUEO_RD).
#[cfg(test)]
mod parse_tests {
    #[test]
    fn builds_table_operations() {
        use super::*;
        let t = DbObject { schema: "public".into(), name: "users".into(), ..Default::default() };
        assert_eq!(
            truncate_sql(DbKind::Postgres, &t, true, true).0,
            vec![r#"TRUNCATE TABLE "public"."users" RESTART IDENTITY CASCADE"#]
        );
        let (stmts, finally) = truncate_sql(DbKind::Mysql, &t, true, false);
        assert_eq!(stmts.len(), 2);
        assert_eq!(finally.as_deref(), Some("SET FOREIGN_KEY_CHECKS = 1"));
        assert_eq!(drop_sql(DbKind::Postgres, &t, true), r#"DROP TABLE "public"."users" CASCADE"#);
        assert_eq!(drop_sql(DbKind::Mysql, &t, true), "DROP TABLE `public`.`users`");
        assert_eq!(
            clone_sql(DbKind::Postgres, &t, "users_copy", true),
            vec![
                r#"CREATE TABLE "public"."users_copy" (LIKE "public"."users" INCLUDING ALL)"#,
                r#"INSERT INTO "public"."users_copy" SELECT * FROM "public"."users""#,
            ]
        );
    }

    #[test]
    fn parses_foreign_keys() {
        use super::*;
        let s = |v: &str| Some(v.to_string());
        let st = QueryResult::from_rows(
            &["column", "type", "references"],
            vec![
                vec![s("id"), s("int"), None],
                vec![s("projectOrderId"), s("int"), s("public.projectOrder(id), public.other(x)")],
                vec![s("odd"), s("int"), s("public.a(b)c(id)")],
            ],
        );
        let fks = parse_foreign_keys(DbKind::Postgres, &st);
        assert_eq!(fks.len(), 2);
        assert_eq!(fks[0].column, "projectOrderId");
        assert_eq!((fks[0].target.schema.as_str(), fks[0].target.name.as_str()), ("public", "projectOrder"));
        assert_eq!(fks[0].target_column, "id");
        assert_eq!(fks[1].target.name, "a(b)c");
        let lite = QueryResult::from_rows(&["column", "references"], vec![vec![s("owner"), s("people(id)")]]);
        let fks = parse_foreign_keys(DbKind::Sqlite, &lite);
        assert_eq!((fks[0].target.schema.as_str(), fks[0].target.name.as_str()), ("main", "people"));
    }

    #[test]
    fn parses_lists() {
        assert_eq!(super::parse_quoted_list("'web', 'it''s', \"x\""), vec!["web", "it's", "x"]);
        assert_eq!(super::mysql::parse_enum_list("enum('a','b c')"), vec!["a", "b c"]);
    }
}

#[cfg(test)]
mod integration {
    use super::*;

    fn cfg_from_env(var: &str, kind: DbKind) -> Option<(ConnectionConfig, String)> {
        let v = std::env::var(var).ok()?;
        let parts: Vec<&str> = v.splitn(4, ':').collect();
        let mut cfg = ConnectionConfig::new(kind);
        cfg.host = parts[0].into();
        cfg.port = parts[1].parse().ok()?;
        cfg.user = parts.get(2).unwrap_or(&"").to_string();
        if kind == DbKind::Mysql {
            cfg.database = "shop".into();
        }
        Some((cfg, parts.get(3).unwrap_or(&"").to_string()))
    }

    /// Wrap a driver the way the app does, so the tests also check the query log.
    fn with_log(driver: SharedDriver) -> (SharedDriver, Arc<log::QueryLog>) {
        let log = log::QueryLog::new();
        (logged(driver, log.clone()), log)
    }

    /// Every kind of call was recorded with the SQL that was sent.
    fn check_log(log: &log::QueryLog) {
        let entries = log.since(0);
        for kind in [log::LogKind::Meta, log::LogKind::Data, log::LogKind::User] {
            assert!(entries.iter().any(|e| e.kind == kind), "no {kind:?} entries logged");
        }
        assert!(entries.iter().all(|e| !e.sql.is_empty()));
        println!("{} queries logged, e.g. {:?}", entries.len(), entries.iter().rev().take(3).map(|e| &e.sql).collect::<Vec<_>>());
    }

    /// A function created by `create` is listed as one and its source can be read back.
    async fn check_routine(d: &SharedDriver, create: &str, name: &str) {
        d.execute(create).await.unwrap();
        let objs = d.list_objects(None).await.unwrap().objects;
        let f = objs.iter().find(|o| o.name == name).expect("function listed");
        assert_eq!(f.kind, ObjectKind::Function);
        let def = d.definition(f).await.unwrap();
        assert!(def.contains("CREATE") && def.contains(name), "{def}");
    }

    async fn exercise(driver: SharedDriver, setup: &str, table: &str) {
        println!("{}", driver.server_info().await.unwrap());
        let results = driver.execute(setup).await.unwrap();
        assert!(!results.is_empty());
        let objects = driver.list_objects(None).await.unwrap().objects;
        let obj = objects.iter().find(|o| o.name == table).expect("table listed").clone();

        let opts = FetchOptions { limit: 2, offset: 1, filter: Some("id > 0".into()), order_by: Some(("id".into(), false)) };
        let page = driver.fetch_rows(&obj, &opts).await.unwrap();
        assert_eq!(page.columns, vec!["id", "name", "note"]);
        assert_eq!(page.rows.len(), 2);
        assert_eq!(page.rows[0][0].as_deref(), Some("2"), "DESC order + offset 1");
        assert_eq!(page.rows[1][2], None, "NULL preserved");
        assert_eq!(driver.count_rows(&obj, Some("id >= 2")).await.unwrap(), 2);

        let structure = driver.structure(&obj).await.unwrap();
        assert_eq!(structure.rows.len(), 3);
        assert_eq!(structure.rows[0][4].as_deref(), Some("PRI"));
        assert_eq!(structure.columns[5], "references");
        let indexes = driver.indexes(&obj).await.unwrap();
        assert_eq!(indexes.columns[..3], ["name", "columns", "unique"]);

        let multi = driver.execute("UPDATE people SET note = 'x' WHERE id = 1; SELECT count(*) AS n FROM people").await.unwrap();
        assert_eq!(multi.len(), 2);
        assert_eq!(multi[0].affected, Some(1));
        assert_eq!(multi[1].rows[0][0].as_deref(), Some("3"));
        assert!(driver.execute("SELEC nonsense").await.is_err());

        // Metadata used by the editor and the grid.
        assert_eq!(driver.primary_key(&obj).await.unwrap(), Some(vec!["id".to_string()]));
        let cols = driver.list_columns().await.unwrap();
        assert!(cols.contains(&("people".to_string(), "note".to_string())));

        // Grid edits: update (with tricky quoting), delete, insert with NULL.
        let s = |v: &str| Some(v.to_string());
        let tricky = r"O'Brien \ back\slash";
        let changes = vec![
            RowChange::Update {
                key: vec![("id".into(), s("1"))],
                set: vec![("name".into(), s(tricky)), ("note".into(), None)],
                row: vec![],
            },
            RowChange::Delete { key: vec![("id".into(), s("3"))] },
            RowChange::Insert { values: vec![("id".into(), s("10")), ("name".into(), s("Ken"))] },
        ];
        assert_eq!(driver.apply_changes(&obj, &changes).await.unwrap(), 3);
        let all = driver
            .fetch_rows(&obj, &FetchOptions { limit: 100, order_by: Some(("id".into(), true)), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(all.rows.len(), 3);
        assert_eq!(all.rows[0], vec![s("1"), s(tricky), None]);
        assert_eq!(all.rows[2], vec![s("10"), s("Ken"), None]);

        // Foreign keys show in Structure and indexes in Indexes.
        driver
            .execute("DROP TABLE IF EXISTS pets; CREATE TABLE pets (id INTEGER PRIMARY KEY, owner INTEGER, FOREIGN KEY (owner) REFERENCES people(id)); CREATE INDEX pets_owner_idx ON pets (owner)")
            .await
            .unwrap();
        let objects = driver.list_objects(None).await.unwrap().objects;
        let pets = objects.iter().find(|o| o.name == "pets").unwrap().clone();
        let st = driver.structure(&pets).await.unwrap();
        let refs = st.rows[1][5].clone().unwrap_or_default();
        assert!(refs.contains("people") && refs.contains("(id)"), "references: {refs}");
        let idx = driver.indexes(&pets).await.unwrap();
        let owner_idx = idx.rows.iter().find(|r| r[0].as_deref() == Some("pets_owner_idx")).expect("index listed");
        assert_eq!(owner_idx[1].as_deref(), Some("owner"));
        driver.execute("DROP TABLE pets").await.unwrap();

        // A failing change rolls back the whole batch.
        let bad = vec![
            RowChange::Delete { key: vec![("id".into(), s("10"))] },
            RowChange::Insert { values: vec![("id".into(), s("1")), ("name".into(), s("dup"))] },
        ];
        assert!(driver.apply_changes(&obj, &bad).await.is_err());
        assert_eq!(driver.count_rows(&obj, None).await.unwrap(), 3, "rolled back");
    }

    /// Start a slow query, cancel it, and check it ends quickly.
    async fn exercise_cancel(driver: SharedDriver, slow_sql: &'static str) {
        let d = driver.clone();
        let start = std::time::Instant::now();
        let running = tokio::spawn(async move { d.execute(slow_sql).await });
        tokio::time::sleep(Duration::from_millis(500)).await;
        driver.cancel().await.unwrap();
        let _ = running.await.unwrap();
        assert!(start.elapsed() < Duration::from_secs(5), "cancelled after {:?}", start.elapsed());
    }

    const SETUP: &str = "DROP TABLE IF EXISTS pets; DROP TABLE IF EXISTS people; \
        CREATE TABLE people (id INTEGER PRIMARY KEY, name VARCHAR(50), note TEXT); \
        INSERT INTO people VALUES (1, 'Ada', NULL), (2, 'Linus', 'kernel'), (3, 'Grace', NULL);";

    #[tokio::test]
    #[ignore]
    async fn postgres() {
        let Some((cfg, pw)) = cfg_from_env("PUEO_PG", DbKind::Postgres) else { return };
        let (d, log) = with_log(connect(&cfg, Some(pw), None).await.unwrap());
        exercise(d.clone(), SETUP, "people").await;
        assert!(d.list_databases().await.unwrap().contains(&"postgres".to_string()));
        check_routine(&d, "CREATE OR REPLACE FUNCTION add_one(i int) RETURNS int LANGUAGE sql AS 'SELECT i + 1'", "add_one").await;
        exercise_cancel(d.clone(), "SELECT pg_sleep(20)").await;
        assert!(d.execute("SELECT 1").await.is_ok(), "connection usable after cancel");
        check_log(&log);
    }

    #[tokio::test]
    #[ignore]
    async fn mysql() {
        let Some((cfg, pw)) = cfg_from_env("PUEO_MY", DbKind::Mysql) else { return };
        let (d, log) = with_log(connect(&cfg, Some(pw), None).await.unwrap());
        exercise(d.clone(), SETUP, "people").await;
        assert!(d.list_databases().await.unwrap().contains(&"shop".to_string()));
        check_routine(
            &d,
            "DROP FUNCTION IF EXISTS add_one; CREATE FUNCTION add_one(i INT) RETURNS INT DETERMINISTIC RETURN i + 1",
            "add_one",
        )
        .await;
        exercise_cancel(d.clone(), "SELECT SLEEP(20)").await;
        assert!(d.execute("SELECT 1").await.is_ok(), "connection usable after cancel");
        check_log(&log);
    }

    #[tokio::test]
    async fn sqlite() {
        let path = std::env::temp_dir().join(format!("pueo-test-{}.db", std::process::id()));
        let mut cfg = ConnectionConfig::new(DbKind::Sqlite);
        cfg.file_path = path.to_string_lossy().into_owned();
        let (d, log) = with_log(connect(&cfg, None, None).await.unwrap());
        exercise(d.clone(), SETUP, "people").await;
        assert!(d.list_databases().await.unwrap().is_empty());
        exercise_cancel(
            d.clone(),
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c",
        )
        .await;
        check_log(&log);
        let _ = std::fs::remove_file(path);
    }

    /// PUEO_SSH=ssh_host:ssh_port:user:password:db_host:db_port (a PostgreSQL behind the bastion,
    /// user postgres / password "secret"); optional PUEO_SSH_KEY=key_path:passphrase.
    #[tokio::test]
    #[ignore]
    async fn ssh_tunnel() {
        use crate::config::SshAuth;
        let Ok(spec) = std::env::var("PUEO_SSH") else { return };
        let p: Vec<&str> = spec.splitn(6, ':').collect();
        let mut cfg = ConnectionConfig::new(DbKind::Postgres);
        cfg.host = p[4].into();
        cfg.port = p[5].parse().unwrap();
        cfg.user = "postgres".into();
        cfg.ssh.enabled = true;
        cfg.ssh.host = p[0].into();
        cfg.ssh.port = p[1].parse().unwrap();
        cfg.ssh.user = p[2].into();

        cfg.ssh.auth = SshAuth::Password;
        let d = connect(&cfg, Some("secret".into()), Some(p[3].into())).await.unwrap();
        let info = d.server_info().await.unwrap();
        assert!(info.ends_with("· SSH"), "{info}");
        assert_eq!(d.execute("SELECT 40 + 2").await.unwrap()[0].rows[0][0].as_deref(), Some("42"));

        // The ssh process goes away with the driver.
        let tunnels = || {
            let out = std::process::Command::new("pgrep").args(["-f", "-c", "^ssh -N .* -p 52222 "]).output().unwrap();
            String::from_utf8_lossy(&out.stdout).trim().parse::<u32>().unwrap_or(0)
        };
        assert_eq!(tunnels(), 1);
        drop(d);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(tunnels(), 0, "ssh process killed on drop");

        let err = match connect(&cfg, Some("secret".into()), Some("wrong".into())).await {
            Ok(_) => panic!("wrong SSH password accepted"),
            Err(e) => format!("{e:#}"),
        };
        assert!(err.contains("SSH tunnel failed"), "{err}");

        if let Ok(key) = std::env::var("PUEO_SSH_KEY") {
            let (path, pass) = key.split_once(':').unwrap();
            cfg.ssh.auth = SshAuth::Key;
            cfg.ssh.key_path = path.into();
            let d = connect(&cfg, Some("secret".into()), Some(pass.into())).await.unwrap();
            assert!(d.execute("SELECT 1").await.is_ok());
        }
    }

    #[tokio::test]
    #[ignore]
    async fn redis() {
        let Some((cfg, pw)) = cfg_from_env("PUEO_RD", DbKind::Redis) else { return };
        let (d, log) = with_log(connect(&cfg, Some(pw), None).await.unwrap());
        println!("{}", d.server_info().await.unwrap());
        d.execute("FLUSHDB\nHSET user:1 name Ada lang \"rust lang\"\nRPUSH queue a b c\nSET greeting hello\nZADD board 10 x 20 y")
            .await
            .unwrap();
        let objs = d.list_objects(None).await.unwrap().objects;
        assert_eq!(objs.len(), 4);
        let hash = objs.iter().find(|o| o.name == "user:1").unwrap();
        assert_eq!(hash.schema, "hash");
        let r = d.fetch_rows(hash, &FetchOptions { limit: 100, ..Default::default() }).await.unwrap();
        assert_eq!(r.rows[0], vec![Some("lang".into()), Some("rust lang".into())]);
        let list = objs.iter().find(|o| o.name == "queue").unwrap();
        let r = d.fetch_rows(list, &FetchOptions { limit: 2, offset: 1, ..Default::default() }).await.unwrap();
        assert_eq!(r.rows, vec![vec![Some("1".into()), Some("b".into())], vec![Some("2".into()), Some("c".into())]]);
        assert_eq!(d.count_rows(list, None).await.unwrap(), 3);
        let z = objs.iter().find(|o| o.name == "board").unwrap();
        let r = d.fetch_rows(z, &FetchOptions { limit: 10, ..Default::default() }).await.unwrap();
        assert_eq!(r.rows[1], vec![Some("y".into()), Some("20".into())]);
        let info = d.structure(list).await.unwrap();
        assert_eq!(info.rows[1][1].as_deref(), Some("list"));
        let get = d.execute("GET greeting").await.unwrap();
        assert_eq!(get[0].rows[0][0].as_deref(), Some("hello"));

        // Server-side pattern search and database list.
        let found = d.list_objects(Some("user")).await.unwrap().objects;
        assert_eq!(found.len(), 1);
        assert_eq!(d.list_databases().await.unwrap().len(), 16);

        // Edits per type.
        let s = |v: &str| Some(v.to_string());
        let row = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), s(v))).collect::<Vec<_>>();
        assert_eq!(d.primary_key(hash).await.unwrap(), Some(vec!["field".to_string()]));
        d.apply_changes(hash, &[
            RowChange::Update { key: row(&[("field", "name")]), set: vec![], row: row(&[("field", "full_name"), ("value", "Ada L.")]) },
            RowChange::Delete { key: row(&[("field", "lang")]) },
            RowChange::Insert { values: row(&[("field", "born"), ("value", "1815")]) },
        ]).await.unwrap();
        let r = d.fetch_rows(hash, &FetchOptions { limit: 100, ..Default::default() }).await.unwrap();
        assert_eq!(r.rows, vec![vec![s("born"), s("1815")], vec![s("full_name"), s("Ada L.")]]);

        d.apply_changes(list, &[
            RowChange::Delete { key: row(&[("index", "0")]) },
            RowChange::Update { key: row(&[("index", "2")]), set: vec![], row: row(&[("index", "2"), ("value", "C")]) },
            RowChange::Insert { values: row(&[("value", "d")]) },
        ]).await.unwrap();
        let r = d.fetch_rows(list, &FetchOptions { limit: 100, ..Default::default() }).await.unwrap();
        assert_eq!(r.rows.iter().map(|x| x[1].clone().unwrap()).collect::<Vec<_>>(), vec!["b", "C", "d"]);

        d.apply_changes(z, &[
            RowChange::Update { key: row(&[("member", "x")]), set: vec![], row: row(&[("member", "xx"), ("score", "30")]) },
        ]).await.unwrap();
        let r = d.fetch_rows(z, &FetchOptions { limit: 10, ..Default::default() }).await.unwrap();
        assert_eq!(r.rows, vec![vec![s("y"), s("20")], vec![s("xx"), s("30")]]);

        let string = objs.iter().find(|o| o.name == "greeting").unwrap();
        d.apply_changes(string, &[RowChange::Update { key: vec![], set: vec![], row: row(&[("value", "bye")]) }]).await.unwrap();
        assert_eq!(d.execute("GET greeting").await.unwrap()[0].rows[0][0], s("bye"));
        assert!(d.cancel().await.is_err());
        check_log(&log);
    }
}
