//! Import connections from other database apps and from files.
//!
//! Passwords that live in another app's keychain or encrypted store are not read; Pueo asks for
//! them on first connect. Passwords that a file carries in plain text (URLs, CSV) are kept so
//! the caller can store them in the keychain.
use crate::{
    config::{Color, ConnectionConfig, Group, SshAuth, Store, Tag},
    db::DbKind,
};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct Imported {
    pub config: ConnectionConfig,
    pub password: Option<String>,
    pub ssh_secret: Option<String>,
    /// Name of the group in the source app; mapped to a Pueo group on import.
    pub group: Option<String>,
}

#[derive(Default)]
pub struct Batch {
    pub connections: Vec<Imported>,
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum App {
    TablePlus,
    DBeaver,
    Beekeeper,
    SequelAce,
    PgAdmin,
}

impl App {
    pub const ALL: [App; 5] = [App::TablePlus, App::DBeaver, App::Beekeeper, App::SequelAce, App::PgAdmin];

    pub fn name(self) -> &'static str {
        match self {
            App::TablePlus => "TablePlus",
            App::DBeaver => "DBeaver",
            App::Beekeeper => "Beekeeper Studio",
            App::SequelAce => "Sequel Ace",
            App::PgAdmin => "pgAdmin 4",
        }
    }

    pub fn badge(self) -> (&'static str, u32) {
        match self {
            App::TablePlus => ("Tp", 0xf2a33a),
            App::DBeaver => ("Db", 0x8d6e4f),
            App::Beekeeper => ("Bk", 0xd9a800),
            App::SequelAce => ("Sa", 0x2f80ed),
            App::PgAdmin => ("pA", 0x336791),
        }
    }

    fn candidates(self) -> Vec<PathBuf> {
        let data = dirs::data_dir().unwrap_or_default();
        let config = dirs::config_dir().unwrap_or_default();
        let home = dirs::home_dir().unwrap_or_default();
        match self {
            App::TablePlus => vec![data.join("com.tinyapp.TablePlus/Data/Connections.plist")],
            App::DBeaver => vec![
                data.join("DBeaverData/workspace6/General/.dbeaver/data-sources.json"),
                home.join(".local/share/DBeaverData/workspace6/General/.dbeaver/data-sources.json"),
            ],
            App::Beekeeper => vec![data.join("beekeeper-studio/app.db"), config.join("beekeeper-studio/app.db")],
            App::SequelAce => vec![home.join(
                "Library/Containers/com.sequel-ace.sequel-ace/Data/Library/Application Support/Sequel Ace/Data/Favorites.plist",
            )],
            App::PgAdmin => vec![home.join(".pgadmin/pgadmin4.db"), data.join("pgadmin/pgadmin4.db")],
        }
    }

    /// Where this app keeps its connections on this computer, if it is installed.
    pub fn path(self) -> Option<PathBuf> {
        self.candidates().into_iter().find(|p| p.exists())
    }

    pub fn load(self, path: &Path) -> Result<Batch> {
        match self {
            App::TablePlus => tableplus(&read_plist(path)?),
            App::DBeaver => dbeaver(&serde_json::from_slice(&std::fs::read(path)?)?),
            App::Beekeeper => beekeeper(path),
            App::SequelAce => sequel_ace(&read_plist(path)?),
            App::PgAdmin => pgadmin(path),
        }
    }
}

/// Read any supported file: Pueo or DBeaver JSON, a TablePlus or Sequel Ace plist, CSV, a
/// SQLite database, or a list of connection URLs.
pub fn load_file(path: &Path) -> Result<Batch> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    match ext.as_str() {
        "json" => {
            let value: Value = serde_json::from_slice(&std::fs::read(path)?).context("not a JSON file")?;
            if value.get("connections").is_some_and(Value::is_object) {
                return dbeaver(&value);
            }
            pueo(value)
        }
        "plist" => {
            let value = read_plist(path)?;
            if value.get("Favorites Root").is_some() { sequel_ace(&value) } else { tableplus(&value) }
        }
        "csv" => csv_file(path),
        "tableplusconnection" => bail!(NeedsPassword),
        "db" | "sqlite" | "sqlite3" => {
            let mut c = ConnectionConfig::new(DbKind::Sqlite);
            c.file_path = path.to_string_lossy().into_owned();
            c.name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            Ok(Batch { connections: vec![plain(c)], skipped: Vec::new() })
        }
        _ => urls(&std::fs::read_to_string(path).context("not a text file")?),
    }
}

/// The file is encrypted and needs the password it was exported with.
#[derive(Debug)]
pub struct NeedsPassword;

impl std::fmt::Display for NeedsPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("this export is protected with a password")
    }
}

impl std::error::Error for NeedsPassword {}

/// Read a TablePlus `.tableplusconnection` export: an RNCryptor v3 container (AES-256-CBC,
/// PBKDF2-SHA1 keys, HMAC-SHA256) around the same connection list TablePlus keeps on disk.
pub fn load_tableplus_export(path: &Path, password: &str) -> Result<Batch> {
    let plain = rncryptor_decrypt(&std::fs::read(path)?, password)?;
    let value: Value = match serde_json::from_slice(&plain) {
        Ok(v) => v,
        Err(_) => plist_from_bytes(&plain)?,
    };
    let list = match &value {
        Value::Object(map) => map
            .iter()
            .find(|(k, v)| k.to_lowercase().contains("connection") && v.is_array())
            .map(|(_, v)| v.clone())
            .unwrap_or(value.clone()),
        _ => value.clone(),
    };
    tableplus(&list)
}

fn rncryptor_decrypt(data: &[u8], password: &str) -> Result<Vec<u8>> {
    use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
    use hmac::Mac;
    if data.len() < 2 + 8 + 8 + 16 + 16 + 32 || data[0] != 3 {
        bail!("not a TablePlus export (unknown format)");
    }
    if data[1] != 1 {
        bail!("this TablePlus export uses a key file, which Pueo can't read");
    }
    let (enc_salt, hmac_salt, iv) = (&data[2..10], &data[10..18], &data[18..34]);
    let (body, tag) = data.split_at(data.len() - 32);
    let derive = |salt: &[u8]| pbkdf2::pbkdf2_hmac_array::<sha1::Sha1, 32>(password.as_bytes(), salt, 10_000);
    let mut mac = <hmac::Hmac<sha2::Sha256> as Mac>::new_from_slice(&derive(hmac_salt))?;
    mac.update(body);
    if mac.verify_slice(tag).is_err() {
        bail!(NeedsPassword);
    }
    let mut buf = body[34..].to_vec();
    let len = cbc::Decryptor::<aes::Aes256>::new(&derive(enc_salt).into(), iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| anyhow::anyhow!("the export is damaged"))?
        .len();
    buf.truncate(len);
    Ok(buf)
}

/// Convert plist bytes (XML or binary) to JSON without writing them to disk.
fn plist_from_bytes(bytes: &[u8]) -> Result<Value> {
    use std::io::Write;
    let mut child = std::process::Command::new("plutil")
        .args(["-convert", "json", "-o", "-", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("could not run plutil")?;
    child.stdin.take().context("plutil stdin")?.write_all(bytes)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!("the export was decrypted but its contents are not a connection list");
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn plain(config: ConnectionConfig) -> Imported {
    Imported { config, password: None, ssh_secret: None, group: None }
}

fn read_plist(path: &Path) -> Result<Value> {
    let output = std::process::Command::new("plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(path)
        .output()
        .context("could not run plutil to read the property list")?;
    if !output.status.success() {
        bail!("could not read {}", path.display());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn kind_from(name: &str) -> Option<DbKind> {
    let name = name.to_lowercase();
    if name.contains("postgres") || ["redshift", "cockroach", "cockroachdb", "greenplum", "pg"].contains(&name.trim()) {
        Some(DbKind::Postgres)
    } else if name.contains("mysql") || name.contains("mariadb") {
        Some(DbKind::Mysql)
    } else if name.contains("sqlite") {
        Some(DbKind::Sqlite)
    } else if name.contains("redis") {
        Some(DbKind::Redis)
    } else {
        None
    }
}

fn tag_from(name: &str) -> Option<Tag> {
    match name.trim().to_lowercase().as_str() {
        "local" => Some(Tag::Local),
        "dev" | "development" => Some(Tag::Development),
        "test" | "testing" | "qa" => Some(Tag::Testing),
        "stage" | "staging" => Some(Tag::Staging),
        "prod" | "production" => Some(Tag::Production),
        _ => None,
    }
}

fn color_from_name(name: &str) -> Option<Color> {
    match name.trim().to_lowercase().as_str() {
        "red" => Some(Color::Red),
        "orange" => Some(Color::Orange),
        "yellow" => Some(Color::Yellow),
        "green" => Some(Color::Green),
        "teal" | "cyan" => Some(Color::Teal),
        "blue" => Some(Color::Blue),
        "purple" | "pink" | "violet" => Some(Color::Purple),
        "gray" | "grey" => Some(Color::Gray),
        other if other.starts_with('#') => nearest_color(other),
        _ => None,
    }
}

fn str_of(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn bool_of(v: &Value, key: &str) -> bool {
    match v.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        Some(Value::String(s)) => matches!(s.as_str(), "1" | "true" | "yes"),
        _ => false,
    }
}

fn set_ssh(c: &mut ConnectionConfig, host: String, port: &str, user: String, key: String, password_auth: bool) {
    c.ssh.enabled = true;
    c.ssh.host = host;
    c.ssh.port = port.parse().unwrap_or(22);
    c.ssh.user = user;
    if !key.is_empty() {
        c.ssh.auth = SshAuth::Key;
        c.ssh.key_path = resolve_key(&key);
    } else if password_auth {
        c.ssh.auth = SshAuth::Password;
    } else {
        c.ssh.auth = SshAuth::Agent;
    }
}

fn set_server(c: &mut ConnectionConfig, host: String, port: &str, user: String, database: String) {
    if !host.is_empty() {
        c.host = host;
    }
    c.port = port.parse().unwrap_or(c.kind.default_port());
    c.user = user;
    c.database = database;
}

fn tableplus(value: &Value) -> Result<Batch> {
    let Some(list) = value.as_array() else { bail!("not a TablePlus connections file") };
    let mut batch = Batch::default();
    for item in list {
        let name = str_of(item, "ConnectionName");
        let driver = str_of(item, "Driver");
        let Some(kind) = kind_from(&driver) else {
            batch.skipped.push(format!("{name} ({driver})"));
            continue;
        };
        if kind == DbKind::Sqlite && bool_of(item, "isOverSSH") {
            batch.skipped.push(format!("{name} (SQLite over SSH)"));
            continue;
        }
        let mut c = ConnectionConfig::new(kind);
        c.id = format!("tableplus-{}", str_of(item, "ID"));
        c.name = name;
        if kind == DbKind::Sqlite {
            c.file_path = str_of(item, "DatabasePath");
        } else {
            set_server(&mut c, str_of(item, "DatabaseHost"), &str_of(item, "DatabasePort"), str_of(item, "DatabaseUser"), str_of(item, "DatabaseName"));
            c.use_tls = item.get("tLSMode").and_then(Value::as_i64).unwrap_or(0) > 0;
        }
        if bool_of(item, "isOverSSH") {
            let uses_key = bool_of(item, "isUsePrivateKey");
            let key = if uses_key { str_of(item, "ServerPrivateKeyName") } else { String::new() };
            set_ssh(&mut c, str_of(item, "ServerAddress"), &str_of(item, "ServerPort"), str_of(item, "ServerUser"), key, !uses_key);
        }
        c.tag = tag_from(&str_of(item, "Enviroment"));
        c.color = nearest_color(&str_of(item, "statusColor"));
        // Only TablePlus' encrypted export files carry passwords; Connections.plist does not.
        let secret = |keys: &[&str]| keys.iter().map(|k| str_of(item, k)).find(|v| !v.is_empty());
        let password = secret(&["DatabasePassword", "Password"]);
        let ssh_secret = secret(&["ServerPassword", "ServerPrivateKeyPassphrase", "SSHPassword"]);
        batch.connections.push(Imported { config: c, password, ssh_secret, group: None });
    }
    Ok(batch)
}

fn dbeaver(value: &Value) -> Result<Batch> {
    let Some(map) = value.get("connections").and_then(Value::as_object) else {
        bail!("not a DBeaver data-sources file");
    };
    let mut batch = Batch::default();
    for (id, item) in map {
        let name = str_of(item, "name");
        let provider = format!("{} {}", str_of(item, "provider"), str_of(item, "driver"));
        let Some(kind) = kind_from(&provider) else {
            batch.skipped.push(format!("{name} ({})", str_of(item, "provider")));
            continue;
        };
        let cfg = item.get("configuration").cloned().unwrap_or(Value::Null);
        let mut c = ConnectionConfig::new(kind);
        c.id = format!("dbeaver-{id}");
        c.name = name;
        if kind == DbKind::Sqlite {
            let db = str_of(&cfg, "database");
            c.file_path = if db.is_empty() { str_of(&cfg, "url").trim_start_matches("jdbc:sqlite:").to_string() } else { db };
        } else {
            set_server(&mut c, str_of(&cfg, "host"), &str_of(&cfg, "port"), str_of(&cfg, "user"), str_of(&cfg, "database"));
        }
        if let Some(ssh) = cfg.pointer("/handlers/ssh_tunnel").filter(|h| bool_of(h, "enabled")) {
            let props = ssh.get("properties").cloned().unwrap_or(Value::Null);
            let auth = str_of(&props, "authType");
            let key = if auth == "PUBLIC_KEY" { str_of(&props, "keyPath") } else { String::new() };
            let user = [str_of(&props, "user"), str_of(ssh, "user")].into_iter().find(|u| !u.is_empty()).unwrap_or_default();
            set_ssh(&mut c, str_of(&props, "host"), &str_of(&props, "port"), user, key, auth == "PASSWORD");
        }
        c.tag = tag_from(&str_of(&cfg, "type"));
        let folder = str_of(item, "folder");
        batch.connections.push(Imported { config: c, password: None, ssh_secret: None, group: (!folder.is_empty()).then_some(folder) });
    }
    Ok(batch)
}

fn sqlite_rows(path: &Path, sql: &str) -> Result<Vec<Value>> {
    let db = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = db.prepare(sql)?;
    let names: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
    let rows = stmt.query_map([], |row| {
        let mut map = serde_json::Map::new();
        for (i, name) in names.iter().enumerate() {
            let v = match row.get_ref(i)? {
                rusqlite::types::ValueRef::Integer(n) => Value::from(n),
                rusqlite::types::ValueRef::Real(f) => Value::from(f),
                rusqlite::types::ValueRef::Text(t) => Value::from(String::from_utf8_lossy(t).into_owned()),
                _ => Value::Null,
            };
            map.insert(name.clone(), v);
        }
        Ok(Value::Object(map))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

fn beekeeper(path: &Path) -> Result<Batch> {
    let mut batch = Batch::default();
    for v in sqlite_rows(path, "SELECT * FROM saved_connection")? {
        let name = str_of(&v, "name");
        let Some(kind) = kind_from(&str_of(&v, "connectionType")) else {
            batch.skipped.push(format!("{name} ({})", str_of(&v, "connectionType")));
            continue;
        };
        let mut c = ConnectionConfig::new(kind);
        c.id = format!("beekeeper-{}", str_of(&v, "id"));
        c.name = name;
        if kind == DbKind::Sqlite {
            c.file_path = str_of(&v, "defaultDatabase");
        } else {
            set_server(&mut c, str_of(&v, "host"), &str_of(&v, "port"), str_of(&v, "username"), str_of(&v, "defaultDatabase"));
            c.use_tls = bool_of(&v, "ssl");
        }
        if bool_of(&v, "sshEnabled") {
            let mode = str_of(&v, "sshMode");
            let key = if mode == "keyfile" { str_of(&v, "sshKeyfile") } else { String::new() };
            set_ssh(&mut c, str_of(&v, "sshHost"), &str_of(&v, "sshPort"), str_of(&v, "sshUsername"), key, mode == "userpass");
        }
        c.color = color_from_name(&str_of(&v, "labelColor"));
        batch.connections.push(plain(c));
    }
    Ok(batch)
}

fn pgadmin(path: &Path) -> Result<Batch> {
    let groups: HashMap<String, String> = sqlite_rows(path, "SELECT id, name FROM servergroup")
        .unwrap_or_default()
        .into_iter()
        .map(|v| (str_of(&v, "id"), str_of(&v, "name")))
        .collect();
    let mut batch = Batch::default();
    for v in sqlite_rows(path, "SELECT * FROM server")? {
        let mut c = ConnectionConfig::new(DbKind::Postgres);
        c.id = format!("pgadmin-{}", str_of(&v, "id"));
        c.name = str_of(&v, "name");
        set_server(&mut c, str_of(&v, "host"), &str_of(&v, "port"), str_of(&v, "username"), str_of(&v, "maintenance_db"));
        if bool_of(&v, "use_ssh_tunnel") {
            let uses_key = bool_of(&v, "tunnel_authentication");
            let key = if uses_key { str_of(&v, "tunnel_identity_file") } else { String::new() };
            set_ssh(&mut c, str_of(&v, "tunnel_host"), &str_of(&v, "tunnel_port"), str_of(&v, "tunnel_username"), key, !uses_key);
        }
        c.color = nearest_color(&str_of(&v, "bgcolor"));
        let group = groups.get(&str_of(&v, "servergroup_id")).cloned().filter(|g| g != "Servers");
        batch.connections.push(Imported { config: c, password: None, ssh_secret: None, group });
    }
    Ok(batch)
}

fn sequel_ace(value: &Value) -> Result<Batch> {
    fn walk(node: &Value, group: Option<String>, batch: &mut Batch) {
        if let Some(children) = node.get("Children").and_then(Value::as_array) {
            let name = str_of(node, "Name");
            let group = if name.is_empty() || name == "Favorites" { group } else { Some(name) };
            for child in children {
                walk(child, group.clone(), batch);
            }
            return;
        }
        if node.get("host").is_none() && node.get("sshHost").is_none() {
            return;
        }
        let mut c = ConnectionConfig::new(DbKind::Mysql);
        c.id = format!("sequelace-{}", str_of(node, "id"));
        c.name = str_of(node, "name");
        set_server(&mut c, str_of(node, "host"), &str_of(node, "port"), str_of(node, "user"), str_of(node, "database"));
        c.use_tls = bool_of(node, "useSSL");
        if node.get("type").and_then(Value::as_i64) == Some(2) {
            let key = if bool_of(node, "sshKeyLocationEnabled") { str_of(node, "sshKeyLocation") } else { String::new() };
            let password_auth = key.is_empty();
            set_ssh(&mut c, str_of(node, "sshHost"), &str_of(node, "sshPort"), str_of(node, "sshUser"), key, password_auth);
        }
        batch.connections.push(Imported { config: c, password: None, ssh_secret: None, group });
    }
    let mut batch = Batch::default();
    walk(value.get("Favorites Root").unwrap_or(value), None, &mut batch);
    Ok(batch)
}

fn pueo(value: Value) -> Result<Batch> {
    let store = if value.is_array() {
        Store { groups: Vec::new(), connections: serde_json::from_value(value).context("not a Pueo connections file")? }
    } else {
        serde_json::from_value::<Store>(value).context("not a Pueo connections file")?
    };
    let names: HashMap<String, String> = store.groups.iter().map(|g| (g.id.clone(), g.name.clone())).collect();
    Ok(Batch {
        connections: store
            .connections
            .into_iter()
            .map(|c| {
                let group = c.group.as_ref().and_then(|g| names.get(g).cloned());
                Imported { config: c, password: None, ssh_secret: None, group }
            })
            .collect(),
        skipped: Vec::new(),
    })
}

fn csv_file(path: &Path) -> Result<Batch> {
    let mut reader = csv::ReaderBuilder::new().flexible(true).from_path(path)?;
    let headers: Vec<String> = reader.headers()?.iter().map(|h| h.trim().to_lowercase().replace([' ', '-'], "_")).collect();
    let mut batch = Batch::default();
    for (n, record) in reader.records().enumerate() {
        let record = record?;
        let get = |keys: &[&str]| {
            keys.iter()
                .find_map(|k| headers.iter().position(|h| h == k).and_then(|i| record.get(i)))
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        };
        let url = get(&["url", "uri", "connection_string"]);
        if !url.is_empty() {
            match parse_url(&url) {
                Ok(mut imported) => {
                    let name = get(&["name"]);
                    if !name.is_empty() {
                        imported.config.name = name;
                    }
                    batch.connections.push(imported);
                }
                Err(_) => batch.skipped.push(format!("row {}", n + 2)),
            }
            continue;
        }
        let engine = get(&["type", "kind", "driver", "engine", "database_type"]);
        let Some(kind) = kind_from(&engine) else {
            batch.skipped.push(format!("row {} ({engine})", n + 2));
            continue;
        };
        let mut c = ConnectionConfig::new(kind);
        c.name = get(&["name", "connection", "title"]);
        if kind == DbKind::Sqlite {
            c.file_path = get(&["path", "file", "database"]);
        } else {
            set_server(&mut c, get(&["host", "hostname", "server"]), &get(&["port"]), get(&["user", "username"]), get(&["database", "db", "dbname"]));
        }
        let ssh_host = get(&["ssh_host", "ssh_server"]);
        if !ssh_host.is_empty() {
            set_ssh(&mut c, ssh_host, &get(&["ssh_port"]), get(&["ssh_user", "ssh_username"]), get(&["ssh_key", "ssh_key_path"]), false);
        }
        c.tag = tag_from(&get(&["tag", "environment", "env"]));
        c.color = color_from_name(&get(&["color", "colour"]));
        let password = Some(get(&["password", "pass"])).filter(|p| !p.is_empty());
        let group = Some(get(&["group", "folder"])).filter(|g| !g.is_empty());
        batch.connections.push(Imported { config: c, password, ssh_secret: None, group });
    }
    Ok(batch)
}

fn urls(text: &str) -> Result<Batch> {
    let mut batch = Batch::default();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        match parse_url(line) {
            Ok(c) => batch.connections.push(c),
            Err(_) => batch.skipped.push(line.chars().take(40).collect()),
        }
    }
    if batch.connections.is_empty() {
        bail!("no connection URLs found (expected lines like postgres://user@host:5432/db)");
    }
    Ok(batch)
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if let Some(b) = bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `postgres://user:pass@host:5432/db?sslmode=require`, `mysql://…`, `redis://…`, `rediss://…`,
/// `sqlite:///absolute/path.db`.
pub fn parse_url(url: &str) -> Result<Imported> {
    let Some((scheme, rest)) = url.split_once("://").or_else(|| url.split_once(':')) else {
        bail!("not a connection URL");
    };
    let scheme = scheme.to_lowercase();
    let Some(kind) = kind_from(&scheme) else { bail!("unsupported scheme {scheme}") };
    let mut c = ConnectionConfig::new(kind);
    if kind == DbKind::Sqlite {
        c.file_path = decode(rest.trim_start_matches("//"));
        c.name = Path::new(&c.file_path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        return Ok(plain(c));
    }
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (userinfo, hostport) = authority.rsplit_once('@').unwrap_or(("", authority));
    let (user, password) = userinfo.split_once(':').map_or((userinfo, None), |(u, p)| (u, Some(decode(p))));
    let (host, port) = match hostport.strip_prefix('[').and_then(|h| h.split_once(']')) {
        Some((host, port)) => (host, port.trim_start_matches(':')),
        None => hostport.rsplit_once(':').unwrap_or((hostport, "")),
    };
    if host.is_empty() {
        bail!("missing host");
    }
    set_server(&mut c, decode(host), port, decode(user), decode(path));
    c.use_tls = scheme == "rediss"
        || query.split('&').any(|kv| matches!(kv, "sslmode=require" | "sslmode=verify-ca" | "sslmode=verify-full" | "ssl=true" | "tls=true"));
    c.name = if c.database.is_empty() { c.host.clone() } else { format!("{} on {}", c.database, c.host) };
    Ok(Imported { config: c, password: password.filter(|p| !p.is_empty()), ssh_secret: None, group: None })
}

fn resolve_key(name: &str) -> String {
    let home = dirs::home_dir().unwrap_or_default();
    if let Some(rest) = name.strip_prefix("~/") {
        return home.join(rest).to_string_lossy().into_owned();
    }
    if Path::new(name).is_absolute() {
        return name.into();
    }
    let in_ssh = home.join(".ssh").join(name);
    if in_ssh.exists() { in_ssh.to_string_lossy().into_owned() } else { name.into() }
}

fn nearest_color(hex: &str) -> Option<Color> {
    let v = u32::from_str_radix(hex.trim().trim_start_matches('#'), 16).ok()?;
    let (h, s) = hue_saturation([(v >> 16) & 0xff, (v >> 8) & 0xff, v & 0xff].map(|c| c as f64 / 255.));
    if s < 0.25 {
        return Some(Color::Gray);
    }
    let distance = |c: Color| {
        let (r, g, b) = c.rgb();
        let d = (hue_saturation([r, g, b]).0 - h).abs();
        d.min(360. - d)
    };
    Color::ALL
        .into_iter()
        .filter(|c| *c != Color::Gray)
        .min_by(|a, b| distance(*a).total_cmp(&distance(*b)))
}

fn hue_saturation([r, g, b]: [f64; 3]) -> (f64, f64) {
    let max = r.max(g).max(b);
    let delta = max - r.min(g).min(b);
    if delta == 0. {
        return (0., 0.);
    }
    let hue = if max == r {
        60. * ((g - b) / delta).rem_euclid(6.)
    } else if max == g {
        60. * ((b - r) / delta + 2.)
    } else {
        60. * ((r - g) / delta + 4.)
    };
    (hue, delta / max)
}

/// True when the store already holds this connection (same id, or same engine and target).
pub fn exists(store: &Store, c: &ConnectionConfig) -> bool {
    store.connections.iter().any(|e| {
        e.id == c.id
            || (e.kind == c.kind
                && e.host == c.host
                && e.port == c.port
                && e.user == c.user
                && e.database == c.database
                && e.file_path == c.file_path
                && e.ssh.host == c.ssh.host)
    })
}

/// Add connections to the store, creating groups by name. Returns the ids that were added.
pub fn merge(store: &mut Store, imported: Vec<Imported>) -> Vec<String> {
    let mut added = Vec::new();
    for Imported { mut config, group, .. } in imported {
        if exists(store, &config) {
            continue;
        }
        config.group = match group {
            Some(name) => Some(match store.groups.iter().find(|g| g.name == name) {
                Some(g) => g.id.clone(),
                None => {
                    let g = Group::new(&name);
                    let id = g.id.clone();
                    store.groups.push(g);
                    id
                }
            }),
            None => config.group.filter(|id| store.groups.iter().any(|g| &g.id == id)),
        };
        added.push(config.id.clone());
        store.connections.push(config);
    }
    added
}

pub fn write_pueo(path: &Path, store: &Store) -> Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(store)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn converts_tableplus_connections() {
        let list = json!([
            {"ID": "A", "ConnectionName": "prod", "Driver": "PostgreSQL", "DatabaseHost": "127.0.0.1",
             "DatabasePort": "5433", "DatabaseUser": "app", "DatabaseName": "shop", "tLSMode": 1,
             "isOverSSH": true, "ServerAddress": "bastion", "ServerPort": "2222", "ServerUser": "root",
             "isUsePrivateKey": true, "ServerPrivateKeyName": "/keys/id", "Enviroment": "production",
             "statusColor": "#E01B24"},
            {"ID": "B", "ConnectionName": "remote file", "Driver": "SQLite", "isOverSSH": true},
            {"ID": "C", "ConnectionName": "mongo", "Driver": "MongoDB"},
        ]);
        let batch = tableplus(&list).unwrap();
        assert_eq!(batch.connections.len(), 1);
        let c = &batch.connections[0].config;
        assert_eq!((c.id.as_str(), c.port, c.database.as_str(), c.use_tls), ("tableplus-A", 5433, "shop", true));
        assert_eq!((c.ssh.port, c.ssh.auth, c.ssh.key_path.as_str()), (2222, SshAuth::Key, "/keys/id"));
        assert_eq!((c.tag, c.color), (Some(Tag::Production), Some(Color::Red)));
        assert_eq!(batch.skipped, vec!["remote file (SQLite over SSH)", "mongo (MongoDB)"]);
        assert_eq!(nearest_color("#A3793E"), Some(Color::Orange));
        assert_eq!(nearest_color("#005392"), Some(Color::Blue));
        assert_eq!(nearest_color("#686B6F"), Some(Color::Gray));

        let mut store = Store::default();
        assert_eq!(merge(&mut store, tableplus(&list).unwrap().connections).len(), 1);
        assert_eq!(merge(&mut store, tableplus(&list).unwrap().connections).len(), 0);
    }

    #[test]
    fn converts_dbeaver_connections() {
        let file = json!({"folders": {}, "connections": {
            "postgres-jdbc-1": {"provider": "postgresql", "driver": "postgres-jdbc", "name": "Shop", "folder": "Work",
                "configuration": {"host": "db.internal", "port": "5432", "database": "shop", "type": "prod",
                    "handlers": {"ssh_tunnel": {"enabled": true, "properties": {"host": "bastion", "port": 22, "authType": "PUBLIC_KEY", "keyPath": "/k"}}}}},
            "oracle-1": {"provider": "oracle", "name": "Legacy", "configuration": {}}
        }});
        let batch = dbeaver(&file).unwrap();
        assert_eq!(batch.connections.len(), 1);
        let i = &batch.connections[0];
        assert_eq!((i.config.host.as_str(), i.config.ssh.host.as_str(), i.group.as_deref()), ("db.internal", "bastion", Some("Work")));
        assert_eq!(i.config.tag, Some(Tag::Production));
        assert_eq!(batch.skipped, vec!["Legacy (oracle)"]);

        let mut store = Store::default();
        merge(&mut store, batch.connections);
        assert_eq!(store.groups.len(), 1);
        assert_eq!(store.connections[0].group.as_ref(), Some(&store.groups[0].id));
    }

    #[test]
    fn decrypts_tableplus_exports() {
        use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
        use hmac::Mac;
        let plain = serde_json::to_vec(&json!([{"ID": "X", "ConnectionName": "shop", "Driver": "PostgreSQL",
            "DatabaseHost": "db", "DatabasePassword": "s3cret", "isOverSSH": true, "ServerAddress": "b",
            "isUsePrivateKey": false, "ServerPassword": "sshpw"}]))
        .unwrap();
        let (enc_salt, hmac_salt, iv) = ([1u8; 8], [2u8; 8], [3u8; 16]);
        let derive = |salt: &[u8]| pbkdf2::pbkdf2_hmac_array::<sha1::Sha1, 32>(b"pw", salt, 10_000);
        let mut buf = plain.clone();
        buf.resize(plain.len() + 16, 0);
        let cipher = cbc::Encryptor::<aes::Aes256>::new(&derive(&enc_salt).into(), &iv.into())
            .encrypt_padded_mut::<Pkcs7>(&mut buf, plain.len())
            .unwrap()
            .to_vec();
        let mut data = vec![3u8, 1];
        data.extend_from_slice(&enc_salt);
        data.extend_from_slice(&hmac_salt);
        data.extend_from_slice(&iv);
        data.extend_from_slice(&cipher);
        let mut mac = <hmac::Hmac<sha2::Sha256> as Mac>::new_from_slice(&derive(&hmac_salt)).unwrap();
        mac.update(&data);
        data.extend_from_slice(&mac.finalize().into_bytes());

        assert_eq!(rncryptor_decrypt(&data, "pw").unwrap(), plain);
        assert!(rncryptor_decrypt(&data, "wrong").unwrap_err().downcast_ref::<NeedsPassword>().is_some());
        let path = std::env::temp_dir().join(format!("pueo-test-{}.tableplusconnection", std::process::id()));
        std::fs::write(&path, &data).unwrap();
        let batch = load_tableplus_export(&path, "pw").unwrap();
        let _ = std::fs::remove_file(&path);
        let i = &batch.connections[0];
        assert_eq!((i.password.as_deref(), i.ssh_secret.as_deref()), (Some("s3cret"), Some("sshpw")));
        assert_eq!(i.config.ssh.auth, SshAuth::Password);
    }

    #[test]
    fn parses_connection_urls() {
        let i = parse_url("postgresql://app:p%40ss@db.example.com:6543/shop?sslmode=require").unwrap();
        assert_eq!((i.config.user.as_str(), i.password.as_deref()), ("app", Some("p@ss")));
        assert_eq!((i.config.host.as_str(), i.config.port, i.config.database.as_str(), i.config.use_tls), ("db.example.com", 6543, "shop", true));
        let r = parse_url("rediss://cache:6380/2").unwrap();
        assert_eq!((r.config.kind, r.config.port, r.config.use_tls, r.config.database.as_str()), (DbKind::Redis, 6380, true, "2"));
        let s = parse_url("sqlite:///tmp/app.db").unwrap();
        assert_eq!(s.config.file_path, "/tmp/app.db");
        let batch = urls("# comment\nmysql://root@localhost/app\nnot a url\n").unwrap();
        assert_eq!((batch.connections.len(), batch.skipped.len()), (1, 1));
    }
}
