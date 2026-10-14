use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::db::DbKind;

/// A saved connection. Passwords are never written here; they live in the keyring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub id: String,
    pub name: String,
    pub kind: DbKind,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub database: String,
    #[serde(default)]
    pub file_path: String,
    /// PostgreSQL schema shown first (empty = public).
    #[serde(default)]
    pub schema: String,
    #[serde(default)]
    pub use_tls: bool,
    #[serde(default)]
    pub ssh: SshConfig,
    /// Row colour on the home page.
    #[serde(default)]
    pub color: Option<Color>,
    #[serde(default)]
    pub tag: Option<Tag>,
    /// Id of the group this connection belongs to.
    #[serde(default)]
    pub group: Option<String>,
}

/// The fixed palette used for connections and groups (GNOME palette).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Color {
    Red,
    Orange,
    Yellow,
    Green,
    Teal,
    Blue,
    Purple,
    Gray,
}

impl Color {
    pub const ALL: [Color; 8] =
        [Color::Red, Color::Orange, Color::Yellow, Color::Green, Color::Teal, Color::Blue, Color::Purple, Color::Gray];

    /// The swatch colour (GNOME palette), as 0-1 RGB.
    pub fn rgb(self) -> (f64, f64, f64) {
        let hex: u32 = match self {
            Color::Red => 0xe01b24,
            Color::Orange => 0xff7800,
            Color::Yellow => 0xf5c211,
            Color::Green => 0x2ec27e,
            Color::Teal => 0x2190a4,
            Color::Blue => 0x3584e4,
            Color::Purple => 0x9141ac,
            Color::Gray => 0x77767b,
        };
        let c = |shift: u32| ((hex >> shift) & 0xff) as f64 / 255.0;
        (c(16), c(8), c(0))
    }

    pub fn label(self) -> &'static str {
        match self {
            Color::Red => "Red",
            Color::Orange => "Orange",
            Color::Yellow => "Yellow",
            Color::Green => "Green",
            Color::Teal => "Teal",
            Color::Blue => "Blue",
            Color::Purple => "Purple",
            Color::Gray => "Gray",
        }
    }
}

/// Environment tag shown as a pill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tag {
    Local,
    Development,
    Testing,
    Staging,
    Production,
}

impl Tag {
    pub const ALL: [Tag; 5] = [Tag::Local, Tag::Development, Tag::Testing, Tag::Staging, Tag::Production];

    pub fn label(self) -> &'static str {
        match self {
            Tag::Local => "Local",
            Tag::Development => "Development",
            Tag::Testing => "Testing",
            Tag::Staging => "Staging",
            Tag::Production => "Production",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Tag::Local => "LOCAL",
            Tag::Development => "DEV",
            Tag::Testing => "TEST",
            Tag::Staging => "STAGING",
            Tag::Production => "PROD",
        }
    }

    /// Colour used when the connection has no colour of its own.
    pub fn color(self) -> Color {
        match self {
            Tag::Local => Color::Gray,
            Tag::Development => Color::Green,
            Tag::Testing => Color::Purple,
            Tag::Staging => Color::Orange,
            Tag::Production => Color::Red,
        }
    }
}

/// A group icon: a symbolic icon name, an emoji, or an image file copied into the config dir.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "lowercase")]
pub enum Icon {
    Symbol(String),
    Emoji(String),
    Image(String),
}

impl Default for Icon {
    fn default() -> Self {
        Icon::Symbol("folder-symbolic".into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub icon: Icon,
    #[serde(default)]
    pub color: Option<Color>,
    #[serde(default)]
    pub collapsed: bool,
}

impl Group {
    pub fn new(name: &str) -> Self {
        Group {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            icon: Icon::default(),
            color: Some(Color::Blue),
            collapsed: false,
        }
    }
}

/// Everything shown on the home page. Order in the vectors is display order.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub groups: Vec<Group>,
    #[serde(default)]
    pub connections: Vec<ConnectionConfig>,
}

/// Where a dragged connection is dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionDrop {
    Before(String),
    After(String),
    /// At the end of a group (`None` = ungrouped).
    IntoGroup(Option<String>),
}

impl Store {
    /// Move a connection; it takes the group of wherever it lands.
    pub fn move_connection(&mut self, id: &str, target: &ConnectionDrop) {
        if matches!(target, ConnectionDrop::Before(t) | ConnectionDrop::After(t) if t == id) {
            return;
        }
        let Some(from) = self.connections.iter().position(|c| c.id == id) else { return };
        let mut conn = self.connections.remove(from);
        let (index, group) = match target {
            ConnectionDrop::Before(t) | ConnectionDrop::After(t) => {
                let Some(pos) = self.connections.iter().position(|c| &c.id == t) else {
                    self.connections.insert(from, conn);
                    return;
                };
                let group = self.connections[pos].group.clone();
                (if matches!(target, ConnectionDrop::After(_)) { pos + 1 } else { pos }, group)
            }
            ConnectionDrop::IntoGroup(g) => {
                let last = self.connections.iter().rposition(|c| &c.group == g);
                (last.map_or(self.connections.len(), |p| p + 1), g.clone())
            }
        };
        conn.group = group;
        self.connections.insert(index, conn);
    }

    /// Reorder groups: put `id` before or after `target`.
    pub fn move_group(&mut self, id: &str, target: &str, after: bool) {
        if id == target {
            return;
        }
        let Some(from) = self.groups.iter().position(|g| g.id == id) else { return };
        let group = self.groups.remove(from);
        let Some(pos) = self.groups.iter().position(|g| g.id == target) else {
            self.groups.insert(from, group);
            return;
        };
        self.groups.insert(if after { pos + 1 } else { pos }, group);
    }

    /// Remove a group; its connections become ungrouped.
    pub fn delete_group(&mut self, id: &str) {
        self.groups.retain(|g| g.id != id);
        for c in &mut self.connections {
            if c.group.as_deref() == Some(id) {
                c.group = None;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SshAuth {
    /// ssh-agent and the default keys / ~/.ssh/config.
    #[default]
    Agent,
    Password,
    /// A specific private key; the stored SSH password is used as its passphrase.
    Key,
}

impl SshAuth {
    pub const ALL: [SshAuth; 3] = [SshAuth::Agent, SshAuth::Password, SshAuth::Key];

    pub fn label(self) -> &'static str {
        match self {
            SshAuth::Agent => "SSH Agent / Default Keys",
            SshAuth::Password => "Password",
            SshAuth::Key => "Private Key",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SshConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    pub key_path: String,
}

impl Default for SshConfig {
    fn default() -> Self {
        SshConfig {
            enabled: false,
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: SshAuth::Agent,
            key_path: String::new(),
        }
    }
}

impl ConnectionConfig {
    pub fn new(kind: DbKind) -> Self {
        ConnectionConfig {
            id: uuid::Uuid::new_v4().to_string(),
            name: String::new(),
            kind,
            host: if kind == DbKind::Sqlite { String::new() } else { "localhost".into() },
            port: kind.default_port(),
            user: String::new(),
            database: String::new(),
            file_path: String::new(),
            schema: String::new(),
            use_tls: false,
            ssh: SshConfig::default(),
            color: None,
            tag: None,
            group: None,
        }
    }

    pub fn summary(&self) -> String {
        match self.kind {
            DbKind::Sqlite => self.file_path.clone(),
            _ => {
                let user = if self.user.is_empty() { String::new() } else { format!("{}@", self.user) };
                let db = if self.database.is_empty() { String::new() } else { format!("/{}", self.database) };
                let via = if self.ssh.enabled { format!(" via ssh {}", self.ssh.host) } else { String::new() };
                format!("{user}{}:{}{db}{via}", self.host, self.port)
            }
        }
    }

    /// The row colour: explicit colour, else the tag's colour.
    pub fn effective_color(&self) -> Option<Color> {
        self.color.or(self.tag.map(Tag::color))
    }

    pub fn display_name(&self) -> String {
        if !self.name.trim().is_empty() {
            return self.name.clone();
        }
        match self.kind {
            DbKind::Sqlite => std::path::Path::new(&self.file_path)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| "SQLite".into()),
            _ => self.host.clone(),
        }
    }
}

pub fn config_dir() -> PathBuf {
    crate::platform::config_base().join("pueo")
}

/// Where query history lives.
pub fn data_dir() -> PathBuf {
    crate::platform::data_base().join("pueo")
}

/// Move settings and history saved under the app's former name ("tabula"), once.
pub fn migrate_from_tabula() {
    let moves = [
        (crate::platform::config_base().join("tabula"), config_dir()),
        (crate::platform::data_base().join("tabula"), data_dir()),
    ];
    for (old, new) in moves {
        if old.is_dir() && !new.exists() {
            if let Err(e) = std::fs::rename(&old, &new) {
                eprintln!("could not move {} to {}: {e}", old.display(), new.display());
            }
        }
    }
    // Custom group images are referenced by absolute path.
    let (old_icons, new_icons) = (crate::platform::config_base().join("tabula").join("icons"), icons_dir());
    let (old_icons, new_icons) = (old_icons.to_string_lossy(), new_icons.to_string_lossy());
    if let Ok(text) = std::fs::read_to_string(config_path())
        && text.contains(old_icons.as_ref())
    {
        let tmp = config_path().with_extension("json.tmp");
        let fixed = text.replace(old_icons.as_ref(), new_icons.as_ref());
        if std::fs::write(&tmp, fixed).and_then(|_| std::fs::rename(&tmp, config_path())).is_err() {
            eprintln!("could not update group image paths");
        }
    }
}

fn config_path() -> PathBuf {
    config_dir().join("connections.json")
}

/// Directory holding copies of custom group images.
pub fn icons_dir() -> PathBuf {
    config_dir().join("icons")
}

/// Load the store; older files contain only an array of connections.
pub fn load_store() -> Store {
    let Ok(data) = std::fs::read(config_path()) else { return Store::default() };
    if let Ok(store) = serde_json::from_slice::<Store>(&data) {
        return store;
    }
    let connections: Vec<ConnectionConfig> = serde_json::from_slice(&data).unwrap_or_default();
    let mut store = Store { groups: Vec::new(), connections };
    // Drop references to groups that do not exist.
    for c in &mut store.connections {
        c.group = None;
    }
    store
}

pub fn save_store(store: &Store) -> Result<()> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Write atomically so a crash never leaves a half-written file.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(store)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn sidebar_state_path() -> PathBuf {
    config_dir().join("sidebar.json")
}

type SidebarState = std::collections::HashMap<String, std::collections::HashMap<String, bool>>;

fn load_sidebar_state() -> SidebarState {
    std::fs::read(sidebar_state_path()).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
}

/// Which sidebar sections (functions, tables, views, Redis key types) were open for a connection.
pub fn sidebar_sections(connection_id: &str) -> std::collections::HashMap<String, bool> {
    load_sidebar_state().remove(connection_id).unwrap_or_default()
}

pub fn save_sidebar_sections(connection_id: &str, sections: &std::collections::HashMap<String, bool>) -> Result<()> {
    let mut state = load_sidebar_state();
    state.insert(connection_id.to_string(), sections.clone());
    let path = sidebar_state_path();
    std::fs::create_dir_all(config_dir())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&state)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

/// 0 follows the system, 1 light, 2 dark.
pub fn appearance() -> i32 {
    std::fs::read(settings_path())
        .ok()
        .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
        .and_then(|v| v.get("appearance")?.as_i64())
        .map_or(0, |v| v as i32)
}

pub fn save_appearance(mode: i32) -> Result<()> {
    let path = settings_path();
    let mut settings = std::fs::read(&path)
        .ok()
        .and_then(|d| serde_json::from_slice::<serde_json::Value>(&d).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    settings["appearance"] = mode.into();
    std::fs::create_dir_all(config_dir())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&settings)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn pins_path() -> PathBuf {
    config_dir().join("pins.json")
}

/// Pinned sidebar objects as `[schema, name]`, keyed by "connection-id/database".
type PinState = std::collections::HashMap<String, Vec<(String, String)>>;

fn load_pins() -> PinState {
    std::fs::read(pins_path()).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
}

fn pins_key(connection_id: &str, database: &str) -> String {
    format!("{connection_id}/{database}")
}

/// Objects pinned to the top of the sidebar, per connection and database.
pub fn pinned_objects(connection_id: &str, database: &str) -> std::collections::HashSet<(String, String)> {
    load_pins().remove(&pins_key(connection_id, database)).unwrap_or_default().into_iter().collect()
}

pub fn save_pinned_objects(
    connection_id: &str,
    database: &str,
    pins: &std::collections::HashSet<(String, String)>,
) -> Result<()> {
    let mut state = load_pins();
    let key = pins_key(connection_id, database);
    if pins.is_empty() {
        state.remove(&key);
    } else {
        let mut list: Vec<(String, String)> = pins.iter().cloned().collect();
        list.sort();
        state.insert(key, list);
    }
    std::fs::create_dir_all(config_dir())?;
    let path = pins_path();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&state)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// Copy an image into the config dir so the group keeps working if the original moves.
pub fn import_icon_image(src: &std::path::Path) -> Result<String> {
    let dir = icons_dir();
    std::fs::create_dir_all(&dir)?;
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("png").to_lowercase();
    let dest = dir.join(format!("{}.{ext}", uuid::Uuid::new_v4()));
    std::fs::copy(src, &dest)?;
    Ok(dest.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(id: &str, group: Option<&str>) -> ConnectionConfig {
        let mut c = ConnectionConfig::new(DbKind::Sqlite);
        c.id = id.into();
        c.group = group.map(Into::into);
        c
    }

    fn order(s: &Store) -> Vec<(String, Option<String>)> {
        s.connections.iter().map(|c| (c.id.clone(), c.group.clone())).collect()
    }

    #[test]
    fn moves_connections() {
        let mut s = Store {
            groups: vec![Group::new("A"), Group::new("B")],
            connections: vec![conn("1", None), conn("2", Some("g")), conn("3", Some("g")), conn("4", None)],
        };
        s.move_connection("1", &ConnectionDrop::After("3".into()));
        assert_eq!(order(&s)[2], ("1".into(), Some("g".into())), "joins the target's group");
        s.move_connection("3", &ConnectionDrop::Before("2".into()));
        assert_eq!(order(&s).iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["3", "2", "1", "4"]);
        s.move_connection("4", &ConnectionDrop::IntoGroup(Some("g".into())));
        assert_eq!(order(&s)[3], ("4".into(), Some("g".into())));
        s.move_connection("2", &ConnectionDrop::IntoGroup(None));
        assert_eq!(order(&s).last().unwrap(), &("2".to_string(), None));
        s.move_connection("2", &ConnectionDrop::Before("2".into()));
        assert_eq!(s.connections.len(), 4);
    }

    #[test]
    fn moves_and_deletes_groups() {
        let (a, b, c) = (Group::new("A"), Group::new("B"), Group::new("C"));
        let ids = [a.id.clone(), b.id.clone(), c.id.clone()];
        let mut s = Store { groups: vec![a, b, c], connections: vec![conn("1", Some(&ids[1]))] };
        s.move_group(&ids[0], &ids[2], true);
        assert_eq!(s.groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["B", "C", "A"]);
        s.move_group(&ids[0], &ids[1], false);
        assert_eq!(s.groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["A", "B", "C"]);
        s.delete_group(&ids[1]);
        assert_eq!(s.groups.len(), 2);
        assert_eq!(s.connections[0].group, None);
    }

    #[test]
    fn reads_old_format() {
        let old = r#"[{"id":"x","name":"Old","kind":"sqlite","file_path":"/tmp/a.db"}]"#;
        let conns: Vec<ConnectionConfig> = serde_json::from_str(old).unwrap();
        assert_eq!(conns[0].color, None);
        assert!(serde_json::from_str::<Store>(old).is_err(), "old format falls back to the array parser");
        let icon = serde_json::to_string(&Icon::Emoji("🚀".into())).unwrap();
        assert_eq!(icon, r#"{"type":"emoji","value":"🚀"}"#);
    }
}
