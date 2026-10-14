use std::time::Instant;

use anyhow::{Context, Result};
use async_trait::async_trait;
use postgres_native_tls::MakeTlsConnector;
use tokio_postgres::{CancelToken, Client, Config, NoTls, SimpleQueryMessage};

use super::{
    log,
    Choices, ColumnChoices, DbKind, DbObject, Driver, FetchOptions, ObjectKind, ObjectList, QueryResult, RowChange,
    change_statements, quote_double, quote_literal, select_tail, where_clause,
};
use crate::config::ConnectionConfig;

/// PostgreSQL errors as one readable sentence: "message. Detail. Hint." instead of
/// the raw "db error: ERROR: ..." text.
fn readable(e: tokio_postgres::Error) -> anyhow::Error {
    let Some(db) = e.as_db_error() else { return e.into() };
    let mut msg = db.message().to_string();
    if let Some(first) = msg.get(..1) {
        msg = first.to_uppercase() + &msg[1..];
    }
    for extra in [db.detail(), db.hint()].into_iter().flatten() {
        msg.push_str(". ");
        msg.push_str(extra.trim_end_matches('.'));
    }
    if !msg.ends_with('.') {
        msg.push('.');
    }
    anyhow::anyhow!(msg)
}

pub struct PgDriver {
    client: Client,
    cancel_token: CancelToken,
    tls: Option<MakeTlsConnector>,
}

impl PgDriver {
    pub async fn connect(cfg: &ConnectionConfig, password: Option<String>) -> Result<Self> {
        let mut pg = Config::new();
        pg.host(if cfg.host.is_empty() { "localhost" } else { &cfg.host })
            .port(cfg.port)
            .user(if cfg.user.is_empty() { "postgres" } else { &cfg.user })
            .dbname(if cfg.database.is_empty() { "postgres" } else { &cfg.database })
            .application_name("Pueo")
            .connect_timeout(std::time::Duration::from_secs(10));
        if let Some(pw) = password.filter(|p| !p.is_empty()) {
            pg.password(pw);
        }

        let (client, tls) = if cfg.use_tls {
            // Equivalent to libpq's sslmode=require: encrypt, don't verify the certificate.
            let tls = MakeTlsConnector::new(
                native_tls::TlsConnector::builder()
                    .danger_accept_invalid_certs(true)
                    .danger_accept_invalid_hostnames(true)
                    .build()?,
            );
            let (client, conn) = pg.connect(tls.clone()).await.context("Could not connect to PostgreSQL")?;
            tokio::spawn(async move {
                if let Err(e) = conn.await {
                    eprintln!("postgres connection error: {e}");
                }
            });
            (client, Some(tls))
        } else {
            let (client, conn) = pg.connect(NoTls).await.context("Could not connect to PostgreSQL")?;
            tokio::spawn(async move {
                if let Err(e) = conn.await {
                    eprintln!("postgres connection error: {e}");
                }
            });
            (client, None)
        };
        let cancel_token = client.cancel_token();
        Ok(PgDriver { client, cancel_token, tls })
    }

    fn qualified(obj: &DbObject) -> String {
        format!("{}.{}", quote_double(&obj.schema), quote_double(&obj.name))
    }

    async fn simple(&self, sql: &str) -> Result<Vec<QueryResult>> {
        let start = Instant::now();
        let messages = log::timed(sql, async { self.client.simple_query(sql).await.map_err(readable) }).await?;
        let elapsed = start.elapsed();

        let mut results = Vec::new();
        let mut current: Option<QueryResult> = None;
        for msg in messages {
            match msg {
                SimpleQueryMessage::RowDescription(cols) => {
                    current = Some(QueryResult {
                        columns: cols.iter().map(|c| c.name().to_string()).collect(),
                        ..Default::default()
                    });
                }
                SimpleQueryMessage::Row(row) => {
                    let cur = current.get_or_insert_with(|| QueryResult {
                        columns: row.columns().iter().map(|c| c.name().to_string()).collect(),
                        ..Default::default()
                    });
                    cur.rows.push((0..row.len()).map(|i| row.get(i).map(str::to_owned)).collect());
                }
                SimpleQueryMessage::CommandComplete(n) => {
                    let mut r = current.take().unwrap_or_default();
                    if !r.has_rows() {
                        r.affected = Some(n);
                    }
                    r.elapsed = elapsed;
                    results.push(r);
                }
                _ => {}
            }
        }
        if let Some(mut r) = current {
            r.elapsed = elapsed;
            results.push(r);
        }
        Ok(results)
    }

    async fn simple_one(&self, sql: &str) -> Result<QueryResult> {
        Ok(self.simple(sql).await?.into_iter().next().unwrap_or_default())
    }

    fn first_column(r: QueryResult) -> Vec<String> {
        r.rows.into_iter().filter_map(|row| row.into_iter().next().flatten()).collect()
    }
}

#[async_trait]
impl Driver for PgDriver {
    fn kind(&self) -> DbKind {
        DbKind::Postgres
    }

    async fn server_info(&self) -> Result<String> {
        let r = self.simple_one("SELECT current_database(), current_setting('server_version')").await?;
        let row = r.rows.first().cloned().unwrap_or_default();
        Ok(format!(
            "{} · PostgreSQL {}",
            row.first().cloned().flatten().unwrap_or_default(),
            row.get(1).cloned().flatten().unwrap_or_default()
        ))
    }

    async fn list_objects(&self, _pattern: Option<&str>) -> Result<ObjectList> {
        let r = self
            .simple_one(
                "SELECT table_schema::text, table_name::text, table_type::text, '' FROM information_schema.tables \
                 WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
                 AND table_schema NOT LIKE 'pg_toast%' \
                 UNION ALL \
                 SELECT schemaname::text, matviewname::text, 'VIEW', '' FROM pg_matviews \
                 UNION ALL \
                 SELECT n.nspname::text, p.proname::text, \
                        CASE p.prokind WHEN 'p' THEN 'PROCEDURE' ELSE 'FUNCTION' END, \
                        oidvectortypes(p.proargtypes) \
                 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
                 WHERE p.prokind IN ('f', 'p') \
                 AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
                 AND n.nspname NOT LIKE 'pg_toast%' \
                 AND NOT EXISTS (SELECT 1 FROM pg_depend d \
                                 WHERE d.classid = 'pg_proc'::regclass AND d.objid = p.oid AND d.deptype = 'e') \
                 ORDER BY 1, 2, 4",
            )
            .await?;
        let objects = r
            .rows
            .into_iter()
            .map(|row| DbObject {
                schema: row[0].clone().unwrap_or_default(),
                name: row[1].clone().unwrap_or_default(),
                kind: match row[2].as_deref() {
                    Some("BASE TABLE" | "LOCAL TEMPORARY") => ObjectKind::Table,
                    Some("FUNCTION") => ObjectKind::Function,
                    Some("PROCEDURE") => ObjectKind::Procedure,
                    _ => ObjectKind::View,
                },
                signature: row[3].clone().unwrap_or_default(),
            })
            .collect();
        Ok(ObjectList { objects, truncated: false })
    }

    async fn fetch_rows(&self, obj: &DbObject, opts: &FetchOptions) -> Result<QueryResult> {
        let sql = format!("SELECT * FROM {}{}", Self::qualified(obj), select_tail(opts, quote_double));
        self.simple_one(&sql).await
    }

    async fn count_rows(&self, obj: &DbObject, filter: Option<&str>) -> Result<u64> {
        let sql = format!("SELECT count(*) FROM {}{}", Self::qualified(obj), where_clause(filter));
        let r = self.simple_one(&sql).await?;
        Ok(r.rows
            .first()
            .and_then(|row| row[0].as_deref())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0))
    }

    async fn structure(&self, obj: &DbObject) -> Result<QueryResult> {
        let sql = format!(
            "SELECT c.column_name AS \"column\", \
                    format_type(a.atttypid, a.atttypmod) AS \"type\", \
                    c.is_nullable AS \"nullable\", \
                    c.column_default AS \"default\", \
                    CASE WHEN pk.column_name IS NOT NULL THEN 'PRI' END AS \"key\", \
                    (SELECT string_agg(fn.nspname || '.' || fc.relname || '(' || fa.attname || ')', ', ') \
                     FROM pg_constraint con \
                     JOIN pg_class fc ON fc.oid = con.confrelid \
                     JOIN pg_namespace fn ON fn.oid = fc.relnamespace \
                     JOIN LATERAL unnest(con.conkey, con.confkey) AS k(col, fcol) ON true \
                     JOIN pg_attribute fa ON fa.attrelid = con.confrelid AND fa.attnum = k.fcol \
                     WHERE con.contype = 'f' AND con.conrelid = a.attrelid AND k.col = a.attnum) AS \"references\", \
                    col_description(a.attrelid, a.attnum) AS \"comment\" \
             FROM information_schema.columns c \
             JOIN pg_attribute a ON a.attrelid = ({rel})::regclass AND a.attname = c.column_name \
             LEFT JOIN ( \
                 SELECT kcu.column_name FROM information_schema.table_constraints tc \
                 JOIN information_schema.key_column_usage kcu \
                   ON tc.constraint_name = kcu.constraint_name AND tc.table_schema = kcu.table_schema \
                 WHERE tc.constraint_type = 'PRIMARY KEY' AND tc.table_schema = {schema} AND tc.table_name = {table} \
             ) pk ON pk.column_name = c.column_name \
             WHERE c.table_schema = {schema} AND c.table_name = {table} \
             ORDER BY c.ordinal_position",
            rel = quote_literal(&Self::qualified(obj)),
            schema = quote_literal(&obj.schema),
            table = quote_literal(&obj.name),
        );
        self.simple_one(&sql).await
    }

    async fn execute(&self, sql: &str) -> Result<Vec<QueryResult>> {
        self.simple(sql).await
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        let r = self
            .simple_one("SELECT datname FROM pg_database WHERE NOT datistemplate AND datallowconn ORDER BY 1")
            .await?;
        Ok(Self::first_column(r))
    }

    async fn list_columns(&self) -> Result<Vec<(String, String)>> {
        let r = self
            .simple_one(
                "SELECT table_name, column_name FROM information_schema.columns \
                 WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
                 ORDER BY table_name, ordinal_position",
            )
            .await?;
        Ok(r.rows
            .into_iter()
            .map(|row| (row[0].clone().unwrap_or_default(), row[1].clone().unwrap_or_default()))
            .collect())
    }

    async fn primary_key(&self, obj: &DbObject) -> Result<Option<Vec<String>>> {
        if obj.kind != ObjectKind::Table {
            return Ok(None);
        }
        let r = self
            .simple_one(&format!(
                "SELECT a.attname FROM pg_index i \
                 JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) \
                 WHERE i.indrelid = ({})::regclass AND i.indisprimary \
                 ORDER BY array_position(i.indkey::int2[], a.attnum)",
                quote_literal(&Self::qualified(obj))
            ))
            .await?;
        let cols = Self::first_column(r);
        Ok((!cols.is_empty()).then_some(cols))
    }

    async fn apply_changes(&self, obj: &DbObject, changes: &[RowChange]) -> Result<u64> {
        let statements = change_statements(DbKind::Postgres, obj, changes)?;
        // A multi-statement simple query runs in one implicit transaction.
        let results = self.simple(&statements.join(";\n")).await?;
        Ok(results.iter().filter_map(|r| r.affected).sum())
    }

    async fn indexes(&self, obj: &DbObject) -> Result<QueryResult> {
        if obj.kind != ObjectKind::Table {
            return Ok(QueryResult::default());
        }
        let sql = format!(
            "SELECT i.relname AS \"name\", \
                    array_to_string(ARRAY(SELECT pg_get_indexdef(ix.indexrelid, k + 1, true) \
                        FROM generate_subscripts(ix.indkey, 1) AS k ORDER BY k), ', ') AS \"columns\", \
                    CASE WHEN ix.indisunique THEN 'YES' END AS \"unique\", \
                    CASE WHEN ix.indisprimary THEN 'YES' END AS \"primary\", \
                    am.amname AS \"type\", \
                    pg_get_indexdef(ix.indexrelid) AS \"definition\" \
             FROM pg_index ix \
             JOIN pg_class i ON i.oid = ix.indexrelid \
             JOIN pg_am am ON am.oid = i.relam \
             WHERE ix.indrelid = ({})::regclass \
             ORDER BY ix.indisprimary DESC, i.relname",
            quote_literal(&Self::qualified(obj))
        );
        self.simple_one(&sql).await
    }

    async fn definition(&self, obj: &DbObject) -> Result<String> {
        // The argument types pick the right overload.
        let target = format!("{}({})", Self::qualified(obj), obj.signature);
        let r = self.simple_one(&format!("SELECT pg_get_functiondef({}::regprocedure)", quote_literal(&target))).await?;
        r.rows.into_iter().next().and_then(|row| row.into_iter().next().flatten()).context("Function not found")
    }

    async fn column_choices(&self, obj: &DbObject) -> Result<Choices> {
        if obj.kind != ObjectKind::Table {
            return Ok(Choices::new());
        }
        // Enum labels in declaration order, plus booleans.
        let sql = format!(
            "SELECT a.attname, NOT a.attnotnull, e.enumlabel, t.typname \
             FROM pg_attribute a JOIN pg_type t ON t.oid = a.atttypid \
             LEFT JOIN pg_enum e ON e.enumtypid = t.oid \
             WHERE a.attrelid = ({})::regclass AND a.attnum > 0 AND NOT a.attisdropped \
               AND (t.typtype = 'e' OR t.typname = 'bool') \
             ORDER BY a.attnum, e.enumsortorder",
            quote_literal(&Self::qualified(obj))
        );
        let mut choices = Choices::new();
        for row in self.simple_one(&sql).await?.rows {
            let (Some(col), Some(nullable)) = (row[0].clone(), row[1].clone()) else { continue };
            let entry = choices
                .entry(col)
                .or_insert_with(|| ColumnChoices { values: Vec::new(), nullable: nullable == "t" });
            match (row[2].clone(), row[3].as_deref()) {
                (Some(label), _) => entry.values.push(label),
                (None, Some("bool")) => entry.values = vec!["true".into(), "false".into()],
                _ => {}
            }
        }
        Ok(choices)
    }

    async fn cancel(&self) -> Result<()> {
        match &self.tls {
            Some(tls) => self.cancel_token.cancel_query(tls.clone()).await?,
            None => self.cancel_token.cancel_query(NoTls).await?,
        }
        Ok(())
    }
}
