//! Deleting like Django's collector: the rows that cascade (through every
//! foreign key in the host's delete rules and its extra deletions) are
//! deleted, `SET_NULL` references are cleared, and a `PROTECT` reference
//! blocks the delete. Row sets are SQL conditions built
//! from the static graph, so nothing is loaded into memory but counts and a
//! few rows to show.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write;

use sqlx::PgConnection;

use crate::{
    Schema,
    auth::Perms,
    html::esc,
    key::Key,
    query,
    registry::Model,
    relations::{OnDelete, Relation},
};

/// How many objects of each kind the confirmation page lists.
const SHOWN_PER_TABLE: i64 = 10;
/// A cascade over more tables than this is refused.
const MAX_TABLES: usize = 80;

struct Node {
    table: &'static str,
    /// The table's key column, which the foreign keys to it refer to.
    key: &'static str,
    /// SQL condition selecting this table's rows (columns unqualified).
    condition: String,
    /// Whether another node's condition reads this node's ids (`nK` CTE).
    parent: bool,
}

/// What deleting rows of one model takes: the rows that go with them, the
/// references cleared and the references that block the delete. Actions
/// that delete rows the way the console does use it too.
pub struct Plan {
    schema: &'static Schema,
    root: &'static Model,
    nodes: Vec<Node>,
    /// `(table, column, node index)`: references cleared before the delete.
    set_null: Vec<(&'static str, &'static str, usize)>,
    /// `(table, column, node index)`: references that block the delete.
    protect: Vec<(&'static str, &'static str, usize)>,
}

fn incoming<'a>(
    schema: &'static Schema,
    table: &'a str,
) -> impl Iterator<Item = &'static Relation> + 'a {
    schema
        .relations
        .iter()
        .filter(move |(_, _, to, _)| *to == table)
}

impl Plan {
    /// The plan for deleting the rows of `model` whose primary key is in
    /// `$1` (a `text[]` of keys).
    pub fn new(schema: &'static Schema, model: &'static Model) -> anyhow::Result<Self> {
        // Tables reached through cascades, with their parents.
        let mut parents: HashMap<&'static str, Vec<(&'static str, String)>> = HashMap::new();
        let mut reached: Vec<&'static str> = vec![model.table];
        let mut queue = vec![model.table];
        while let Some(table) = queue.pop() {
            let cascades = incoming(schema, table)
                .filter(|(_, _, _, rule)| *rule == OnDelete::Cascade)
                .map(|(child, column, _, _)| (*child, format!("{column} IN ({{parent}})")));
            let signals = schema
                .also_delete
                .iter()
                .filter(|(from, _, _)| *from == table)
                .map(|(_, child, condition)| (*child, (*condition).to_string()));
            for (child, condition) in cascades.chain(signals).collect::<Vec<_>>() {
                parents.entry(child).or_default().push((table, condition));
                if !reached.contains(&child) {
                    reached.push(child);
                    queue.push(child);
                }
            }
            anyhow::ensure!(reached.len() <= MAX_TABLES, "delete cascade too large");
        }

        // Topological order: a table after every table it cascades from.
        let mut order: Vec<&'static str> = Vec::new();
        let mut placed: BTreeSet<&'static str> = BTreeSet::new();
        while order.len() < reached.len() {
            let next = reached.iter().copied().find(|table| {
                !placed.contains(table)
                    && (*table == model.table
                        || parents
                            .get(table)
                            .is_some_and(|list| list.iter().all(|(p, _)| placed.contains(p))))
            });
            let Some(next) = next else {
                anyhow::bail!("delete cascade from {} has a cycle", model.table);
            };
            placed.insert(next);
            order.push(next);
        }

        let index: HashMap<&str, usize> = order.iter().enumerate().map(|(i, t)| (*t, i)).collect();
        let mut nodes: Vec<Node> = order
            .iter()
            .map(|table| Node {
                table,
                key: schema.table_key(table).column,
                condition: String::new(),
                parent: false,
            })
            .collect();
        nodes[0].condition = format!("{} = ANY({})", model.pk, model.key.cast_array("$1"));
        for (position, table) in order.iter().enumerate().skip(1) {
            let mut terms = Vec::new();
            for (parent, condition) in &parents[table] {
                let parent_index = index[parent];
                nodes[parent_index].parent = true;
                let parent_key = nodes[parent_index].key;
                terms.push(condition.replace(
                    "{parent}",
                    &format!("SELECT {parent_key} FROM n{parent_index}"),
                ));
            }
            nodes[position].condition = terms.join(" OR ");
        }

        let mut set_null = Vec::new();
        let mut protect = Vec::new();
        for (position, node) in nodes.iter_mut().enumerate() {
            for (table, column, _, rule) in incoming(schema, node.table) {
                match rule {
                    OnDelete::Cascade => {}
                    OnDelete::SetNull => set_null.push((*table, *column, position)),
                    OnDelete::Protect => protect.push((*table, *column, position)),
                }
                if *rule != OnDelete::Cascade {
                    node.parent = true;
                }
            }
        }
        Ok(Self {
            schema,
            root: model,
            nodes,
            set_null,
            protect,
        })
    }

    /// `WITH n0 AS (...), ...` for every node other conditions read.
    fn prefix(&self) -> String {
        let ctes: Vec<String> = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.parent)
            .map(|(i, node)| {
                format!(
                    "n{i} AS MATERIALIZED (SELECT {} FROM {} WHERE {})",
                    node.key, node.table, node.condition
                )
            })
            .collect();
        if ctes.is_empty() {
            String::new()
        } else {
            format!("WITH {} ", ctes.join(", "))
        }
    }

    /// Node `i`'s rows aliased `alias`.
    fn rows_as(&self, i: usize, alias: &str) -> String {
        let node = &self.nodes[i];
        format!("FROM {} {alias} WHERE {}", node.table, node.condition)
    }

    /// The rows of node `i`, as `FROM table WHERE condition`.
    fn rows(&self, i: usize) -> String {
        let node = &self.nodes[i];
        format!("FROM {} WHERE {}", node.table, node.condition)
    }

    /// What deleting the rows with `keys` would delete and what blocks it,
    /// for the confirmation page.
    pub async fn summary(
        &self,
        db: &mut PgConnection,
        keys: &[Key],
        perms: &Perms,
    ) -> anyhow::Result<Summary> {
        let keys = Key::texts(keys);
        let prefix = self.prefix();
        let mut summary = Summary::default();
        for (i, node) in self.nodes.iter().enumerate() {
            let count: i64 =
                sqlx::query_scalar(&format!("{prefix}SELECT count(*) {}", self.rows(i)))
                    .bind(&keys)
                    .fetch_one(&mut *db)
                    .await?;
            if count == 0 {
                continue;
            }
            let model = if i == 0 {
                Some(self.root)
            } else {
                self.schema.models().find(|m| m.table == node.table)
            };
            let label = model.map_or_else(|| node.table.to_string(), |m| m.title_plural());
            let mut shown = Vec::new();
            if let Some(model) = model {
                if i > 0 && !perms.delete(model) {
                    summary.perms_needed.push(model.verbose.to_string());
                }
                let sql = format!(
                    "{prefix}SELECT t.{pk}::text, ({repr})::text {rows} ORDER BY t.{pk} LIMIT {SHOWN_PER_TABLE}",
                    pk = model.pk,
                    repr = query::repr(model, "t"),
                    rows = self.rows_as(i, "t"),
                );
                let rows: Vec<(Option<String>, Option<String>)> =
                    sqlx::query_as(&sql).bind(&keys).fetch_all(&mut *db).await?;
                shown = rows
                    .into_iter()
                    .filter_map(|(key, text)| {
                        let key = model.key.parse(&key?)?;
                        Some((model.object_url(key), text.unwrap_or_default()))
                    })
                    .collect();
            }
            summary.tables.push((label, count, shown));
        }
        for (table, column, i) in &self.protect {
            let sql = format!(
                "{prefix}SELECT count(*) FROM {table} WHERE {column} IN (SELECT {} {})",
                self.nodes[*i].key,
                self.rows(*i)
            );
            let count: i64 = sqlx::query_scalar(&sql)
                .bind(&keys)
                .fetch_one(&mut *db)
                .await?;
            if count > 0 {
                summary
                    .protected
                    .push(format!("{count} {table} rows ({column})"));
            }
        }
        Ok(summary)
    }

    /// Clears `SET_NULL` references, then deletes leaves before the rows
    /// they cascade from. Run it inside a transaction.
    pub async fn execute(&self, db: &mut PgConnection, keys: &[Key]) -> sqlx::Result<()> {
        let keys = Key::texts(keys);
        let prefix = self.prefix();
        for (table, column, i) in &self.set_null {
            let sql = format!(
                "{prefix}UPDATE {table} SET {column} = NULL WHERE {column} IN (SELECT {} {})",
                self.nodes[*i].key,
                self.rows(*i)
            );
            sqlx::query(&sql).bind(&keys).execute(&mut *db).await?;
        }
        for i in (0..self.nodes.len()).rev() {
            let sql = format!("{prefix}DELETE {}", self.rows(i));
            sqlx::query(&sql).bind(&keys).execute(&mut *db).await?;
        }
        Ok(())
    }
}

/// `(label, row count, [(url, text)] of the first rows)`.
pub type TableSummary = (String, i64, Vec<(String, String)>);

#[derive(Default)]
pub struct Summary {
    /// `(label, count, [(url, text)])`, the deleted rows first.
    pub tables: Vec<TableSummary>,
    /// Registered models the user may not delete.
    pub perms_needed: Vec<String>,
    pub protected: Vec<String>,
}

impl Summary {
    pub fn blocked(&self) -> bool {
        !self.perms_needed.is_empty() || !self.protected.is_empty()
    }

    pub fn html(&self) -> String {
        let mut out = String::from("<h2>Summary</h2><ul class=\"delete-summary\">");
        for (label, count, _) in &self.tables {
            let _ = write!(out, "<li>{}: {count}</li>", esc(label));
        }
        out.push_str("</ul><h2>Objects</h2><ul class=\"deleted-objects\">");
        for (label, count, shown) in &self.tables {
            let _ = write!(out, "<li>{}<ul>", esc(label));
            for (url, text) in shown {
                let _ = write!(out, "<li><a href=\"{}\">{}</a></li>", esc(url), esc(text));
            }
            let more = count - shown.len() as i64;
            if more > 0 {
                let _ = write!(out, "<li>… and {more} more</li>");
            }
            out.push_str("</ul></li>");
        }
        out.push_str("</ul>");
        out
    }
}
