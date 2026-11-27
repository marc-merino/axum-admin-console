//! The change list: Django's result table with search, filters, sortable
//! columns, paging, the date hierarchy and actions.

use std::fmt::Write;

use axum::{
    extract::{Path as UrlPath, RawQuery, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use chrono::{Datelike, Duration, NaiveDate, Utc};
use chrono_tz::Tz;

use crate::{
    Console,
    delete::Plan,
    history::{Change, Event},
    html::{self, esc},
    key::Key,
    pages::{crumbs, find_model},
    query::{
        self, OPTION_CAP, Params, as_text, bind_all, field_text, key_at, on, path_condition,
        path_value, repr_of, text_at,
    },
    registry::{Column, ColumnSource, Filter, Kind, Model, Refusal},
    session::{AdminError, AdminResult, Form, Staff},
};

/// Query-string parameters of the page.
struct Query(Vec<(String, String)>);

impl Query {
    fn parse(raw: Option<&str>) -> Self {
        Self(serde_urlencoded::from_str(raw.unwrap_or("")).unwrap_or_default())
    }
    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    }
    /// The query string with `remove` dropped (and the page reset), then
    /// `add` appended.
    fn with(&self, remove: &[&str], add: &[(&str, String)]) -> String {
        let mut pairs: Vec<(String, String)> = self
            .0
            .iter()
            .filter(|(k, _)| k != "p" && !remove.contains(&k.as_str()))
            .cloned()
            .collect();
        pairs.extend(add.iter().map(|(k, v)| ((*k).to_string(), v.clone())));
        if pairs.is_empty() {
            "?".to_string()
        } else {
            format!(
                "?{}",
                serde_urlencoded::to_string(&pairs).unwrap_or_default()
            )
        }
    }
}

/// One rendered filter: title and `(label, link, selected)` choices.
struct FilterBox {
    title: String,
    choices: Vec<(String, String, bool)>,
}

/// Builds the WHERE conditions and the sidebar from `list_filter`.
struct Filters<'a> {
    query: &'a Query,
    params: &'a mut Params,
    conditions: Vec<String>,
    boxes: Vec<FilterBox>,
}

impl Filters<'_> {
    /// Adds one filter whose parameters are `keys`; `options` are
    /// `(label, [(key, value)])`; `condition` turns the selected
    /// `(key, value)` into SQL (or `None` for a value it rejects).
    fn add(
        &mut self,
        title: &str,
        keys: &[&str],
        options: Vec<(String, Vec<(String, String)>)>,
        condition: impl Fn(&str, &str, &mut Params) -> Option<String>,
    ) {
        let active: Vec<(String, String)> = keys
            .iter()
            .filter_map(|key| {
                self.query
                    .get(key)
                    .map(|v| ((*key).to_string(), v.to_string()))
            })
            .collect();
        for (key, value) in &active {
            if let Some(sql) = condition(key, value, self.params) {
                self.conditions.push(sql);
            }
        }
        let mut choices = vec![(
            "All".to_string(),
            self.query.with(keys, &[]),
            active.is_empty(),
        )];
        for (label, set) in options {
            let add: Vec<(&str, String)> =
                set.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
            let selected = !active.is_empty()
                && active.len() == set.len()
                && set.iter().all(|pair| active.contains(pair));
            choices.push((label, self.query.with(keys, &add), selected));
        }
        self.boxes.push(FilterBox {
            title: title.to_string(),
            choices,
        });
    }
}

fn pair(key: &str, value: &str) -> Vec<(String, String)> {
    vec![(key.to_string(), value.to_string())]
}

fn today(tz: Tz) -> NaiveDate {
    Utc::now().with_timezone(&tz).date_naive()
}

/// Django's `DateFieldListFilter` choices.
fn date_options(
    tz: Tz,
    key_gte: &str,
    key_lt: &str,
    key_null: &str,
    nullable: bool,
) -> Vec<(String, Vec<(String, String)>)> {
    let today = today(tz);
    let tomorrow = today + Duration::days(1);
    let month = today.with_day(1).unwrap_or(today);
    let next_month = if month.month() == 12 {
        NaiveDate::from_ymd_opt(month.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(month.year(), month.month() + 1, 1)
    }
    .unwrap_or(tomorrow);
    let year = NaiveDate::from_ymd_opt(today.year(), 1, 1).unwrap_or(today);
    let next_year = NaiveDate::from_ymd_opt(today.year() + 1, 1, 1).unwrap_or(tomorrow);
    let range = |from: NaiveDate, to: NaiveDate| {
        vec![
            (key_gte.to_string(), from.to_string()),
            (key_lt.to_string(), to.to_string()),
        ]
    };
    let mut options = vec![
        ("Today".to_string(), range(today, tomorrow)),
        (
            "Past 7 days".to_string(),
            range(today - Duration::days(7), tomorrow),
        ),
        ("This month".to_string(), range(month, next_month)),
        ("This year".to_string(), range(year, next_year)),
    ];
    if nullable {
        options.push(("No date".to_string(), pair(key_null, "True")));
        options.push(("Has date".to_string(), pair(key_null, "False")));
    }
    options
}

fn date_bound(
    tz: Tz,
    column: &str,
    kind: Kind,
    op: &str,
    value: &str,
    params: &mut Params,
) -> Option<String> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
    let p = params.push(value.to_string());
    Some(match kind {
        Kind::Date => format!("{column} {op} {p}::date"),
        _ => format!(
            "{column} {op} ({p}::date::timestamp AT TIME ZONE '{}')",
            tz.name()
        ),
    })
}

fn null_condition(column: &str, value: &str) -> Option<String> {
    match value {
        "True" | "true" | "1" => Some(format!("{column} IS NULL")),
        "False" | "false" | "0" => Some(format!("{column} IS NOT NULL")),
        _ => None,
    }
}

async fn distinct_values(
    db: &mut sqlx::PgConnection,
    model: &Model,
    expr: &str,
) -> sqlx::Result<Vec<String>> {
    let sql = format!(
        "SELECT DISTINCT ({expr})::text AS v FROM {} t WHERE ({expr}) IS NOT NULL ORDER BY 1 LIMIT {OPTION_CAP}",
        model.table
    );
    let rows: Vec<(Option<String>,)> = sqlx::query_as(&sql).fetch_all(db).await?;
    Ok(rows.into_iter().filter_map(|(v,)| v).collect())
}

async fn build_filters(
    console: &Console,
    db: &mut sqlx::PgConnection,
    model: &Model,
    filters: &mut Filters<'_>,
) -> AdminResult<()> {
    let tz = console.time_zone();
    for filter in model.list_filter {
        match *filter {
            Filter::Field(name) => {
                let field = model
                    .field(name)
                    .ok_or_else(|| anyhow::anyhow!("unknown filter field {name}"))?;
                let column = format!("t.{}", field.column);
                let exact = format!("{name}__exact");
                let isnull = format!("{name}__isnull");
                let title = format!("By {}", field.label.to_lowercase());
                let null_option = |options: &mut Vec<(String, Vec<(String, String)>)>| {
                    if field.null {
                        options.push(("Unknown".to_string(), pair(&isnull, "True")));
                    }
                };
                match field.kind {
                    Kind::Bool => {
                        let mut options = vec![
                            ("Yes".to_string(), pair(&exact, "1")),
                            ("No".to_string(), pair(&exact, "0")),
                        ];
                        null_option(&mut options);
                        filters.add(&title, &[&exact, &isnull], options, |key, value, _| {
                            if key.ends_with("__isnull") {
                                return null_condition(&column, value);
                            }
                            match value {
                                "1" | "True" | "true" => Some(format!("{column} = true")),
                                "0" | "False" | "false" => Some(format!("{column} = false")),
                                _ => None,
                            }
                        });
                    }
                    Kind::Choice(choices) => {
                        let mut options: Vec<_> = choices
                            .iter()
                            .map(|(value, label)| ((*label).to_string(), pair(&exact, value)))
                            .collect();
                        null_option(&mut options);
                        filters.add(&title, &[&exact, &isnull], options, |key, value, params| {
                            if key.ends_with("__isnull") {
                                return null_condition(&column, value);
                            }
                            Some(format!("{column} = {}", params.push(value.to_string())))
                        });
                    }
                    Kind::ForeignKey(table) => {
                        let key = format!("{name}__id__exact");
                        let kind = console.table_key(table).kind;
                        let mut options: Vec<_> = query::options(console, db, table)
                            .await?
                            .into_iter()
                            .map(|(id, label)| (label, pair(&key, &id)))
                            .collect();
                        null_option(&mut options);
                        filters.add(&title, &[&key, &isnull], options, |key, value, params| {
                            if key.ends_with("__isnull") {
                                return null_condition(&column, value);
                            }
                            kind.parse(value)?;
                            Some(format!(
                                "{column} = {}",
                                kind.cast(&params.push(value.to_string()))
                            ))
                        });
                    }
                    Kind::DateTime | Kind::Date => {
                        let gte = format!("{name}__gte");
                        let lt = format!("{name}__lt");
                        let options = date_options(tz, &gte, &lt, &isnull, field.null);
                        let kind = field.kind;
                        filters.add(
                            &title,
                            &[&gte, &lt, &isnull],
                            options,
                            |key, value, params| {
                                if key.ends_with("__isnull") {
                                    null_condition(&column, value)
                                } else if key.ends_with("__gte") {
                                    date_bound(tz, &column, kind, ">=", value, params)
                                } else {
                                    date_bound(tz, &column, kind, "<", value, params)
                                }
                            },
                        );
                    }
                    _ => {
                        let mut options: Vec<_> = distinct_values(db, model, &column)
                            .await?
                            .into_iter()
                            .map(|v| (v.clone(), pair(&exact, &v)))
                            .collect();
                        null_option(&mut options);
                        filters.add(&title, &[&exact, &isnull], options, |key, value, params| {
                            if key.ends_with("__isnull") {
                                return null_condition(&column, value);
                            }
                            Some(format!(
                                "{column}::text = {}",
                                params.push(value.to_string())
                            ))
                        });
                    }
                }
            }
            Filter::Related {
                param,
                label,
                path,
                target,
            } => {
                let key = format!("{param}__id__exact");
                let kind = console.table_key(target).kind;
                let options: Vec<_> = query::options(console, db, target)
                    .await?
                    .into_iter()
                    .map(|(id, text)| (text, pair(&key, &id)))
                    .collect();
                filters.add(
                    &format!("By {}", label.to_lowercase()),
                    &[&key],
                    options,
                    |_, value, params| {
                        kind.parse(value)?;
                        let p = kind.cast(&params.push(value.to_string()));
                        Some(path_condition(console, &path, "t", &format!("{{v}} = {p}")))
                    },
                );
            }
            Filter::Values { param, label, path } => {
                let key = format!("{param}__exact");
                let options: Vec<_> = distinct_values(db, model, &path_value(console, &path, "t"))
                    .await?
                    .into_iter()
                    .map(|v| (v.clone(), pair(&key, &v)))
                    .collect();
                filters.add(
                    &format!("By {}", label.to_lowercase()),
                    &[&key],
                    options,
                    |_, value, params| {
                        let p = params.push(value.to_string());
                        Some(path_condition(
                            console,
                            &path,
                            "t",
                            &format!("{{v}}::text = {p}"),
                        ))
                    },
                );
            }
            Filter::Options {
                param,
                label,
                path,
                options_sql,
            } => {
                let key = format!("{param}__exact");
                let sql = format!(
                    "SELECT o.v::text, o.l::text FROM ({options_sql}) o(v, l) \
                     WHERE o.v IS NOT NULL LIMIT {OPTION_CAP}"
                );
                let rows: Vec<(String, Option<String>)> =
                    sqlx::query_as(&sql).fetch_all(&mut *db).await?;
                let options: Vec<_> = rows
                    .into_iter()
                    .map(|(value, text)| {
                        (text.unwrap_or_else(|| value.clone()), pair(&key, &value))
                    })
                    .collect();
                filters.add(
                    &format!("By {}", label.to_lowercase()),
                    &[&key],
                    options,
                    |_, value, params| {
                        let p = params.push(value.to_string());
                        Some(path_condition(
                            console,
                            &path,
                            "t",
                            &format!("{{v}}::text = {p}"),
                        ))
                    },
                );
            }
        }
    }
    Ok(())
}

/// The conditions for `q`: every word must match one search field.
fn search_conditions(
    console: &Console,
    model: &Model,
    q: &str,
    params: &mut Params,
) -> Vec<String> {
    if model.search.is_empty() {
        return Vec::new();
    }
    q.split_whitespace()
        .take(10)
        .map(|word| {
            let word: String = word.chars().take(100).collect();
            let escaped = word
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            let p = params.push(format!("%{escaped}%"));
            let alternatives: Vec<String> = model
                .search
                .iter()
                .map(|path| path_condition(console, path, "t", &format!("({{v}})::text ILIKE {p}")))
                .collect();
            format!("({})", alternatives.join(" OR "))
        })
        .collect()
}

/// A result column: header, SQL for its text, kind, sort expression.
struct Col {
    header: String,
    sql: String,
    kind: Kind,
    order: Option<String>,
}

fn columns(console: &Console, model: &Model) -> AdminResult<Vec<Col>> {
    if model.list_display.is_empty() {
        return Ok(vec![Col {
            header: model.verbose.to_uppercase(),
            sql: format!("({})::text", query::repr(model, "t")),
            kind: Kind::Char,
            order: None,
        }]);
    }
    model
        .list_display
        .iter()
        .map(|column| {
            let header = column
                .label_fn
                .map_or_else(|| column.label.to_string(), |f| f());
            Ok(match column.source {
                ColumnSource::Field(name) => {
                    let field = model
                        .field(name)
                        .ok_or_else(|| anyhow::anyhow!("unknown list_display field {name}"))?;
                    let header = if header.is_empty() {
                        field.label.to_string()
                    } else {
                        header
                    };
                    let own = format!("t.{}", field.column);
                    let (sql, kind) = match field.kind {
                        Kind::ForeignKey(table) => (
                            format!("({})::text", repr_of(console, table, &own)),
                            Kind::Char,
                        ),
                        kind => (field_text(console, field, "t"), kind),
                    };
                    let order = match field.kind {
                        Kind::Password => None,
                        _ => Some(declared_order(console, column).unwrap_or(own)),
                    };
                    Col {
                        header,
                        sql,
                        kind,
                        order,
                    }
                }
                ColumnSource::Expr(sql, kind) => Col {
                    header,
                    sql: as_text(&on(sql, "t"), kind, console.time_zone()),
                    kind,
                    order: declared_order(console, column),
                },
                ColumnSource::Path(path) => Col {
                    header,
                    sql: format!("({})::text", path_value(console, &path, "t")),
                    kind: Kind::Char,
                    order: declared_order(console, column),
                },
            })
        })
        .collect()
}

/// A column's `admin_order_field`, as SQL on `t`.
fn declared_order(console: &Console, column: &Column) -> Option<String> {
    column
        .order_sql
        .map(|sql| on(sql, "t"))
        .or_else(|| column.order.map(|path| path_value(console, &path, "t")))
}

/// `o=1.-3`: 1-based column numbers, `-` for descending.
fn parse_order(raw: Option<&str>, count: usize) -> Vec<(usize, bool)> {
    raw.unwrap_or("")
        .split('.')
        .filter_map(|part| {
            let (desc, number) = match part.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, part),
            };
            let index = number.parse::<usize>().ok()?.checked_sub(1)?;
            (index < count).then_some((index, desc))
        })
        .take(5)
        .collect()
}

/// The date hierarchy: its conditions, the local date expression and the
/// `(part, value)` pairs chosen so far.
type DateNav = (Vec<String>, String, Vec<(&'static str, String)>);

/// The date hierarchy's conditions and its links.
fn hierarchy(
    console: &Console,
    model: &Model,
    query: &Query,
    params: &mut Params,
) -> Option<DateNav> {
    let field = model.field(model.date_hierarchy?)?;
    let local = match field.kind {
        Kind::Date => format!("t.{}", field.column),
        _ => format!(
            "(t.{} AT TIME ZONE '{}')",
            field.column,
            console.time_zone().name()
        ),
    };
    let keys = [
        ("year", format!("{}__year", field.name)),
        ("month", format!("{}__month", field.name)),
        ("day", format!("{}__day", field.name)),
    ];
    let mut conditions = Vec::new();
    let mut chosen = Vec::new();
    for (part, key) in &keys {
        let Some(value) = query.get(key).and_then(|v| v.parse::<i32>().ok()) else {
            break;
        };
        let p = params.push(value.to_string());
        conditions.push(format!("extract({part} FROM {local}) = {p}::int"));
        chosen.push((*part, value.to_string()));
    }
    Some((conditions, local, chosen))
}

pub async fn page(
    State(console): State<Console>,
    staff: Staff,
    UrlPath((app, name)): UrlPath<(String, String)>,
    RawQuery(raw): RawQuery,
) -> AdminResult<Response> {
    let model = find_model(&console, &app, &name)?;
    if !staff.perms.view(model) {
        return Err(AdminError::Forbidden);
    }
    let query = Query::parse(raw.as_deref());
    let mut db = console.db().acquire().await?;
    let mut params = Params::default();

    let q = query.get("q").unwrap_or("").to_string();
    let mut conditions = search_conditions(&console, model, &q, &mut params);
    let mut filters = Filters {
        query: &query,
        params: &mut params,
        conditions: Vec::new(),
        boxes: Vec::new(),
    };
    build_filters(&console, &mut db, model, &mut filters).await?;
    let boxes = std::mem::take(&mut filters.boxes);
    conditions.append(&mut filters.conditions);
    let date_nav = hierarchy(&console, model, &query, &mut params);
    if let Some((date_conditions, ..)) = &date_nav {
        conditions.extend(date_conditions.iter().cloned());
    }
    let filtered = !conditions.is_empty();
    let where_sql = if filtered {
        format!("WHERE {}", conditions.join(" AND "))
    } else {
        String::new()
    };

    let cols = columns(&console, model)?;
    let order = parse_order(query.get("o"), cols.len());
    let mut order_sql: Vec<String> = order
        .iter()
        .filter_map(|(i, desc)| {
            cols[*i]
                .order
                .as_ref()
                .map(|expr| format!("{expr} {}", if *desc { "DESC" } else { "ASC" }))
        })
        .collect();
    order_sql.extend(model.ordering.iter().map(|sort| {
        format!(
            "{} {}",
            path_value(&console, &sort.path, "t"),
            if sort.desc { "DESC" } else { "ASC" }
        )
    }));
    order_sql.push(format!("t.{} DESC", model.pk));

    let count_sql = format!("SELECT count(*) FROM {} t {where_sql}", model.table);
    let count: i64 = sqlx::query_scalar(&count_sql)
        .bind_all_text(&params)
        .fetch_one(&mut *db)
        .await?;
    let total: i64 = if filtered {
        sqlx::query_scalar(&format!("SELECT count(*) FROM {}", model.table))
            .fetch_one(&mut *db)
            .await?
    } else {
        count
    };
    let per_page = model.per_page.max(1);
    let pages = ((count + per_page - 1) / per_page).max(1);
    let page_number = query
        .get("p")
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1)
        .clamp(1, pages);

    let select: Vec<String> = cols.iter().map(|c| c.sql.clone()).collect();
    let mut list_params = Params(params.0.clone());
    let limit = list_params.push(per_page.to_string());
    let offset = list_params.push(((page_number - 1) * per_page).to_string());
    let list_sql = format!(
        "SELECT t.{pk}::text, {} FROM {} t {where_sql} ORDER BY {} LIMIT {limit}::bigint OFFSET {offset}::bigint",
        select.join(", "),
        model.table,
        order_sql.join(", "),
        pk = model.pk,
    );
    let rows = bind_all(&list_sql, &list_params)
        .fetch_all(&mut *db)
        .await?;

    // Date hierarchy links.
    let mut date_html = String::new();
    if let Some((_, local, chosen)) = &date_nav {
        let field = model.field(model.date_hierarchy.unwrap_or_default());
        if let Some(field) = field {
            let parts = ["year", "month", "day"];
            if chosen.len() < 3 {
                let next = parts[chosen.len()];
                let sql = format!(
                    "SELECT DISTINCT extract({next} FROM {local})::int FROM {} t {where_sql} ORDER BY 1 LIMIT 60",
                    model.table
                );
                let values: Vec<(Option<i32>,)> = sqlx::query_as(&sql)
                    .bind_all_text(&params)
                    .fetch_all(&mut *db)
                    .await?;
                let keys: Vec<String> = parts
                    .iter()
                    .map(|p| format!("{}__{p}", field.name))
                    .collect();
                let all_keys: Vec<&str> = keys.iter().map(String::as_str).collect();
                date_html.push_str("<nav class=\"xfull\"><ul class=\"date-hierarchy\">");
                if !chosen.is_empty() {
                    let back: Vec<(&str, String)> = chosen[..chosen.len() - 1]
                        .iter()
                        .zip(keys.iter())
                        .map(|((_, v), k)| (k.as_str(), v.clone()))
                        .collect();
                    let _ = write!(
                        date_html,
                        "<li><a href=\"{}\">&lsaquo; All</a></li>",
                        esc(&query.with(&all_keys, &back))
                    );
                }
                for value in values.into_iter().filter_map(|(v,)| v) {
                    let mut set: Vec<(&str, String)> = chosen
                        .iter()
                        .zip(keys.iter())
                        .map(|((_, v), k)| (k.as_str(), v.clone()))
                        .collect();
                    set.push((keys[chosen.len()].as_str(), value.to_string()));
                    let _ = write!(
                        date_html,
                        "<li><a href=\"{}\">{value}</a></li>",
                        esc(&query.with(&all_keys, &set))
                    );
                }
                date_html.push_str("</ul></nav>");
            }
        }
    }

    // Header row with sort links.
    let mut head = String::from(
        "<tr><th class=\"action-checkbox-column\"><input type=\"checkbox\" id=\"action-toggle\" aria-label=\"Select all\"></th>",
    );
    for (index, col) in cols.iter().enumerate() {
        let current = order.iter().find(|(i, _)| *i == index);
        if col.order.is_some() {
            let number = index + 1;
            let next = match current {
                Some((_, false)) => format!("-{number}"),
                _ => number.to_string(),
            };
            let class = match current {
                Some((_, false)) => " sorted ascending",
                Some((_, true)) => " sorted descending",
                None => "",
            };
            let _ = write!(
                head,
                "<th class=\"sortable{class}\"><a href=\"{}\">{}</a></th>",
                esc(&query.with(&["o"], &[("o", next)])),
                esc(&col.header)
            );
        } else {
            let _ = write!(head, "<th>{}</th>", esc(&col.header));
        }
    }
    head.push_str("</tr>");

    let mut body = String::new();
    for row in &rows {
        let Some(key) = key_at(row, 0, model.key) else {
            continue;
        };
        let _ = write!(
            body,
            "<tr><td class=\"action-checkbox\"><input type=\"checkbox\" name=\"_selected_action\" value=\"{}\" class=\"action-select\" aria-label=\"Select this object\"></td>",
            esc(&key.to_string())
        );
        for (index, col) in cols.iter().enumerate() {
            let value = text_at(row, index + 1);
            let shown = html::display(col.kind, value.as_deref(), true);
            if index == 0 {
                let shown = if shown.is_empty() {
                    html::EMPTY.to_string()
                } else {
                    shown
                };
                let _ = write!(
                    body,
                    "<th><a href=\"{}\">{shown}</a></th>",
                    esc(&model.object_url(&key))
                );
            } else {
                let _ = write!(body, "<td>{shown}</td>");
            }
        }
        body.push_str("</tr>");
    }

    // Actions.
    let mut actions = String::new();
    let can_delete = model.delete_selected && staff.perms.delete(model);
    let can_act = !model.actions.is_empty() && staff.perms.change(model);
    let can_export = !model.exports.is_empty();
    if can_delete || can_act || can_export {
        actions.push_str("<div class=\"actions\"><label>Action: <select name=\"action\" required><option value=\"\" selected>---------</option>");
        if can_delete {
            let _ = write!(
                actions,
                "<option value=\"delete_selected\">Delete selected {}</option>",
                esc(model.plural)
            );
        }
        if can_act {
            for action in model.actions {
                let _ = write!(
                    actions,
                    "<option value=\"{}\">{}</option>",
                    esc(action.name),
                    esc(action.label)
                );
            }
        }
        for export in model.exports {
            let _ = write!(
                actions,
                "<option value=\"{}\">{}</option>",
                esc(export.name),
                esc(export.label)
            );
        }
        actions.push_str("</select></label> <button type=\"submit\" class=\"button\" title=\"Run the selected action\">Go</button></div>");
    }

    let mut content = String::new();
    if staff.perms.add(model) {
        let _ = write!(
            content,
            "<ul class=\"object-tools\"><li><a href=\"{}add/\" class=\"addlink\">Add {}</a></li></ul>",
            esc(&model.url()),
            esc(model.verbose)
        );
    }
    content.push_str("<div id=\"changelist\"><div class=\"changelist-main\">");
    if !model.search.is_empty() {
        let hidden: String = query
            .0
            .iter()
            .filter(|(k, _)| k != "q" && k != "p")
            .map(|(k, v)| {
                format!(
                    "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
                    esc(k),
                    esc(v)
                )
            })
            .collect();
        let counts = if filtered {
            format!(
                " <span class=\"small quiet\">{count} result{} (<a href=\"?\">{total} total</a>)</span>",
                if count == 1 { "" } else { "s" }
            )
        } else {
            String::new()
        };
        let _ = write!(
            content,
            "<div id=\"toolbar\"><form id=\"changelist-search\" method=\"get\" role=\"search\">\
             <label for=\"searchbar\" class=\"visually-hidden\">Search {plural}</label>\
             <input type=\"text\" size=\"40\" name=\"q\" value=\"{q}\" id=\"searchbar\" autofocus>\
             <input type=\"submit\" value=\"Search\">{counts}{hidden}</form></div>",
            plural = esc(model.plural),
            q = esc(&q),
        );
    }
    content.push_str(&date_html);
    let _ = write!(
        content,
        "<form id=\"changelist-form\" method=\"post\" novalidate>\
         <input type=\"hidden\" name=\"csrfmiddlewaretoken\" value=\"{}\">{actions}\
         <div class=\"results\"><table id=\"result_list\"><thead>{head}</thead><tbody>{body}</tbody></table></div>",
        esc(&staff.token())
    );
    // Paginator.
    content.push_str("<p class=\"paginator\">");
    if pages > 1 {
        for number in page_numbers(page_number, pages) {
            match number {
                None => content.push_str("<span class=\"dots\">…</span> "),
                Some(n) if n == page_number => {
                    let _ = write!(content, "<span class=\"this-page\">{n}</span> ");
                }
                Some(n) => {
                    let mut link = query.with(&[], &[]);
                    link.push_str(if link.len() > 1 { "&" } else { "" });
                    link.push_str(&format!("p={n}"));
                    let _ = write!(content, "<a href=\"{}\">{n}</a> ", esc(&link));
                }
            }
        }
    }
    let noun = if count == 1 {
        model.verbose
    } else {
        model.plural
    };
    let _ = write!(content, "{count} {}</p></form></div>", esc(noun));
    if !boxes.is_empty() {
        content.push_str("<nav id=\"changelist-filter\" aria-labelledby=\"changelist-filter-header\"><h2 id=\"changelist-filter-header\">Filter</h2>");
        if filtered
            && query
                .0
                .iter()
                .any(|(k, _)| k != "q" && k != "o" && k != "p")
        {
            let keep: Vec<(&str, String)> = ["q", "o"]
                .iter()
                .filter_map(|k| query.get(k).map(|v| (*k, v.to_string())))
                .collect();
            let clear = Query(Vec::new()).with(&[], &keep);
            let _ = write!(
                content,
                "<h3 id=\"changelist-filter-clear\"><a href=\"{}\">&#10006; Clear all filters</a></h3>",
                esc(&clear)
            );
        }
        for filter in &boxes {
            let _ = write!(
                content,
                "<details open><summary>{}</summary><ul>",
                esc(&filter.title)
            );
            for (label, link, selected) in &filter.choices {
                let class = if *selected { " class=\"selected\"" } else { "" };
                let _ = write!(
                    content,
                    "<li{class}><a href=\"{}\">{}</a></li>",
                    esc(link),
                    esc(label)
                );
            }
            content.push_str("</ul></details>");
        }
        content.push_str("</nav>");
    }
    content.push_str("</div>");
    content.push_str(TOGGLE_SCRIPT);
    let title = format!("Select {} to change", model.verbose);
    Ok(staff.page(
        &title,
        &crumbs(&console, model, &[]),
        "change-list",
        &content,
    ))
}

/// The script that checks every row box when the header box is checked.
const TOGGLE_SCRIPT: &str = "<script src=\"/admin/static/admin.js\" defer></script>";

/// Django's elided page range: ends and the neighbourhood of the page.
fn page_numbers(current: i64, pages: i64) -> Vec<Option<i64>> {
    let mut out = Vec::new();
    let mut last = 0;
    for n in 1..=pages {
        if n <= 2 || n > pages - 2 || (n - current).abs() <= 3 {
            if n > last + 1 {
                out.push(None);
            }
            out.push(Some(n));
            last = n;
        }
    }
    out
}

/// The most rows one action or bulk delete takes.
const MAX_SELECTED: usize = 1000;

/// POST on the change list: run an action on the selected rows.
pub async fn action(
    State(console): State<Console>,
    staff: Staff,
    UrlPath((app, name)): UrlPath<(String, String)>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    form: Form,
) -> AdminResult<Response> {
    let model = find_model(&console, &app, &name)?;
    staff.check_csrf(&headers, &form)?;
    if !staff.perms.view(model) {
        return Err(AdminError::Forbidden);
    }
    let back = match raw.as_deref() {
        Some(q) if !q.is_empty() => format!("{}?{q}", model.url()),
        _ => model.url(),
    };
    let mut keys: Vec<Key> = form
        .all("_selected_action")
        .filter_map(|v| model.key.parse(v))
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys.truncate(MAX_SELECTED);
    let name = form.get("action").unwrap_or("");
    if name.is_empty() {
        return Ok(staff.redirect(&back, "warning", "No action selected."));
    }
    if keys.is_empty() {
        return Ok(staff.redirect(
            &back,
            "warning",
            "Items must be selected in order to perform actions on them. No items have been changed.",
        ));
    }
    if name == "delete_selected" {
        if !(model.delete_selected && staff.perms.delete(model)) {
            return Err(AdminError::Forbidden);
        }
        return delete_selected(&console, &staff, model, &keys, &form, &back).await;
    }
    if let Some(export) = model.exports.iter().find(|e| e.name == name) {
        let mut db = console.db().acquire().await?;
        let file = (export.run)(&mut db, &keys).await?;
        let disposition = format!("attachment; filename=\"{}\"", file.filename);
        return Ok((
            [
                (header::CONTENT_TYPE, file.content_type.to_string()),
                (header::CONTENT_DISPOSITION, disposition),
            ],
            file.body,
        )
            .into_response());
    }
    let Some(action) = model.actions.iter().find(|a| a.name == name) else {
        return Ok(staff.redirect(&back, "warning", "No action selected."));
    };
    if !staff.perms.change(model) {
        return Err(AdminError::Forbidden);
    }
    let mut tx = console.db().begin().await?;
    let found = query::reprs(&mut tx, model, &keys).await?;
    let found_keys: Vec<Key> = found.iter().map(|(key, _)| key.clone()).collect();
    let message = match (action.run)(&mut tx, &found_keys).await {
        Ok(message) => message,
        Err(error) => match error.downcast::<Refusal>() {
            Ok(refusal) => {
                tx.rollback().await?;
                return Ok(staff.redirect(&back, "error", &refusal.0));
            }
            Err(error) => return Err(error.into()),
        },
    };
    if let Some(history) = &console.inner().history {
        for (key, text) in &found {
            history
                .record(
                    &mut tx,
                    Change {
                        staff_id: &staff.id,
                        model,
                        key,
                        repr: text,
                        event: Event::Action(action.label),
                    },
                )
                .await?;
        }
    }
    tx.commit().await?;
    Ok(staff.redirect(&back, "success", &message))
}

async fn delete_selected(
    console: &Console,
    staff: &Staff,
    model: &'static Model,
    keys: &[Key],
    form: &Form,
    back: &str,
) -> AdminResult<Response> {
    let plan = Plan::new(console.schema(), model)?;
    let mut tx = console.db().begin().await?;
    let found = query::reprs(&mut tx, model, keys).await?;
    let keys: Vec<Key> = found.iter().map(|(key, _)| key.clone()).collect();
    if keys.is_empty() {
        return Ok(staff.redirect(back, "warning", "No items were found to delete."));
    }
    let summary = plan.summary(&mut tx, &keys, &staff.perms).await?;
    if form.get("post") == Some("yes") && !summary.blocked() {
        plan.execute(&mut tx, &keys).await?;
        if let Some(history) = &console.inner().history {
            for (key, text) in &found {
                history
                    .record(
                        &mut tx,
                        Change {
                            staff_id: &staff.id,
                            model,
                            key,
                            repr: text,
                            event: Event::Deleted,
                        },
                    )
                    .await?;
            }
        }
        tx.commit().await?;
        let noun = if keys.len() == 1 {
            model.verbose
        } else {
            model.plural
        };
        return Ok(staff.redirect(
            back,
            "success",
            &format!("Successfully deleted {} {noun}.", keys.len()),
        ));
    }
    tx.rollback().await?;
    let mut content = String::new();
    if summary.blocked() {
        content.push_str(&blocked_html(model, &summary));
    } else {
        let _ = write!(
            content,
            "<p>Are you sure you want to delete the selected {}? All of the following objects and their related items will be deleted:</p>{}\
             <form method=\"post\"><input type=\"hidden\" name=\"csrfmiddlewaretoken\" value=\"{}\">\
             <input type=\"hidden\" name=\"action\" value=\"delete_selected\"><input type=\"hidden\" name=\"post\" value=\"yes\">",
            esc(model.plural),
            summary.html(),
            esc(&staff.token())
        );
        for key in &keys {
            let _ = write!(
                content,
                "<input type=\"hidden\" name=\"_selected_action\" value=\"{}\">",
                esc(&key.to_string())
            );
        }
        let _ = write!(
            content,
            "<div class=\"submit-row\"><input type=\"submit\" class=\"deletelink\" value=\"Yes, I’m sure\"> \
             <a href=\"{}\" class=\"button cancel-link\">No, take me back</a></div></form>",
            esc(back)
        );
    }
    Ok(staff.page(
        "Delete multiple objects",
        &crumbs(
            console,
            model,
            &[("Delete multiple objects".to_string(), String::new())],
        ),
        "delete-confirmation",
        &content,
    ))
}

/// Django's refusal when related rows block a delete.
pub fn blocked_html(model: &Model, summary: &crate::delete::Summary) -> String {
    let mut out = String::new();
    if !summary.perms_needed.is_empty() {
        let _ = write!(
            out,
            "<p>Deleting the selected {} would result in deleting related objects, but your account doesn't have permission to delete the following types of objects:</p><ul>",
            esc(model.verbose)
        );
        for name in &summary.perms_needed {
            let _ = write!(out, "<li>{}</li>", esc(name));
        }
        out.push_str("</ul>");
    }
    if !summary.protected.is_empty() {
        let _ = write!(
            out,
            "<p>Deleting the selected {} would require deleting the following protected related objects:</p><ul>",
            esc(model.verbose)
        );
        for item in &summary.protected {
            let _ = write!(out, "<li>{}</li>", esc(item));
        }
        out.push_str("</ul>");
    }
    out
}

/// Binding helper for queries whose parameters are all text.
trait BindAllText<'q> {
    fn bind_all_text(self, params: &'q Params) -> Self;
}

impl<'q, O> BindAllText<'q>
    for sqlx::query::QueryScalar<'q, sqlx::Postgres, O, sqlx::postgres::PgArguments>
{
    fn bind_all_text(self, params: &'q Params) -> Self {
        params.0.iter().fold(self, |q, v| q.bind(v.as_deref()))
    }
}

impl<'q, O> BindAllText<'q>
    for sqlx::query::QueryAs<'q, sqlx::Postgres, O, sqlx::postgres::PgArguments>
{
    fn bind_all_text(self, params: &'q Params) -> Self {
        params.0.iter().fold(self, |q, v| q.bind(v.as_deref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_column_ordering() {
        assert_eq!(parse_order(Some("2.-1"), 3), vec![(1, false), (0, true)]);
        assert_eq!(parse_order(Some("9.x.-0"), 3), vec![]);
    }

    #[test]
    fn elides_long_page_ranges() {
        assert_eq!(page_numbers(1, 3), vec![Some(1), Some(2), Some(3)]);
        let pages = page_numbers(10, 30);
        assert_eq!(pages.first(), Some(&Some(1)));
        assert!(pages.contains(&None));
        assert!(pages.contains(&Some(13)) && !pages.contains(&Some(14)));
    }
}
