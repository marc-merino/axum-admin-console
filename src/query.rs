//! SQL for the admin pages. Identifiers come only from the registry; every
//! value from the request is a bound parameter. Rows travel as text (one
//! `Option<String>` per column), formatted per [`Kind`] when shown; keys
//! travel as text and are cast to their column type.

use chrono_tz::Tz;
use sqlx::{PgConnection, Row, postgres::PgRow};

use crate::{
    Console,
    key::{Key, KeyKind},
    registry::{Field, Kind, Model, Path},
};

/// The most options a select, filter or many-to-many list loads.
pub const OPTION_CAP: i64 = 200;
/// Foreign keys to tables with more rows than this are edited as a key.
pub const SELECT_THRESHOLD: i64 = 100;

/// Field values of one row as text, by field name.
#[derive(Clone, Debug, Default)]
pub struct Values(pub Vec<(&'static str, Option<String>)>);

impl Values {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| *key == name)
            .and_then(|(_, value)| value.as_deref())
    }
    pub fn set(&mut self, name: &'static str, value: Option<String>) {
        match self.0.iter_mut().find(|(key, _)| *key == name) {
            Some(slot) => slot.1 = value,
            None => self.0.push((name, value)),
        }
    }
}

/// Positional parameters, all bound as text and cast in SQL.
#[derive(Default)]
pub struct Params(pub Vec<Option<String>>);

impl Params {
    /// Adds a value and returns its placeholder (`$3`).
    pub fn push(&mut self, value: impl Into<Option<String>>) -> String {
        self.0.push(value.into());
        format!("${}", self.0.len())
    }
}

pub fn bind_all<'q>(
    sql: &'q str,
    params: &'q Params,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    params.0.iter().fold(sqlx::query(sql), |query, value| {
        query.bind(value.as_deref())
    })
}

/// Replaces the `{t}` placeholder of a registry fragment with `alias`.
pub fn on(fragment: &str, alias: &str) -> String {
    fragment.replace("{t}", alias)
}

/// A value reached through `path` from the row aliased `alias`.
pub fn path_value(console: &Console, path: &Path, alias: &str) -> String {
    let Some(((first_fk, first_table), rest)) = path.hops.split_first() else {
        return format!("{alias}.{}", path.column);
    };
    let mut from = format!("{first_table} _p1");
    for (index, (fk, table)) in rest.iter().enumerate() {
        let (prev, this) = (index + 1, index + 2);
        let key = console.table_key(table).column;
        from.push_str(&format!(
            " JOIN {table} _p{this} ON _p{this}.{key} = _p{prev}.{fk}"
        ));
    }
    format!(
        "(SELECT _p{last}.{column} FROM {from} WHERE _p1.{key} = {alias}.{first_fk} LIMIT 1)",
        last = path.hops.len(),
        column = path.column,
        key = console.table_key(first_table).column,
    )
}

/// A condition on the value at `path`: `condition` with `{v}` standing for
/// it. Through foreign keys it becomes `fk IN (SELECT key ...)`, which uses
/// the indexes instead of a subquery per row.
pub fn path_condition(console: &Console, path: &Path, alias: &str, condition: &str) -> String {
    fn nested(
        console: &Console,
        hops: &[(&str, &str)],
        column: &str,
        level: usize,
        condition: &str,
    ) -> String {
        match hops.split_first() {
            None => condition.replace("{v}", &format!("_c{}.{column}", level - 1)),
            Some(((fk, table), rest)) => {
                let inner = nested(console, rest, column, level + 1, condition);
                format!(
                    "_c{prev}.{fk} IN (SELECT _c{level}.{key} FROM {table} _c{level} WHERE {inner})",
                    prev = level - 1,
                    key = console.table_key(table).column,
                )
            }
        }
    }
    let Some(((fk, table), rest)) = path.hops.split_first() else {
        return condition.replace("{v}", &format!("{alias}.{}", path.column));
    };
    let inner = nested(console, rest, path.column, 2, condition);
    format!(
        "{alias}.{fk} IN (SELECT _c1.{key} FROM {table} _c1 WHERE {inner})",
        key = console.table_key(table).column
    )
}

/// `expr` (of type `kind`) as text the formatter understands, datetimes in
/// time zone `tz`.
pub fn as_text(expr: &str, kind: Kind, tz: Tz) -> String {
    match kind {
        Kind::DateTime => {
            format!(
                "to_char(({expr}) AT TIME ZONE '{}', 'YYYY-MM-DD\"T\"HH24:MI:SS')",
                tz.name()
            )
        }
        Kind::Duration => format!("floor(extract(epoch FROM ({expr})))::bigint::text"),
        _ => format!("({expr})::text"),
    }
}

/// The SQL for a field's stored value as text.
pub fn field_text(console: &Console, field: &Field, alias: &str) -> String {
    as_text(
        &format!("{alias}.{}", field.column),
        field.kind,
        console.time_zone(),
    )
}

/// A placeholder cast to the column type of `kind`.
pub fn cast_param(console: &Console, kind: Kind, placeholder: &str) -> String {
    match kind {
        Kind::Int => format!("{placeholder}::bigint"),
        Kind::ForeignKey(table) => console.table_key(table).kind.cast(placeholder),
        Kind::Decimal(_) => format!("{placeholder}::numeric"),
        Kind::Float => format!("{placeholder}::float8"),
        Kind::Bool => format!("{placeholder}::boolean"),
        Kind::DateTime => format!(
            "({placeholder}::timestamp AT TIME ZONE '{}')",
            console.time_zone().name()
        ),
        Kind::Date => format!("{placeholder}::date"),
        // Whole seconds, stored as Python's timedelta: days, then 0..86400 seconds.
        Kind::Duration => format!(
            "make_interval(days => floor({placeholder}::numeric / 86400)::int, \
             secs => ({placeholder}::bigint - floor({placeholder}::numeric / 86400)::bigint * 86400)::float8)"
        ),
        Kind::Json => format!("{placeholder}::jsonb"),
        Kind::Char | Kind::Text | Kind::Choice(_) | Kind::Password => {
            format!("{placeholder}::text")
        }
    }
}

/// The text of the row `key_expr` in `table`: the registered model's
/// `__str__`, or `"#<key>"`.
pub fn repr_of(console: &Console, table: &str, key_expr: &str) -> String {
    match console.model_for_table(table) {
        Some(model) if !model.repr.is_empty() => format!(
            "(SELECT {} FROM {table} _r WHERE _r.{} = {key_expr})",
            on(model.repr, "_r"),
            model.pk
        ),
        _ => format!("CASE WHEN {key_expr} IS NULL THEN NULL ELSE '#' || {key_expr} END"),
    }
}

/// The text of a row of `model` aliased `alias`.
pub fn repr(model: &Model, alias: &str) -> String {
    if model.repr.is_empty() {
        format!(
            "'{} object (' || {alias}.{} || ')'",
            model.verbose, model.pk
        )
    } else {
        format!("({})", on(model.repr, alias))
    }
}

pub fn text_at(row: &PgRow, index: usize) -> Option<String> {
    row.try_get::<Option<String>, _>(index).ok().flatten()
}

/// The key in column `index` of `row`, selected as text.
pub(crate) fn key_at(row: &PgRow, index: usize, kind: KeyKind) -> Option<Key> {
    text_at(row, index).and_then(|text| kind.parse(&text))
}

/// One row's fields and text, or `None` when it does not exist.
pub(crate) async fn load(
    console: &Console,
    db: &mut PgConnection,
    model: &Model,
    key: &Key,
) -> sqlx::Result<Option<(Values, String)>> {
    let mut columns: Vec<String> = model
        .fields
        .iter()
        .map(|f| field_text(console, f, "t"))
        .collect();
    columns.push(repr(model, "t"));
    let sql = format!(
        "SELECT {} FROM {} t WHERE t.{} = {}",
        columns.join(", "),
        model.table,
        model.pk,
        model.key.cast("$1")
    );
    let row = sqlx::query(&sql)
        .bind(key.to_string())
        .fetch_optional(&mut *db)
        .await?;
    Ok(row.map(|row| {
        let values = Values(
            model
                .fields
                .iter()
                .enumerate()
                .map(|(index, field)| (field.name, text_at(&row, index)))
                .collect(),
        );
        let text = text_at(&row, model.fields.len()).unwrap_or_default();
        (values, text)
    }))
}

/// `(key, text)` of each row given that exists, by key, for the delete
/// pages and the history.
pub(crate) async fn reprs(
    db: &mut PgConnection,
    model: &Model,
    keys: &[Key],
) -> sqlx::Result<Vec<(Key, String)>> {
    let sql = format!(
        "SELECT t.{pk}::text, ({repr})::text FROM {table} t WHERE t.{pk} = ANY({keys}) ORDER BY t.{pk}",
        pk = model.pk,
        repr = repr(model, "t"),
        table = model.table,
        keys = model.key.cast_array("$1"),
    );
    let rows = sqlx::query(&sql)
        .bind(Key::texts(keys))
        .fetch_all(&mut *db)
        .await?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            Some((
                key_at(row, 0, model.key)?,
                text_at(row, 1).unwrap_or_default(),
            ))
        })
        .collect())
}

/// `(key, text)` choices of a table, at most [`OPTION_CAP`], by text.
pub(crate) async fn options(
    console: &Console,
    db: &mut PgConnection,
    table: &str,
) -> sqlx::Result<Vec<(String, String)>> {
    let key = console.table_key(table).column;
    let text = match console.model_for_table(table) {
        Some(model) => repr(model, "t"),
        None => format!("'#' || t.{key}"),
    };
    let sql = format!(
        "SELECT t.{key}::text, ({text})::text AS label FROM {table} t ORDER BY 2, 1 LIMIT {OPTION_CAP}"
    );
    let rows = sqlx::query(&sql).fetch_all(&mut *db).await?;
    Ok(rows
        .iter()
        .map(|row| {
            (
                text_at(row, 0).unwrap_or_default(),
                text_at(row, 1).unwrap_or_default(),
            )
        })
        .collect())
}

/// Whether `table` has at most `limit` rows, without counting them all.
pub(crate) async fn at_most(db: &mut PgConnection, table: &str, limit: i64) -> sqlx::Result<bool> {
    let sql = format!(
        "SELECT count(*) FROM (SELECT 1 FROM {table} LIMIT {}) s",
        limit + 1
    );
    let count: i64 = sqlx::query_scalar(&sql).fetch_one(&mut *db).await?;
    Ok(count <= limit)
}

/// Whether `table` has a row with this key.
pub(crate) async fn exists(
    console: &Console,
    db: &mut PgConnection,
    table: &str,
    key: &Key,
) -> sqlx::Result<bool> {
    let table_key = console.table_key(table);
    let sql = format!(
        "SELECT EXISTS (SELECT 1 FROM {table} WHERE {} = {})",
        table_key.column,
        table_key.kind.cast("$1")
    );
    sqlx::query_scalar(&sql)
        .bind(key.to_string())
        .fetch_one(&mut *db)
        .await
}
