//! Whole-database backup and restore through the servers' own tools (pg_dump / pg_restore /
//! psql, mysqldump / mysql), or SQLite's online backup API.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Notify;

use crate::config::ConnectionConfig;
use crate::db::DbKind;
use crate::ssh;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// pg_dump's compressed archive, restored with pg_restore.
    Custom,
    /// A script of SQL statements.
    Sql,
    /// A copy of the SQLite file.
    File,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Custom => "dump",
            Format::Sql => "sql",
            Format::File => "db",
        }
    }

    /// The format of an existing backup file, from its first bytes.
    pub fn detect(kind: DbKind, path: &Path) -> Result<Format> {
        let mut head = [0u8; 16];
        let n = std::io::Read::read(&mut std::fs::File::open(path)?, &mut head)?;
        let head = &head[..n];
        Ok(match kind {
            DbKind::Sqlite if head.starts_with(b"SQLite format 3") => Format::File,
            DbKind::Sqlite => bail!("{} is not a SQLite database", path.display()),
            DbKind::Postgres if head.starts_with(b"PGDMP") => Format::Custom,
            _ => Format::Sql,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BackupOptions {
    pub format: Format,
    /// Tables, views and functions without their rows.
    pub schema_only: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RestoreOptions {
    /// Drop the objects in the backup before recreating them (pg_restore only).
    pub clean: bool,
}

/// Where the tools connect: the server, or the local end of an SSH tunnel.
#[derive(Debug, Clone)]
struct Endpoint {
    host: String,
    port: u16,
    user: String,
    password: Option<String>,
    tls: bool,
}

/// A command line ready to run.
#[derive(Debug, Default, PartialEq)]
struct Invocation {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    /// File fed to the tool's standard input.
    stdin: Option<PathBuf>,
}

/// Back up `database` into `path`. `cancel` stops the tool; a partial file is removed.
pub async fn backup(
    cfg: &ConnectionConfig,
    password: Option<String>,
    ssh_secret: Option<String>,
    database: &str,
    path: &Path,
    opts: BackupOptions,
    cancel: Arc<Notify>,
) -> Result<()> {
    let result = match cfg.kind {
        DbKind::Sqlite => sqlite_copy(PathBuf::from(&cfg.file_path), path.to_path_buf(), false).await,
        DbKind::Redis => bail!("Redis databases can’t be backed up from Pueo"),
        kind => {
            let (ep, _tunnel) = endpoint(cfg, password, ssh_secret).await?;
            let inv = match kind {
                DbKind::Postgres => pg_dump(&ep, database, path, opts),
                _ => mysqldump(&mysql_client(true).await?, &ep, database, path, opts),
            };
            run(inv, cancel).await.map(|_| ())
        }
    };
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// Restore the backup in `path` into `database`.
pub async fn restore(
    cfg: &ConnectionConfig,
    password: Option<String>,
    ssh_secret: Option<String>,
    database: &str,
    path: &Path,
    opts: RestoreOptions,
    cancel: Arc<Notify>,
) -> Result<()> {
    let format = Format::detect(cfg.kind, path)?;
    match cfg.kind {
        DbKind::Sqlite => sqlite_copy(PathBuf::from(&cfg.file_path), path.to_path_buf(), true).await,
        DbKind::Redis => bail!("Redis databases can’t be restored from Pueo"),
        kind => {
            let (ep, _tunnel) = endpoint(cfg, password, ssh_secret).await?;
            let inv = match kind {
                DbKind::Postgres => pg_restore(&ep, database, path, format, opts),
                _ => mysql_restore(&mysql_client(false).await?, &ep, database, path),
            };
            run(inv, cancel).await.map(|_| ())
        }
    }
}

async fn endpoint(
    cfg: &ConnectionConfig,
    password: Option<String>,
    ssh_secret: Option<String>,
) -> Result<(Endpoint, Option<ssh::Tunnel>)> {
    let mut ep = Endpoint {
        host: if cfg.host.is_empty() { "localhost".into() } else { cfg.host.clone() },
        port: cfg.port,
        user: match (cfg.kind, cfg.user.is_empty()) {
            (DbKind::Postgres, true) => "postgres".into(),
            (_, true) => "root".into(),
            _ => cfg.user.clone(),
        },
        password: password.filter(|p| !p.is_empty()),
        tls: cfg.use_tls,
    };
    if !cfg.ssh.enabled {
        return Ok((ep, None));
    }
    let tunnel = ssh::open(&cfg.ssh, ssh_secret, &cfg.host, cfg.port).await?;
    ep.host = "127.0.0.1".into();
    ep.port = tunnel.local_port;
    Ok((ep, Some(tunnel)))
}

// ---------- PostgreSQL ----------

fn pg_env(ep: &Endpoint) -> Vec<(String, String)> {
    let mut env = vec![
        ("PGSSLMODE".into(), if ep.tls { "require" } else { "prefer" }.into()),
        ("PGCONNECT_TIMEOUT".into(), "10".into()),
        ("PGAPPNAME".into(), "Pueo".into()),
    ];
    if let Some(pw) = &ep.password {
        env.push(("PGPASSWORD".into(), pw.clone()));
    }
    env
}

fn pg_target(ep: &Endpoint, database: &str) -> Vec<String> {
    let database = if database.is_empty() { "postgres" } else { database };
    ["-h", &ep.host, "-p", &ep.port.to_string(), "-U", &ep.user, "-d", database]
        .map(String::from)
        .to_vec()
}

fn pg_dump(ep: &Endpoint, database: &str, path: &Path, opts: BackupOptions) -> Invocation {
    let mut args = pg_target(ep, database);
    args.push(format!("--format={}", if opts.format == Format::Custom { "custom" } else { "plain" }));
    if opts.schema_only {
        args.push("--schema-only".into());
    }
    args.push(format!("--file={}", path.display()));
    Invocation { program: "pg_dump".into(), args, env: pg_env(ep), stdin: None }
}

fn pg_restore(ep: &Endpoint, database: &str, path: &Path, format: Format, opts: RestoreOptions) -> Invocation {
    let mut args = pg_target(ep, database);
    if format == Format::Custom {
        // Roles in the backup may not exist on this server: objects belong to whoever restores.
        args.extend(["--no-owner", "--no-privileges"].map(String::from));
        if opts.clean {
            args.extend(["--clean", "--if-exists"].map(String::from));
        }
        args.push(path.display().to_string());
        Invocation { program: "pg_restore".into(), args, env: pg_env(ep), stdin: None }
    } else {
        args.extend(["-v", "ON_ERROR_STOP=1", "--quiet", "-f"].map(String::from));
        args.push(path.display().to_string());
        Invocation { program: "psql".into(), args, env: pg_env(ep), stdin: None }
    }
}

// ---------- MySQL / MariaDB ----------

/// The installed client: the program and whether it is MariaDB's.
#[derive(Debug, Clone, PartialEq)]
struct MysqlClient {
    program: String,
    mariadb: bool,
}

/// Find mysqldump / mariadb-dump (`dump`) or mysql / mariadb.
async fn mysql_client(dump: bool) -> Result<MysqlClient> {
    let names: &[&str] = if dump { &["mariadb-dump", "mysqldump"] } else { &["mariadb", "mysql"] };
    for name in names {
        if let Ok(out) = Command::new(name).arg("--version").output().await {
            let version = String::from_utf8_lossy(&out.stdout);
            return Ok(MysqlClient { program: name.to_string(), mariadb: version.contains("MariaDB") });
        }
    }
    bail!(
        "{} was not found. Install the MySQL or MariaDB client tools (e.g. sudo apt install mariadb-client).",
        names[1]
    )
}

fn mysql_args(client: &MysqlClient, ep: &Endpoint) -> Vec<String> {
    let mut args = ["-h", &ep.host, "-P", &ep.port.to_string(), "-u", &ep.user, "--protocol=TCP"]
        .map(String::from)
        .to_vec();
    // Like the app's own connection: encrypt when asked, without verifying the certificate.
    args.extend(
        match (client.mariadb, ep.tls) {
            (true, true) => &["--ssl", "--skip-ssl-verify-server-cert"][..],
            (true, false) => &["--skip-ssl"][..],
            (false, true) => &["--ssl-mode=REQUIRED"][..],
            (false, false) => &[][..],
        }
        .iter()
        .map(|s| s.to_string()),
    );
    args
}

fn mysql_env(ep: &Endpoint) -> Vec<(String, String)> {
    ep.password.iter().map(|pw| ("MYSQL_PWD".to_string(), pw.clone())).collect()
}

fn mysqldump(client: &MysqlClient, ep: &Endpoint, database: &str, path: &Path, opts: BackupOptions) -> Invocation {
    let mut args = mysql_args(client, ep);
    // No --databases: the dump has no CREATE DATABASE / USE, so it restores into any database.
    args.extend(["--single-transaction", "--routines", "--triggers", "--no-tablespaces"].map(String::from));
    if opts.schema_only {
        args.push("--no-data".into());
    }
    args.push(format!("--result-file={}", path.display()));
    args.push(database.to_string());
    Invocation { program: client.program.clone(), args, env: mysql_env(ep), stdin: None }
}

fn mysql_restore(client: &MysqlClient, ep: &Endpoint, database: &str, path: &Path) -> Invocation {
    let mut args = mysql_args(client, ep);
    args.push(database.to_string());
    Invocation { program: client.program.clone(), args, env: mysql_env(ep), stdin: Some(path.to_path_buf()) }
}

// ---------- running ----------

fn package_hint(program: &str) -> &'static str {
    match program {
        "pg_dump" | "pg_restore" | "psql" => " Install the PostgreSQL client tools (sudo apt install postgresql-client).",
        _ => "",
    }
}

/// Run the tool; fails with the end of its error output. Returns what it printed to stderr.
async fn run(inv: Invocation, cancel: Arc<Notify>) -> Result<String> {
    let mut cmd = Command::new(&inv.program);
    cmd.args(&inv.args)
        .envs(inv.env.iter().map(|(k, v)| (k, v)))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match &inv.stdin {
        Some(path) => {
            let file = std::fs::File::open(path).with_context(|| format!("Could not open {}", path.display()))?;
            cmd.stdin(file);
        }
        None => {
            cmd.stdin(Stdio::null());
        }
    }
    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => anyhow!("{} was not found.{}", inv.program, package_hint(&inv.program)),
        _ => anyhow!("Could not start {}: {e}", inv.program),
    })?;
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let finished = async {
        let mut err = String::new();
        let _ = stderr.read_to_string(&mut err).await;
        (child.wait().await, err)
    };
    // On cancel the child is dropped here, which kills it.
    let (status, err) = tokio::select! {
        done = finished => done,
        _ = cancel.notified() => bail!("Cancelled"),
    };
    if !status?.success() {
        bail!("{} failed:\n{}", inv.program, tail(&err, 15));
    }
    Ok(err)
}

/// The last `n` non-empty lines of `text`.
fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

// ---------- SQLite ----------

/// Copy the database at `db` into `file`, or (`restore`) the contents of `file` into `db`.
/// Uses SQLite's backup API, so the open connection keeps working.
async fn sqlite_copy(db: PathBuf, file: PathBuf, restore: bool) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let mut conn = rusqlite::Connection::open(&db).with_context(|| format!("Could not open {}", db.display()))?;
        if restore {
            conn.restore(rusqlite::MAIN_DB, &file, None::<fn(rusqlite::backup::Progress)>)?;
        } else {
            conn.backup(rusqlite::MAIN_DB, &file, None)?;
        }
        Ok(())
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep() -> Endpoint {
        Endpoint { host: "db".into(), port: 5432, user: "me".into(), password: Some("pw".into()), tls: false }
    }

    #[test]
    fn builds_pg_commands() {
        let opts = BackupOptions { format: Format::Custom, schema_only: true };
        let inv = pg_dump(&ep(), "ERP", Path::new("/b/erp.dump"), opts);
        assert_eq!(inv.program, "pg_dump");
        assert_eq!(
            inv.args,
            ["-h", "db", "-p", "5432", "-U", "me", "-d", "ERP", "--format=custom", "--schema-only", "--file=/b/erp.dump"]
        );
        assert!(inv.env.contains(&("PGPASSWORD".into(), "pw".into())));

        let inv = pg_restore(&ep(), "ERP", Path::new("/b/erp.dump"), Format::Custom, RestoreOptions { clean: true });
        assert_eq!(inv.program, "pg_restore");
        assert!(inv.args.ends_with(&["--clean".into(), "--if-exists".into(), "/b/erp.dump".into()]));

        let inv = pg_restore(&ep(), "ERP", Path::new("/b/erp.sql"), Format::Sql, RestoreOptions::default());
        assert_eq!(inv.program, "psql");
        assert!(inv.args.ends_with(&["-f".into(), "/b/erp.sql".into()]));
    }

    #[test]
    fn builds_mysql_commands() {
        let client = MysqlClient { program: "mariadb-dump".into(), mariadb: true };
        let ep = Endpoint { port: 3306, tls: true, ..ep() };
        let opts = BackupOptions { format: Format::Sql, schema_only: false };
        let inv = mysqldump(&client, &ep, "shop", Path::new("/b/shop.sql"), opts);
        assert!(inv.args.contains(&"--ssl".into()));
        assert!(inv.args.ends_with(&["--result-file=/b/shop.sql".into(), "shop".into()]));
        assert_eq!(inv.env, [("MYSQL_PWD".to_string(), "pw".to_string())]);

        let client = MysqlClient { program: "mysql".into(), mariadb: false };
        let inv = mysql_restore(&client, &ep, "shop", Path::new("/b/shop.sql"));
        assert!(inv.args.contains(&"--ssl-mode=REQUIRED".into()));
        assert_eq!(inv.stdin.as_deref(), Some(Path::new("/b/shop.sql")));
    }

    #[tokio::test]
    async fn sqlite_round_trip() {
        let dir = std::env::temp_dir();
        let id = std::process::id();
        let (db, file) = (dir.join(format!("pueo-bk-{id}.db")), dir.join(format!("pueo-bk-{id}.backup")));
        let mut conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1), (2);").unwrap();
        sqlite_copy(db.clone(), file.clone(), false).await.unwrap();
        assert_eq!(Format::detect(DbKind::Sqlite, &file).unwrap(), Format::File);
        conn.execute_batch("DELETE FROM t").unwrap();
        sqlite_copy(db.clone(), file.clone(), true).await.unwrap();
        let n: i64 = conn.query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
        let _ = std::fs::remove_file(db);
        let _ = std::fs::remove_file(file);
    }

    #[tokio::test]
    async fn missing_tool_is_explained() {
        let inv = Invocation { program: "pg_dump-not-installed".into(), ..Default::default() };
        let err = run(inv, Arc::new(Notify::new())).await.unwrap_err();
        assert!(err.to_string().contains("was not found"), "{err}");
    }
}
