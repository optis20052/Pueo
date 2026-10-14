//! Toolkit-independent workspace state and transactional grid edits.
use super::completion::SchemaCache;
use crate::{
    config::ConnectionConfig,
    db::{self, Cell, Choices, DbObject, FetchOptions, QueryResult, RowChange, SharedDriver},
};
use std::collections::HashSet;
use std::sync::Arc;

pub const PAGE_SIZE: usize = 300;

#[derive(Debug, Clone)]
pub struct GridRow {
    pub original: Option<Vec<Cell>>,
    pub current: Vec<Cell>,
    pub deleted: bool,
}
impl GridRow {
    pub fn dirty(&self) -> bool {
        self.deleted || self.original.as_ref() != Some(&self.current)
    }
}
#[derive(Debug, Clone, Default)]
pub struct Grid {
    pub columns: Vec<String>,
    pub rows: Vec<GridRow>,
    pub keys: Option<Vec<String>>,
    pub choices: Choices,
    /// The focused cell (row, column).
    pub selected: Option<(usize, usize)>,
    /// Rows picked with Cmd/Ctrl- or Shift-click, in addition to the focused one.
    pub marked: std::collections::BTreeSet<usize>,
    /// Where a Shift-click range starts.
    pub anchor: Option<usize>,
}
impl Grid {
    pub fn from_result(result: &QueryResult) -> Self {
        Self {
            columns: result.columns.clone(),
            rows: result
                .rows
                .iter()
                .map(|r| GridRow {
                    original: Some(r.clone()),
                    current: r.clone(),
                    deleted: false,
                })
                .collect(),
            ..Default::default()
        }
    }
    pub fn pending(&self) -> usize {
        self.rows.iter().filter(|r| r.dirty()).count()
    }
    pub fn editable(&self) -> bool {
        self.keys
            .as_ref()
            .is_some_and(|keys| !keys.is_empty() && keys.iter().all(|k| self.columns.contains(k)))
    }
    pub fn set_cell(&mut self, row: usize, col: usize, value: Cell) {
        if !self.editable() {
            return;
        }
        if let Some(r) = self.rows.get_mut(row).filter(|r| !r.deleted)
            && let Some(cell) = r.current.get_mut(col)
        {
            *cell = value;
        }
    }
    /// Focus a cell. `mode` 0 selects only its row, 1 toggles the row (Cmd/Ctrl-click),
    /// 2 extends from the anchor (Shift-click), 3 keeps the selection.
    pub fn select(&mut self, row: usize, col: usize, mode: i32) {
        if row >= self.rows.len() {
            return;
        }
        match mode {
            1 => {
                if self.marked.is_empty()
                    && let Some((r, _)) = self.selected
                {
                    self.marked.insert(r);
                }
                if !self.marked.remove(&row) {
                    self.marked.insert(row);
                }
                self.anchor = Some(row);
            }
            // Right-click inside the current selection: keep it, just move the focus.
            3 => {}
            2 => {
                let anchor = self.anchor.or(self.selected.map(|(r, _)| r)).unwrap_or(row);
                self.marked = (anchor.min(row)..=anchor.max(row)).collect();
            }
            _ => {
                self.marked = [row].into();
                self.anchor = Some(row);
            }
        }
        self.selected = Some((row, col));
    }
    /// Rows the row actions apply to, in order.
    pub fn selected_rows(&self) -> Vec<usize> {
        if self.marked.is_empty() {
            self.selected.map(|(r, _)| vec![r]).unwrap_or_default()
        } else {
            self.marked.iter().copied().filter(|r| *r < self.rows.len()).collect()
        }
    }
    pub fn duplicate_selected(&mut self) {
        for row in self.selected_rows() {
            self.add(Some(row));
        }
    }
    /// Add rows from tab-separated text (one row per line, `NULL` for null). Key columns stay
    /// empty so the database assigns them. Returns how many rows were added.
    pub fn paste_rows(&mut self, text: &str) -> usize {
        if !self.editable() {
            return 0;
        }
        let mut added = 0;
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let fields: Vec<&str> = line.split('\t').collect();
            let mut current: Vec<Cell> = (0..self.columns.len())
                .map(|i| match fields.get(i) {
                    Some(&"NULL") | None => None,
                    Some(v) => Some(v.to_string()),
                })
                .collect();
            for (i, name) in self.columns.iter().enumerate() {
                if self.keys.as_ref().is_some_and(|keys| keys.contains(name)) {
                    current[i] = None;
                }
            }
            self.rows.push(GridRow { original: None, current, deleted: false });
            added += 1;
        }
        if added > 0 {
            let first = self.rows.len() - added;
            self.marked = (first..self.rows.len()).collect();
            self.selected = Some((first, 0));
            self.anchor = Some(first);
        }
        added
    }
    pub fn add(&mut self, duplicate: Option<usize>) {
        if !self.editable() {
            return;
        }
        let mut current = duplicate
            .and_then(|i| self.rows.get(i))
            .map_or_else(|| vec![None; self.columns.len()], |r| r.current.clone());
        for (i, name) in self.columns.iter().enumerate() {
            if self.keys.as_ref().is_some_and(|keys| keys.contains(name)) {
                current[i] = None;
            }
        }
        self.rows.push(GridRow {
            original: None,
            current,
            deleted: false,
        });
        self.selected = Some((self.rows.len() - 1, 0));
        self.marked = [self.rows.len() - 1].into();
        self.anchor = Some(self.rows.len() - 1);
    }
    /// Mark the selected rows for deletion (or unmark them when all are marked already).
    /// Rows that were only added here are dropped right away.
    pub fn delete_selected(&mut self) {
        if !self.editable() {
            return;
        }
        let rows = self.selected_rows();
        let restore = rows.iter().all(|&i| self.rows.get(i).is_some_and(|r| r.deleted || r.original.is_none()))
            && rows.iter().any(|&i| self.rows[i].deleted);
        for &i in rows.iter().rev() {
            let Some(row) = self.rows.get_mut(i) else { continue };
            if row.original.is_none() {
                self.rows.remove(i);
            } else {
                row.deleted = !restore;
            }
        }
        if rows.iter().any(|&i| i >= self.rows.len()) || self.selected.is_some_and(|(r, _)| r >= self.rows.len()) {
            self.selected = None;
            self.marked.clear();
        }
    }
    pub fn discard(&mut self) {
        self.rows.retain(|r| r.original.is_some());
        for r in &mut self.rows {
            r.current = r.original.clone().unwrap();
            r.deleted = false;
        }
        self.selected = None;
        self.marked.clear();
        self.anchor = None;
    }
    pub fn changes(&self) -> anyhow::Result<Vec<RowChange>> {
        anyhow::ensure!(
            self.editable(),
            "This object has no primary key and is read-only"
        );
        let mut out = Vec::new();
        for r in &self.rows {
            let values = || {
                self.columns
                    .iter()
                    .cloned()
                    .zip(r.current.iter().cloned())
                    .collect::<Vec<_>>()
            };
            match &r.original {
                None if !r.deleted => {
                    // Unset insert fields use database defaults, as in the GTK grid.
                    let values = values().into_iter().filter(|(_, v)| v.is_some()).collect();
                    out.push(RowChange::Insert { values });
                }
                Some(original) if r.dirty() => {
                    let key = self
                        .keys
                        .as_ref()
                        .unwrap()
                        .iter()
                        .map(|k| {
                            let index = self.columns.iter().position(|c| c == k).unwrap();
                            (k.clone(), original[index].clone())
                        })
                        .collect();
                    if r.deleted {
                        out.push(RowChange::Delete { key });
                    } else {
                        let set = self
                            .columns
                            .iter()
                            .enumerate()
                            .filter(|(i, _)| original.get(*i) != r.current.get(*i))
                            .map(|(i, c)| (c.clone(), r.current[i].clone()))
                            .collect();
                        out.push(RowChange::Update {
                            key,
                            set,
                            row: values(),
                        });
                    }
                }
                _ => {}
            }
        }
        // Free unique keys before updates and inserts that reuse them.
        out.sort_by_key(|change| match change {
            RowChange::Delete { .. } => 0,
            RowChange::Update { .. } => 1,
            RowChange::Insert { .. } => 2,
        });
        Ok(out)
    }
    pub fn values(&self) -> Vec<Vec<Cell>> {
        self.rows
            .iter()
            .filter(|r| !r.deleted)
            .map(|r| r.current.clone())
            .collect()
    }
}

pub struct TableState {
    pub object: DbObject,
    pub options: FetchOptions,
    pub total: u64,
    pub view: i32,
    pub data: Grid,
    pub metadata: Grid,
    pub filter_text: String,
}
pub struct QueryState {
    pub sql: String,
    pub results: Vec<QueryResult>,
    pub result: usize,
    pub grid: Grid,
}
pub enum TabContent {
    Table(TableState),
    Query(QueryState),
}
pub struct Tab {
    pub id: u64,
    pub title: String,
    pub content: TabContent,
    pub busy: bool,
    pub status: String,
}
impl Tab {
    pub fn grid(&self) -> &Grid {
        match &self.content {
            TabContent::Table(t) if t.view == 0 => &t.data,
            TabContent::Table(t) => &t.metadata,
            TabContent::Query(q) => &q.grid,
        }
    }
    pub fn grid_mut(&mut self) -> &mut Grid {
        match &mut self.content {
            TabContent::Table(t) => {
                if t.view == 0 {
                    &mut t.data
                } else {
                    &mut t.metadata
                }
            }
            TabContent::Query(q) => &mut q.grid,
        }
    }
    pub fn pending(&self) -> usize {
        match &self.content {
            TabContent::Table(t) => t.data.pending(),
            _ => 0,
        }
    }
}
pub struct Workspace {
    pub config: ConnectionConfig,
    pub driver: SharedDriver,
    pub log: Arc<db::log::QueryLog>,
    pub info: String,
    pub databases: Vec<String>,
    pub objects: Vec<DbObject>,
    pub sections: std::collections::HashMap<String, bool>,
    pub expanded: std::collections::HashMap<(String, String), QueryResult>,
    pub truncated: bool,
    pub pins: HashSet<(String, String)>,
    pub schema: SchemaCache,
    pub tabs: Vec<Tab>,
    pub active_tab: Option<u64>,
    pub busy: bool,
    /// PostgreSQL schema shown in the sidebar ("" = the default, "*" = all schemas).
    pub schema_filter: String,
    // Secrets remain only in memory when the user opts out of keyring storage.
    pub password: Option<String>,
    pub ssh_secret: Option<String>,
}
impl Workspace {
    pub fn active(&self) -> Option<&Tab> {
        self.tabs.iter().find(|t| Some(t.id) == self.active_tab)
    }
    pub fn active_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| Some(t.id) == self.active_tab)
    }
    pub fn busy(&self) -> bool {
        self.busy || self.tabs.iter().any(|t| t.busy)
    }
    pub fn pending(&self) -> usize {
        self.tabs.iter().map(Tab::pending).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn grid() -> Grid {
        let mut g = Grid::from_result(&QueryResult::from_rows(
            &["id", "tenant", "note"],
            vec![vec![Some("1".into()), Some("A".into()), None]],
        ));
        g.keys = Some(vec!["id".into(), "tenant".into()]);
        g
    }
    #[test]
    fn changed_primary_key_still_addresses_original_row() {
        let mut g = grid();
        g.set_cell(0, 0, Some("2".into()));
        g.set_cell(0, 2, Some("".into()));
        match &g.changes().unwrap()[0] {
            RowChange::Update { key, set, .. } => {
                assert_eq!(
                    key,
                    &vec![
                        ("id".into(), Some("1".into())),
                        ("tenant".into(), Some("A".into()))
                    ]
                );
                assert_eq!(set.len(), 2);
                assert_eq!(set[1].1, Some("".into()));
            }
            _ => panic!("expected update"),
        }
        g.selected = Some((0, 0));
        g.delete_selected();
        assert!(
            matches!(&g.changes().unwrap()[0], RowChange::Delete { key } if key[0].1 == Some("1".into()))
        );
        g.delete_selected();
        g.discard();
        assert_eq!(g.pending(), 0);
        assert_eq!(g.rows[0].current[2], None);
    }
    #[test]
    fn new_rows_duplicates_and_read_only_tables() {
        let mut g = grid();
        g.add(Some(0));
        assert_eq!(g.rows[1].current, vec![None, None, None]);
        assert!(
            matches!(&g.changes().unwrap()[0], RowChange::Insert { values } if values.is_empty())
        );
        g.delete_selected();
        assert_eq!(g.rows.len(), 1);
        g.keys = None;
        g.add(None);
        g.set_cell(0, 0, None);
        assert_eq!(g.rows.len(), 1);
        assert_eq!(g.rows[0].current[0], Some("1".into()));
        assert!(g.changes().is_err());
    }
    #[tokio::test]
    async fn sqlite_commit_round_trip_and_failed_commit_keeps_edits() {
        let cfg = ConnectionConfig {
            file_path: ":memory:".into(),
            ..ConnectionConfig::new(db::DbKind::Sqlite)
        };
        let driver = db::connect(&cfg, None, None).await.unwrap();
        driver.execute("CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, state TEXT DEFAULT 'active'); INSERT INTO items(id,name) VALUES(1, 'one');").await.unwrap();
        let obj = driver
            .list_objects(None)
            .await
            .unwrap()
            .objects
            .into_iter()
            .find(|o| o.name == "items")
            .unwrap();
        let opts = FetchOptions {
            limit: PAGE_SIZE,
            ..Default::default()
        };
        let mut g = Grid::from_result(&driver.fetch_rows(&obj, &opts).await.unwrap());
        g.keys = driver.primary_key(&obj).await.unwrap();
        g.set_cell(0, 1, Some("two".into()));
        g.add(None);
        g.set_cell(1, 1, Some("three".into()));
        driver
            .apply_changes(&obj, &g.changes().unwrap())
            .await
            .unwrap();
        assert_eq!(driver.count_rows(&obj, None).await.unwrap(), 2);
        assert_eq!(
            driver.fetch_rows(&obj, &opts).await.unwrap().rows[1][2],
            Some("active".into()),
            "unset insert fields use SQL defaults"
        );
        let mut g = Grid::from_result(&driver.fetch_rows(&obj, &opts).await.unwrap());
        g.keys = Some(vec!["id".into()]);
        g.set_cell(0, 1, Some("four".into()));
        g.set_cell(1, 1, None);
        assert!(
            driver
                .apply_changes(&obj, &g.changes().unwrap())
                .await
                .is_err()
        );
        assert_eq!(g.pending(), 2);
        assert_eq!(
            driver.fetch_rows(&obj, &opts).await.unwrap().rows[0][1],
            Some("two".into()),
            "transaction rolled back"
        );
        let mut g = Grid::from_result(&driver.fetch_rows(&obj, &opts).await.unwrap());
        g.keys = Some(vec!["id".into()]);
        g.set_cell(0, 1, Some("three".into()));
        g.selected = Some((1, 0));
        g.delete_selected();
        driver
            .apply_changes(&obj, &g.changes().unwrap())
            .await
            .unwrap();
        assert_eq!(
            driver.count_rows(&obj, None).await.unwrap(),
            1,
            "delete frees the unique value before update"
        );
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;

    fn grid() -> Grid {
        let mut g = Grid::from_result(&QueryResult {
            columns: vec!["id".into(), "name".into()],
            rows: (0..5).map(|i| vec![Some(i.to_string()), Some(format!("n{i}"))]).collect(),
            ..Default::default()
        });
        g.keys = Some(vec!["id".into()]);
        g
    }

    #[test]
    fn selects_ranges_and_toggles() {
        let mut g = grid();
        g.select(1, 0, 0);
        g.select(3, 1, 2);
        assert_eq!(g.selected_rows(), vec![1, 2, 3]);
        g.select(2, 0, 1);
        assert_eq!(g.selected_rows(), vec![1, 3]);
        g.delete_selected();
        assert!(g.rows[1].deleted && g.rows[3].deleted && !g.rows[2].deleted);
        g.delete_selected();
        assert!(!g.rows[1].deleted && !g.rows[3].deleted);
    }

    #[test]
    fn pastes_tab_separated_rows() {
        let mut g = grid();
        assert_eq!(g.paste_rows("9\tnew\n10\tNULL\n"), 2);
        assert_eq!(g.rows.len(), 7);
        assert_eq!(g.rows[5].current, vec![None, Some("new".into())]);
        assert_eq!(g.rows[6].current, vec![None, None]);
        assert_eq!(g.selected_rows(), vec![5, 6]);
        g.duplicate_selected();
        assert_eq!(g.rows.len(), 9);
    }
}
