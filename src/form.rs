//! The add, change and delete pages.

use std::fmt::Write;

use axum::{
    extract::{Path as UrlPath, State},
    http::HeaderMap,
    response::Response,
};
use chrono::{NaiveDate, NaiveDateTime};
use rust_decimal::Decimal;
use sqlx::PgConnection;

use crate::{
    Console,
    changelist::blocked_html,
    delete::Plan,
    history::{Change, Event},
    html::{self, esc},
    key::{Key, KeyKind},
    pages::{crumbs, find_model, parse_key},
    query::{self, Params, SELECT_THRESHOLD, Values, bind_all, cast_param, key_at, on, text_at},
    registry::{Auto, ComputedRender, Field, Fieldset, Initial, Kind, Model, Refusal, Saved},
    session::{AdminError, AdminResult, Form, Staff},
};

const REQUIRED: &str = "This field is required.";
/// Names of the add form's password pair (Django's `UserCreationForm`).
const PASSWORD1: &str = "password1";
const PASSWORD2: &str = "password2";

// ---------------------------------------------------------------------------
// Values from the form

/// A submitted value as the text bound for its column, or an error. `fk`
/// is the key type of the table a foreign key field points to.
pub(crate) fn parse(
    field: &Field,
    raw: Option<&str>,
    fk: KeyKind,
) -> Result<Option<String>, String> {
    if matches!(field.kind, Kind::Bool) && !field.null {
        return Ok(Some(
            raw.is_some_and(|v| v == "on" || v == "true" || v == "1")
                .to_string(),
        ));
    }
    let trimmed = raw.map(str::trim).unwrap_or("");
    let text = match field.kind {
        // Django strips surrounding whitespace of text inputs.
        Kind::Text | Kind::Json => raw.unwrap_or("").to_string(),
        _ => trimmed.to_string(),
    };
    if text.trim().is_empty() {
        return if field.null {
            Ok(None)
        } else if field.blank && matches!(field.kind, Kind::Char | Kind::Text | Kind::Choice(_)) {
            Ok(Some(String::new()))
        } else {
            Err(REQUIRED.to_string())
        };
    }
    let value = match field.kind {
        Kind::Char | Kind::Text | Kind::Password => {
            if let Some(max) = field.max_length {
                let length = text.chars().count();
                if length > max as usize {
                    return Err(format!(
                        "Ensure this value has at most {max} characters (it has {length})."
                    ));
                }
            }
            text
        }
        Kind::Choice(choices) => {
            if !choices.iter().any(|(stored, _)| *stored == text) {
                return Err(format!(
                    "Select a valid choice. {text} is not one of the available choices."
                ));
            }
            text
        }
        Kind::Int => text
            .parse::<i64>()
            .map_err(|_| "Enter a whole number.".to_string())?
            .to_string(),
        Kind::ForeignKey(_) => match fk {
            KeyKind::Int => text
                .parse::<i64>()
                .map_err(|_| "Enter a whole number.".to_string())?
                .to_string(),
            KeyKind::Uuid => fk
                .parse(&text)
                .ok_or_else(|| "Enter a valid UUID.".to_string())?
                .to_string(),
            KeyKind::Text => text,
        },
        Kind::Decimal(places) => {
            let number: Decimal = text.parse().map_err(|_| "Enter a number.".to_string())?;
            if number.scale() > places {
                return Err(format!(
                    "Ensure that there are no more than {places} decimal places."
                ));
            }
            number.to_string()
        }
        Kind::Float => {
            let number: f64 = text.parse().map_err(|_| "Enter a number.".to_string())?;
            if !number.is_finite() {
                return Err("Enter a number.".to_string());
            }
            number.to_string()
        }
        Kind::Bool => match text.as_str() {
            "true" | "True" | "1" | "on" => "true".to_string(),
            "false" | "False" | "0" => "false".to_string(),
            _ => return Err("Select a valid choice.".to_string()),
        },
        Kind::DateTime => [
            "%Y-%m-%dT%H:%M:%S",
            "%Y-%m-%dT%H:%M",
            "%Y-%m-%d %H:%M:%S",
            "%Y-%m-%d %H:%M",
        ]
        .iter()
        .find_map(|format| NaiveDateTime::parse_from_str(&text, format).ok())
        .map(|at| at.format("%Y-%m-%dT%H:%M:%S").to_string())
        .ok_or_else(|| "Enter a valid date/time.".to_string())?,
        Kind::Date => NaiveDate::parse_from_str(&text, "%Y-%m-%d")
            .map(|day| day.to_string())
            .map_err(|_| "Enter a valid date.".to_string())?,
        Kind::Duration => parse_duration(&text)
            .ok_or_else(|| "Enter a valid duration.".to_string())?
            .to_string(),
        Kind::Json => serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|_| "Enter a valid JSON.".to_string())?
            .to_string(),
    };
    Ok(Some(value))
}

/// Django's `parse_duration` for its own format: `[-]D [days, ]HH:MM:SS`,
/// `MM:SS` or seconds. Returns whole seconds.
pub fn parse_duration(text: &str) -> Option<i64> {
    let text = text.trim();
    let (days, clock) = match text.split_once(' ') {
        Some((days, rest)) => {
            let rest = rest.trim_start_matches("days,").trim_start_matches("day,");
            let rest = rest
                .trim_start_matches("days")
                .trim_start_matches("day")
                .trim();
            (days.parse::<i64>().ok()?, rest)
        }
        None => (0, text),
    };
    let (negative, clock) = match clock.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, clock),
    };
    let parts: Vec<&str> = clock.split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut seconds: i64 = 0;
    for (index, part) in parts.iter().enumerate() {
        let whole = part.split(['.', ',']).next()?;
        let value: i64 = whole.parse().ok()?;
        if value < 0 || (index > 0 && value >= 60 && parts.len() > 1) {
            return None;
        }
        seconds = seconds.checked_mul(60)?.checked_add(value)?;
    }
    let seconds = if negative { -seconds } else { seconds };
    days.checked_mul(86_400)?.checked_add(seconds)
}

/// Whether two texts of a field hold the same value.
fn same(kind: Kind, old: Option<&str>, new: Option<&str>) -> bool {
    match (old, new) {
        (Some(a), Some(b)) => match kind {
            Kind::Decimal(_) => a.parse::<Decimal>().ok() == b.parse::<Decimal>().ok(),
            Kind::Float => a.parse::<f64>().ok() == b.parse::<f64>().ok(),
            Kind::Json => {
                serde_json::from_str::<serde_json::Value>(a).ok()
                    == serde_json::from_str::<serde_json::Value>(b).ok()
            }
            _ => a == b,
        },
        (a, b) => a == b,
    }
}

// ---------------------------------------------------------------------------
// Layout

/// The fieldsets a page shows: the declared ones or, like Django, every
/// editable field followed by the read-only ones.
/// One fieldset of a page.
struct Set {
    name: Option<&'static str>,
    fields: Vec<&'static str>,
    collapse: bool,
    description: &'static str,
}

impl From<&Fieldset> for Set {
    fn from(set: &Fieldset) -> Self {
        Self {
            name: set.name,
            fields: set.fields.to_vec(),
            collapse: set.collapse,
            description: set.description,
        }
    }
}

fn fieldsets(model: &Model, add: bool) -> Vec<Set> {
    if add && !model.add_fieldsets.is_empty() {
        return model.add_fieldsets.iter().map(Set::from).collect();
    }
    if !model.fieldsets.is_empty() {
        return model.fieldsets.iter().map(Set::from).collect();
    }
    let names: Vec<&'static str> = model
        .fields
        .iter()
        .filter(|f| {
            f.editable() && !matches!(f.kind, Kind::Password) && !model.readonly.contains(&f.name)
        })
        .map(|f| f.name)
        .chain(model.m2m.iter().map(|m| m.name))
        .chain(model.readonly.iter().copied())
        .collect();
    vec![Set {
        name: None,
        fields: names,
        collapse: false,
        description: "",
    }]
}

/// Field names the form edits.
/// On an add page with `add_fieldsets` those fields are editable even when
/// read-only on the change page (Django's `UserAdmin` and `TokenAdmin`).
fn edited(model: &Model, sets: &[Set], view_only: bool, add: bool) -> Vec<&'static Field> {
    if view_only {
        return Vec::new();
    }
    let add_form = add && !model.add_fieldsets.is_empty();
    sets.iter()
        .flat_map(|set| set.fields.iter())
        .filter_map(|name| model.field(name))
        .filter(|f| {
            f.editable()
                && !matches!(f.kind, Kind::Password)
                && (add_form || !model.readonly.contains(&f.name))
        })
        .collect()
}

struct FormView<'a> {
    model: &'static Model,
    key: Option<&'a Key>,
    values: &'a Values,
    m2m: &'a [(&'static str, Vec<String>)],
    errors: &'a [(String, String)],
    view_only: bool,
    add: bool,
}

async fn widget(
    console: &Console,
    db: &mut PgConnection,
    field: &Field,
    value: Option<&str>,
) -> sqlx::Result<String> {
    let name = esc(field.name);
    let id = format!("id_{name}");
    let required = if field.blank || matches!(field.kind, Kind::Bool) {
        ""
    } else {
        " required"
    };
    let text = esc(value.unwrap_or(""));
    Ok(match field.kind {
        Kind::Char | Kind::Password => {
            let max = field
                .max_length
                .map(|m| format!(" maxlength=\"{m}\""))
                .unwrap_or_default();
            format!(
                "<input type=\"text\" name=\"{name}\" id=\"{id}\" value=\"{text}\" class=\"vTextField\"{max}{required}>"
            )
        }
        Kind::Text => format!(
            "<textarea name=\"{name}\" id=\"{id}\" rows=\"6\" cols=\"60\" class=\"vLargeTextField\">{text}</textarea>"
        ),
        Kind::Json => format!(
            "<textarea name=\"{name}\" id=\"{id}\" rows=\"8\" cols=\"60\" class=\"vLargeTextField mono\">{}</textarea>",
            esc(&value.map(html::pretty_json).unwrap_or_default())
        ),
        Kind::Int => format!(
            "<input type=\"number\" step=\"1\" name=\"{name}\" id=\"{id}\" value=\"{text}\" class=\"vIntegerField\"{required}>"
        ),
        Kind::Decimal(places) => {
            let step = if places == 0 {
                "1".to_string()
            } else {
                format!("0.{}1", "0".repeat(places as usize - 1))
            };
            format!(
                "<input type=\"number\" step=\"{step}\" name=\"{name}\" id=\"{id}\" value=\"{text}\" class=\"vDecimalField\"{required}>"
            )
        }
        Kind::Float => format!(
            "<input type=\"number\" step=\"any\" name=\"{name}\" id=\"{id}\" value=\"{text}\"{required}>"
        ),
        Kind::Bool if field.null => {
            let option = |v: &str, label: &str| {
                let selected = if value == Some(v) || (v.is_empty() && value.is_none()) {
                    " selected"
                } else {
                    ""
                };
                format!("<option value=\"{v}\"{selected}>{label}</option>")
            };
            format!(
                "<select name=\"{name}\" id=\"{id}\">{}{}{}</select>",
                option("", "Unknown"),
                option("true", "Yes"),
                option("false", "No")
            )
        }
        Kind::Bool => {
            let checked = if value == Some("true") {
                " checked"
            } else {
                ""
            };
            format!("<input type=\"checkbox\" name=\"{name}\" id=\"{id}\"{checked}>")
        }
        Kind::DateTime => format!(
            "<input type=\"datetime-local\" step=\"1\" name=\"{name}\" id=\"{id}\" value=\"{text}\"{required}> <span class=\"timezonewarning\">{}</span>",
            esc(console.time_zone().name())
        ),
        Kind::Date => {
            format!("<input type=\"date\" name=\"{name}\" id=\"{id}\" value=\"{text}\"{required}>")
        }
        Kind::Duration => {
            let shown = value
                .and_then(|v| v.parse::<i64>().ok())
                .map_or_else(|| value.unwrap_or("").to_string(), html::duration_string);
            format!(
                "<input type=\"text\" name=\"{name}\" id=\"{id}\" value=\"{}\" placeholder=\"[DD] [HH:[MM:]]ss\"{required}>",
                esc(&shown)
            )
        }
        Kind::Choice(choices) => {
            let mut out = format!("<select name=\"{name}\" id=\"{id}\">");
            if field.blank || value.is_none() {
                out.push_str("<option value=\"\">---------</option>");
            }
            for (stored, label) in choices {
                let selected = if value == Some(*stored) {
                    " selected"
                } else {
                    ""
                };
                let _ = write!(
                    out,
                    "<option value=\"{}\"{selected}>{}</option>",
                    esc(stored),
                    esc(label)
                );
            }
            out.push_str("</select>");
            out
        }
        Kind::ForeignKey(table) => {
            if query::at_most(db, table, SELECT_THRESHOLD).await? {
                let mut out = format!(
                    "<select name=\"{name}\" id=\"{id}\"><option value=\"\">---------</option>"
                );
                for (option, label) in query::options(console, db, table).await? {
                    let selected = if value == Some(option.as_str()) {
                        " selected"
                    } else {
                        ""
                    };
                    let _ = write!(
                        out,
                        "<option value=\"{}\"{selected}>{}</option>",
                        esc(&option),
                        esc(&label)
                    );
                }
                out.push_str("</select>");
                out
            } else {
                let current = fk_text(console, db, table, value).await?;
                let input = match console.table_key(table).kind {
                    KeyKind::Int => "type=\"number\" step=\"1\"",
                    KeyKind::Uuid | KeyKind::Text => "type=\"text\"",
                };
                format!(
                    "<input {input} name=\"{name}\" id=\"{id}\" value=\"{text}\" class=\"vForeignKeyRawIdAdminField\"{required}> {current}"
                )
            }
        }
    })
}

/// The linked text of a foreign key's target.
async fn fk_text(
    console: &Console,
    db: &mut PgConnection,
    table: &str,
    value: Option<&str>,
) -> sqlx::Result<String> {
    let kind = console.table_key(table).kind;
    let Some(key) = value.and_then(|v| kind.parse(v)) else {
        return Ok(String::new());
    };
    let text: Option<String> = sqlx::query_scalar(&format!(
        "SELECT ({})::text",
        query::repr_of(console, table, &kind.cast("$1"))
    ))
    .bind(key.to_string())
    .fetch_one(&mut *db)
    .await?;
    let text = esc(&text.unwrap_or_else(|| format!("#{key} (missing)")));
    Ok(match console.model_for_table(table) {
        Some(model) => format!(
            "<strong><a href=\"{}\">{text}</a></strong>",
            esc(&model.object_url(&key))
        ),
        None => format!("<strong>{text}</strong>"),
    })
}

async fn readonly_value(
    console: &Console,
    db: &mut PgConnection,
    view: &FormView<'_>,
    name: &str,
) -> anyhow::Result<(String, String)> {
    let model = view.model;
    if let Some(field) = model.field(name) {
        let value = view.values.get(name);
        let shown = match field.kind {
            Kind::ForeignKey(table) => {
                let linked = fk_text(console, db, table, value).await?;
                if linked.is_empty() {
                    html::EMPTY.to_string()
                } else {
                    linked
                }
            }
            Kind::Password => {
                let passwords = console.inner().passwords.as_deref();
                let summary = match (passwords, value) {
                    (Some(passwords), Some(stored)) => esc(&passwords.summary(stored)),
                    _ => html::display(field.kind, value, false),
                };
                let change_url = match (passwords, view.key) {
                    (Some(passwords), Some(key)) => passwords.change_url(model, key),
                    _ => None,
                };
                match change_url {
                    Some(url) if !(view.view_only || view.add) => format!(
                        "{summary}<div class=\"help\">Raw passwords are not stored, so there is no way to see this user’s password, but you can change the password using <a href=\"{}\">this form</a>.</div>",
                        esc(&url)
                    ),
                    _ => summary,
                }
            }
            kind => html::display(kind, value, false),
        };
        return Ok((field.label.to_string(), shown));
    }
    if let Some(m2m) = model.m2m_field(name) {
        let selected = view
            .m2m
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, s)| s.clone())
            .unwrap_or_default();
        let kind = console.table_key(m2m.target).kind;
        let keys: Vec<Key> = selected.iter().filter_map(|v| kind.parse(v)).collect();
        let sql = format!(
            "SELECT label FROM ({}) o(id, label) WHERE o.id = ANY({})",
            m2m.options_sql,
            kind.cast_array("$1")
        );
        let labels: Vec<String> = sqlx::query_scalar(&sql)
            .bind(Key::texts(&keys))
            .fetch_all(&mut *db)
            .await?;
        let shown = if labels.is_empty() {
            html::EMPTY.to_string()
        } else {
            labels.iter().map(|l| esc(l)).collect::<Vec<_>>().join(", ")
        };
        return Ok((m2m.label.to_string(), shown));
    }
    if let Some(computed) = model.computed_field(name) {
        let shown = match (view.key, computed.render) {
            (None, _) => html::EMPTY.to_string(),
            (Some(key), ComputedRender::Sql(sql, kind)) => {
                let text: Option<String> = sqlx::query_scalar(&format!(
                    "SELECT {} FROM {} t WHERE t.{} = {}",
                    console.as_text(&on(sql, "t"), kind),
                    model.table,
                    model.pk,
                    model.key.cast("$1")
                ))
                .bind(key.to_string())
                .fetch_optional(&mut *db)
                .await?
                .flatten();
                html::display(kind, text.as_deref(), false)
            }
            (Some(key), ComputedRender::Html(render)) => render(&mut *db, key.clone()).await?,
        };
        return Ok((computed.label.to_string(), shown));
    }
    anyhow::bail!(
        "unknown field {name} in {}.{} fieldsets",
        model.app,
        model.name
    )
}

async fn m2m_widget(
    db: &mut PgConnection,
    view: &FormView<'_>,
    name: &str,
) -> anyhow::Result<Option<(String, String)>> {
    let Some(m2m) = view.model.m2m_field(name) else {
        return Ok(None);
    };
    let selected = view
        .m2m
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| s.as_slice())
        .unwrap_or(&[]);
    let rows = sqlx::query(&format!(
        "SELECT id::text, label::text FROM ({}) o(id, label) LIMIT 1000",
        m2m.options_sql
    ))
    .fetch_all(&mut *db)
    .await?;
    let mut out = format!(
        "<select name=\"{0}\" id=\"id_{0}\" multiple size=\"10\" class=\"filtered\">",
        esc(name)
    );
    for row in &rows {
        let id = text_at(row, 0).unwrap_or_default();
        let label = text_at(row, 1).unwrap_or_default();
        let mark = if selected.contains(&id) {
            " selected"
        } else {
            ""
        };
        let _ = write!(
            out,
            "<option value=\"{}\"{mark}>{}</option>",
            esc(&id),
            esc(&label)
        );
    }
    out.push_str("</select>");
    if !m2m.help.is_empty() {
        let _ = write!(out, "<div class=\"help\">{}</div>", esc(m2m.help));
    }
    Ok(Some((m2m.label.to_string(), out)))
}

async fn render_form(
    console: &Console,
    db: &mut PgConnection,
    staff: &Staff,
    view: &FormView<'_>,
) -> anyhow::Result<String> {
    let model = view.model;
    let sets = fieldsets(model, view.add);
    let edit = edited(model, &sets, view.view_only, view.add);
    let mut out = String::new();
    if !view.view_only {
        let _ = write!(
            out,
            "<form method=\"post\" id=\"{}_form\" novalidate><input type=\"hidden\" name=\"csrfmiddlewaretoken\" value=\"{}\">",
            esc(model.name),
            esc(&staff.token())
        );
    }
    if !view.errors.is_empty() {
        let note = if view.errors.len() == 1 {
            "Please correct the error below."
        } else {
            "Please correct the errors below."
        };
        let _ = write!(out, "<p class=\"errornote\">{note}</p>");
        for (name, message) in view.errors {
            if name.is_empty() {
                let _ = write!(
                    out,
                    "<ul class=\"errorlist nonfield\"><li>{}</li></ul>",
                    esc(message)
                );
            }
        }
    }
    for set in &sets {
        let class = if set.collapse {
            "module aligned collapse"
        } else {
            "module aligned"
        };
        out.push_str(&format!("<fieldset class=\"{class}\">"));
        if let Some(name) = set.name {
            let _ = write!(out, "<h2>{}</h2>", esc(name));
        }
        if !set.description.is_empty() {
            let _ = write!(
                out,
                "<div class=\"description\">{}</div>",
                esc(set.description)
            );
        }
        for &name in &set.fields {
            let errors: Vec<&String> = view
                .errors
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, m)| m)
                .collect();
            let (label, content, required) = if name == PASSWORD1 || name == PASSWORD2 {
                let label = if name == PASSWORD1 {
                    "Password"
                } else {
                    "Password confirmation"
                };
                let help = if name == PASSWORD2 {
                    "<div class=\"help\">Enter the same password as before, for verification.</div>"
                } else {
                    ""
                };
                (
                    label.to_string(),
                    format!(
                        "<input type=\"password\" name=\"{name}\" id=\"id_{name}\" autocomplete=\"new-password\" required>{help}"
                    ),
                    true,
                )
            } else if let Some(field) = edit.iter().find(|f| f.name == name) {
                let mut content = widget(console, db, field, view.values.get(name)).await?;
                if !field.help.is_empty() {
                    let _ = write!(content, "<div class=\"help\">{}</div>", esc(field.help));
                }
                (
                    field.label.to_string(),
                    content,
                    !field.blank && !matches!(field.kind, Kind::Bool),
                )
            } else if let (false, Some(widget)) = (
                view.view_only || model.readonly.contains(&name),
                m2m_widget(db, view, name).await?,
            ) {
                (widget.0, widget.1, false)
            } else {
                let (label, shown) = readonly_value(console, db, view, name).await?;
                (
                    label,
                    format!("<div class=\"readonly\">{shown}</div>"),
                    false,
                )
            };
            let error_class = if errors.is_empty() { "" } else { " errors" };
            let _ = write!(
                out,
                "<div class=\"form-row field-{}{error_class}\">",
                esc(name)
            );
            if !errors.is_empty() {
                out.push_str("<ul class=\"errorlist\">");
                for message in errors {
                    let _ = write!(out, "<li>{}</li>", esc(message));
                }
                out.push_str("</ul>");
            }
            let required = if required { " class=\"required\"" } else { "" };
            let _ = write!(
                out,
                "<div class=\"flex-container\"><label for=\"id_{}\"{required}>{}:</label>{content}</div></div>",
                esc(name),
                esc(&label)
            );
        }
        out.push_str("</fieldset>");
    }
    if let Some(key) = view.key {
        out.push_str(&inlines(console, db, staff, model, key).await?);
    }
    if !view.view_only {
        out.push_str("<div class=\"submit-row\"><input type=\"submit\" value=\"Save\" class=\"default\" name=\"_save\">");
        if staff.perms.add(model) {
            out.push_str(
                " <input type=\"submit\" value=\"Save and add another\" name=\"_addanother\">",
            );
        }
        out.push_str(
            " <input type=\"submit\" value=\"Save and continue editing\" name=\"_continue\">",
        );
        if view.key.is_some() && staff.perms.delete(model) {
            out.push_str(" <a href=\"../delete/\" class=\"deletelink\">Delete</a>");
        }
        out.push_str("</div></form>");
    } else {
        let _ = write!(
            out,
            "<div class=\"submit-row\"><a href=\"{}\" class=\"closelink\">Close</a></div>",
            esc(&model.url())
        );
    }
    Ok(out)
}

/// The most rows one inline shows.
const INLINE_LIMIT: i64 = 50;

async fn inlines(
    console: &Console,
    db: &mut PgConnection,
    staff: &Staff,
    model: &Model,
    key: &Key,
) -> anyhow::Result<String> {
    let mut out = String::new();
    for inline in model.inlines {
        // The first cell links to the row itself, or to the row it points at.
        let (link_column, link_table) = inline
            .link
            .unwrap_or((console.table_key(inline.table).column, inline.table));
        let link_kind = console.table_key(link_table).kind;
        let columns: Vec<String> = inline
            .columns
            .iter()
            .map(|(_, sql, kind)| console.as_text(&on(sql, "t"), *kind))
            .collect();
        let sql = format!(
            "SELECT t.{}::text, {} FROM {} t WHERE t.{} = {} ORDER BY {} LIMIT {}",
            link_column,
            if columns.is_empty() {
                "NULL".to_string()
            } else {
                columns.join(", ")
            },
            inline.table,
            inline.fk,
            model.key.cast("$1"),
            on(inline.order, "t"),
            INLINE_LIMIT + 1
        );
        let rows = sqlx::query(&sql)
            .bind(key.to_string())
            .fetch_all(&mut *db)
            .await?;
        let linked = console
            .model_for_table(link_table)
            .filter(|m| staff.perms.view(m));
        let _ = write!(
            out,
            "<div class=\"inline-group\"><fieldset class=\"module\"><h2>{}</h2><table><thead><tr>",
            esc(inline.label)
        );
        for (label, _, _) in inline.columns {
            let _ = write!(out, "<th>{}</th>", esc(label));
        }
        out.push_str("</tr></thead><tbody>");
        if rows.is_empty() {
            let _ = write!(
                out,
                "<tr><td colspan=\"{}\">None</td></tr>",
                inline.columns.len().max(1)
            );
        }
        for row in rows.iter().take(INLINE_LIMIT as usize) {
            let row_key = key_at(row, 0, link_kind);
            out.push_str("<tr>");
            for (index, (_, _, kind)) in inline.columns.iter().enumerate() {
                let shown = html::display(*kind, text_at(row, index + 1).as_deref(), true);
                match (index, linked, &row_key) {
                    (0, Some(target), Some(row_key)) => {
                        let _ = write!(
                            out,
                            "<td><a href=\"{}\">{shown}</a></td>",
                            esc(&target.object_url(row_key))
                        );
                    }
                    _ => {
                        let _ = write!(out, "<td>{shown}</td>");
                    }
                }
            }
            out.push_str("</tr>");
        }
        if rows.len() as i64 > INLINE_LIMIT {
            let _ = write!(
                out,
                "<tr><td colspan=\"{}\">Only the first {INLINE_LIMIT} are shown.</td></tr>",
                inline.columns.len().max(1)
            );
        }
        out.push_str("</tbody></table></fieldset></div>");
    }
    Ok(out)
}

async fn selected_m2m(
    db: &mut PgConnection,
    model: &Model,
    key: &Key,
) -> sqlx::Result<Vec<(&'static str, Vec<String>)>> {
    let mut out = Vec::new();
    for m2m in model.m2m {
        let sql = format!(
            "SELECT {}::text FROM {} WHERE {} = {} ORDER BY 1",
            m2m.other,
            m2m.through,
            m2m.own,
            model.key.cast("$1")
        );
        let ids: Vec<String> = sqlx::query_scalar(&sql)
            .bind(key.to_string())
            .fetch_all(&mut *db)
            .await?;
        out.push((m2m.name, ids));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Saving

struct Submitted {
    values: Values,
    m2m: Vec<(&'static str, Vec<String>)>,
    errors: Vec<(String, String)>,
    /// The new password hash from the add form's password pair.
    password: Option<String>,
}

async fn read_form(
    console: &Console,
    db: &mut PgConnection,
    model: &Model,
    form: &Form,
    sets: &[Set],
    base: &Values,
    add: bool,
) -> anyhow::Result<Submitted> {
    let mut values = base.clone();
    let mut errors = Vec::new();
    for field in edited(model, sets, false, add) {
        let fk = match field.kind {
            Kind::ForeignKey(table) => console.table_key(table).kind,
            _ => KeyKind::Int,
        };
        match parse(field, form.get(field.name), fk) {
            Ok(value) => {
                let target = match (field.kind, value.as_deref().and_then(|v| fk.parse(v))) {
                    (Kind::ForeignKey(table), Some(key)) => Some((table, key)),
                    _ => None,
                };
                if let Some((table, key)) = target
                    && !query::exists(console, db, table, &key).await?
                {
                    errors.push((
                        field.name.to_string(),
                        "Select a valid choice. That choice is not one of the available choices."
                            .to_string(),
                    ));
                }
                values.set(field.name, value);
            }
            Err(message) => {
                values.set(field.name, form.get(field.name).map(str::to_string));
                errors.push((field.name.to_string(), message));
            }
        }
    }
    let mut m2m = Vec::new();
    for name in sets.iter().flat_map(|s| s.fields.iter()) {
        if let Some(field) = model.m2m_field(name) {
            if model.readonly.contains(name) {
                continue;
            }
            let target = console.table_key(field.target);
            let mut keys: Vec<Key> = form
                .all(field.name)
                .filter_map(|v| target.kind.parse(v))
                .collect();
            keys.sort_unstable();
            keys.dedup();
            let sql = format!(
                "SELECT {key}::text FROM {} WHERE {key} = ANY({})",
                field.target,
                target.kind.cast_array("$1"),
                key = target.column,
            );
            let found: Vec<String> = sqlx::query_scalar(&sql)
                .bind(Key::texts(&keys))
                .fetch_all(&mut *db)
                .await?;
            let mut found: Vec<Key> = found.iter().filter_map(|v| target.kind.parse(v)).collect();
            found.sort_unstable();
            if found.len() != keys.len() {
                errors.push((field.name.to_string(), "Select a valid choice.".to_string()));
            }
            m2m.push((field.name, found.iter().map(Key::to_string).collect()));
        }
    }
    let mut password_hash = None;
    if sets.iter().any(|s| s.fields.contains(&PASSWORD1)) {
        let one = form.get(PASSWORD1).unwrap_or("");
        let two = form.get(PASSWORD2).unwrap_or("");
        match validate_password(one, two) {
            Ok(()) => {
                let passwords = console.inner().passwords.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "{}.{} sets a password without a Passwords hook",
                        model.app,
                        model.name
                    )
                })?;
                password_hash = Some(passwords.hash(one.to_string()).await?);
            }
            Err((name, message)) => errors.push((name.to_string(), message)),
        }
    }
    Ok(Submitted {
        values,
        m2m,
        errors,
        password: password_hash,
    })
}

/// Django's `SetPasswordForm` checks of a new password typed twice: both
/// given, equal, and the default validators' length and all-numeric rules.
/// The error names the field it belongs to (`password1` or `password2`).
pub fn validate_password(one: &str, two: &str) -> Result<(), (&'static str, String)> {
    if one.is_empty() {
        return Err((PASSWORD1, REQUIRED.to_string()));
    }
    if two.is_empty() {
        return Err((PASSWORD2, REQUIRED.to_string()));
    }
    if one != two {
        return Err((
            PASSWORD2,
            "The two password fields didn’t match.".to_string(),
        ));
    }
    if one.chars().count() < 8 {
        return Err((
            PASSWORD2,
            "This password is too short. It must contain at least 8 characters.".to_string(),
        ));
    }
    if one.chars().all(|c| c.is_ascii_digit()) {
        return Err((PASSWORD2, "This password is entirely numeric.".to_string()));
    }
    Ok(())
}

/// A database refusal shown on the form instead of a server error.
fn constraint_message(error: &sqlx::Error, model: &Model) -> Option<String> {
    let sqlx::Error::Database(db) = error else {
        return None;
    };
    match db.code().as_deref() {
        Some("23505") => Some(format!(
            "A {} with these values already exists.",
            model.verbose
        )),
        Some("23503") => Some("A related object does not exist.".to_string()),
        Some("23514") | Some("22003") => Some("A value is out of range.".to_string()),
        Some("23502") => Some("A required value is missing.".to_string()),
        Some("22P02") | Some("22007") | Some("22008") => Some("A value is not valid.".to_string()),
        _ => None,
    }
}

/// Runs `work` in a savepoint so a constraint error can be shown on the
/// form while the transaction stays usable.
async fn guarded<T>(
    tx: &mut PgConnection,
    model: &Model,
    work: impl AsyncFnOnce(&mut PgConnection) -> anyhow::Result<T>,
) -> anyhow::Result<Result<T, String>> {
    sqlx::query("SAVEPOINT admin_save")
        .execute(&mut *tx)
        .await?;
    let result = work(&mut *tx).await;
    let constraints = sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
        .execute(&mut *tx)
        .await;
    match (result, constraints) {
        (Ok(value), Ok(_)) => {
            sqlx::query("SET CONSTRAINTS ALL DEFERRED")
                .execute(&mut *tx)
                .await?;
            sqlx::query("RELEASE SAVEPOINT admin_save")
                .execute(&mut *tx)
                .await?;
            Ok(Ok(value))
        }
        (Err(error), _) => {
            let message = match error.downcast_ref::<Refusal>() {
                Some(refusal) => Some(refusal.0.clone()),
                None => error
                    .downcast_ref::<sqlx::Error>()
                    .and_then(|e| constraint_message(e, model)),
            };
            match message {
                Some(message) => {
                    sqlx::query("ROLLBACK TO SAVEPOINT admin_save")
                        .execute(&mut *tx)
                        .await?;
                    Ok(Err(message))
                }
                None => Err(error),
            }
        }
        (Ok(_), Err(error)) => match constraint_message(&error, model) {
            Some(message) => {
                sqlx::query("ROLLBACK TO SAVEPOINT admin_save")
                    .execute(&mut *tx)
                    .await?;
                Ok(Err(message))
            }
            None => Err(error.into()),
        },
    }
}

async fn write_m2m(
    console: &Console,
    db: &mut PgConnection,
    model: &Model,
    key: &Key,
    wanted: &[(&'static str, Vec<String>)],
) -> anyhow::Result<()> {
    for (name, keys) in wanted {
        let Some(m2m) = model.m2m_field(name) else {
            continue;
        };
        let kind = console.table_key(m2m.target).kind;
        let keys: Vec<Key> = keys.iter().filter_map(|v| kind.parse(v)).collect();
        let keys = Key::texts(&keys);
        sqlx::query(&format!(
            "DELETE FROM {} WHERE {} = {} AND NOT ({} = ANY({}))",
            m2m.through,
            m2m.own,
            model.key.cast("$1"),
            m2m.other,
            kind.cast_array("$2")
        ))
        .bind(key.to_string())
        .bind(&keys)
        .execute(&mut *db)
        .await?;
        sqlx::query(&format!(
            "INSERT INTO {t} ({own}, {other}) SELECT {own_key}, x FROM unnest({other_keys}) x \
             WHERE NOT EXISTS (SELECT 1 FROM {t} WHERE {own} = {own_key} AND {other} = x)",
            t = m2m.through,
            own = m2m.own,
            other = m2m.other,
            own_key = model.key.cast("$1"),
            other_keys = kind.cast_array("$2"),
        ))
        .bind(key.to_string())
        .bind(&keys)
        .execute(&mut *db)
        .await?;
    }
    Ok(())
}

async fn insert(
    console: &Console,
    db: &mut PgConnection,
    model: &Model,
    submitted: &Submitted,
    sets: &[Set],
) -> anyhow::Result<Key> {
    let on_form: Vec<&str> = edited(model, sets, false, true)
        .iter()
        .map(|f| f.name)
        .collect();
    let mut params = Params::default();
    let mut columns = Vec::new();
    let mut exprs = Vec::new();
    for field in model.fields {
        let expr = if field.auto != Auto::None {
            Some("now()".to_string())
        } else if matches!(field.kind, Kind::Password) && submitted.password.is_some() {
            Some(cast_param(
                console,
                field.kind,
                &params.push(submitted.password.clone()),
            ))
        } else if on_form.contains(&field.name) {
            Some(cast_param(
                console,
                field.kind,
                &params.push(submitted.values.get(field.name).map(str::to_string)),
            ))
        } else {
            match field.default {
                Initial::Value(value) => Some(cast_param(
                    console,
                    field.kind,
                    &params.push(value.to_string()),
                )),
                Initial::Sql(sql) => Some(sql.to_string()),
                Initial::Gen(generate) => {
                    Some(cast_param(console, field.kind, &params.push(generate())))
                }
                Initial::None if field.null => Some("NULL".to_string()),
                Initial::None => None,
            }
        };
        if let Some(expr) = expr {
            columns.push(field.column);
            exprs.push(expr);
        }
    }
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({}) RETURNING {}::text",
        model.table,
        columns.join(", "),
        exprs.join(", "),
        model.pk
    );
    let row = bind_all(&sql, &params).fetch_one(&mut *db).await?;
    key_at(&row, 0, model.key)
        .ok_or_else(|| anyhow::anyhow!("{} returned no key on insert", model.table))
}

async fn update(
    console: &Console,
    db: &mut PgConnection,
    model: &Model,
    key: &Key,
    changed: &[&'static Field],
    values: &Values,
) -> anyhow::Result<()> {
    let mut params = Params::default();
    let mut sets: Vec<String> = changed
        .iter()
        .map(|field| {
            let placeholder = params.push(values.get(field.name).map(str::to_string));
            format!(
                "{} = {}",
                field.column,
                cast_param(console, field.kind, &placeholder)
            )
        })
        .collect();
    sets.extend(
        model
            .fields
            .iter()
            .filter(|f| f.auto == Auto::OnSave)
            .map(|f| format!("{} = now()", f.column)),
    );
    if sets.is_empty() {
        return Ok(());
    }
    let placeholder = params.push(key.to_string());
    let sql = format!(
        "UPDATE {} SET {} WHERE {} = {}",
        model.table,
        sets.join(", "),
        model.pk,
        model.key.cast(&placeholder)
    );
    bind_all(&sql, &params).execute(&mut *db).await?;
    Ok(())
}

/// Where to go after a save, and the message, like Django's `response_*`.
fn after_save(
    staff: &Staff,
    model: &Model,
    form: &Form,
    key: &Key,
    text: &str,
    added: bool,
) -> Response {
    let verb = if added { "added" } else { "changed" };
    let base = format!("The {} “{}” was {verb} successfully.", model.verbose, text);
    if form.has("_continue") {
        staff.redirect(
            &model.object_url(key),
            "success",
            &format!("{base} You may edit it again below."),
        )
    } else if form.has("_addanother") {
        staff.redirect(
            &format!("{}add/", model.url()),
            "success",
            &format!("{base} You may add another {} below.", model.verbose),
        )
    } else {
        staff.redirect(&model.url(), "success", &base)
    }
}

// ---------------------------------------------------------------------------
// Handlers

fn change_title(model: &Model, editable: bool) -> String {
    format!(
        "{} {}",
        if editable { "Change" } else { "View" },
        model.verbose
    )
}

/// The change page's "History" button, when the console keeps history.
fn object_tools(console: &Console, model: &Model, key: &Key) -> String {
    if console.inner().history.is_none() {
        return String::new();
    }
    format!(
        "<ul class=\"object-tools\"><li><a href=\"{}\" class=\"historylink\">History</a></li></ul>",
        esc(&model.history_url(key))
    )
}

/// Records `event` on the row in the console's history, if it keeps one.
async fn record(
    console: &Console,
    db: &mut PgConnection,
    staff: &Staff,
    model: &Model,
    key: &Key,
    repr: &str,
    event: Event<'_>,
) -> anyhow::Result<()> {
    if let Some(history) = &console.inner().history {
        history
            .record(
                db,
                Change {
                    staff_id: &staff.id,
                    model,
                    key,
                    repr,
                    event,
                },
            )
            .await?;
    }
    Ok(())
}

/// GET `/admin/<app>/<model>/<id>/change/`.
pub async fn change_form(
    State(console): State<Console>,
    staff: Staff,
    UrlPath((app, name, id)): UrlPath<(String, String, String)>,
) -> AdminResult<Response> {
    let model = find_model(&console, &app, &name)?;
    if !staff.perms.view(model) {
        return Err(AdminError::Forbidden);
    }
    let key = parse_key(model, &id)?;
    let mut db = console.db().acquire().await?;
    let (values, text) = query::load(&console, &mut db, model, &key)
        .await?
        .ok_or(AdminError::NotFound)?;
    let m2m = selected_m2m(&mut db, model, &key).await?;
    let editable = staff.perms.change(model);
    let view = FormView {
        model,
        key: Some(&key),
        values: &values,
        m2m: &m2m,
        errors: &[],
        view_only: !editable,
        add: false,
    };
    let content = format!(
        "{}<h2 class=\"object-title\">{}</h2>{}",
        object_tools(&console, model, &key),
        esc(&text),
        render_form(&console, &mut db, &staff, &view).await?
    );
    Ok(staff.page(
        &change_title(model, editable),
        &crumbs(&console, model, &[(text, String::new())]),
        "change-form",
        &content,
    ))
}

/// POST `/admin/<app>/<model>/<id>/change/`.
pub async fn change(
    State(console): State<Console>,
    staff: Staff,
    UrlPath((app, name, id)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
    form: Form,
) -> AdminResult<Response> {
    let model = find_model(&console, &app, &name)?;
    staff.check_csrf(&headers, &form)?;
    if !staff.perms.change(model) {
        return Err(AdminError::Forbidden);
    }
    let key = parse_key(model, &id)?;
    let mut tx = console.db().begin().await?;
    let (old, text) = query::load(&console, &mut tx, model, &key)
        .await?
        .ok_or(AdminError::NotFound)?;
    let old_m2m = selected_m2m(&mut tx, model, &key).await?;
    let sets = fieldsets(model, false);
    let mut submitted = read_form(&console, &mut tx, model, &form, &sets, &old, false).await?;
    if submitted.errors.is_empty() {
        let changed: Vec<&'static Field> = edited(model, &sets, false, false)
            .into_iter()
            .filter(|f| !same(f.kind, old.get(f.name), submitted.values.get(f.name)))
            .collect();
        let changed_m2m: Vec<&'static str> = submitted
            .m2m
            .iter()
            .filter(|(n, ids)| {
                old_m2m
                    .iter()
                    .find(|(o, _)| o == n)
                    .map(|(_, old_ids)| old_ids)
                    != Some(ids)
            })
            .map(|(n, _)| *n)
            .collect();
        let names: Vec<&'static str> = changed
            .iter()
            .map(|f| f.name)
            .chain(changed_m2m.iter().copied())
            .collect();
        let outcome = guarded(&mut tx, model, async |db| {
            update(&console, db, model, &key, &changed, &submitted.values).await?;
            let wanted: Vec<_> = submitted
                .m2m
                .iter()
                .filter(|(n, _)| changed_m2m.contains(n))
                .cloned()
                .collect();
            write_m2m(&console, db, model, &key, &wanted).await?;
            if let Some(hook) = model.after_save {
                hook(
                    db,
                    &Saved {
                        key: key.clone(),
                        created: false,
                        changed: &names,
                        old: Some(&old),
                        new: &submitted.values,
                    },
                )
                .await?;
            }
            Ok(())
        })
        .await?;
        match outcome {
            Ok(()) => {
                let (_, new_text) = query::load(&console, &mut tx, model, &key)
                    .await?
                    .ok_or(AdminError::NotFound)?;
                let labels: Vec<String> = names
                    .iter()
                    .map(|n| {
                        model
                            .field(n)
                            .map(|f| f.label)
                            .or_else(|| model.m2m_field(n).map(|m| m.label))
                            .unwrap_or(n)
                            .to_string()
                    })
                    .collect();
                record(
                    &console,
                    &mut tx,
                    &staff,
                    model,
                    &key,
                    &new_text,
                    Event::Changed(&labels),
                )
                .await?;
                tx.commit().await?;
                return Ok(after_save(&staff, model, &form, &key, &new_text, false));
            }
            Err(message) => submitted.errors.push((String::new(), message)),
        }
    }
    let view = FormView {
        model,
        key: Some(&key),
        values: &submitted.values,
        m2m: &submitted.m2m,
        errors: &submitted.errors,
        view_only: false,
        add: false,
    };
    let content = format!(
        "{}<h2 class=\"object-title\">{}</h2>{}",
        object_tools(&console, model, &key),
        esc(&text),
        render_form(&console, &mut tx, &staff, &view).await?
    );
    tx.rollback().await?;
    Ok(staff.page(
        &change_title(model, true),
        &crumbs(&console, model, &[(text, String::new())]),
        "change-form",
        &content,
    ))
}

/// The add form's starting values: each field's default.
fn initial_values(model: &Model) -> Values {
    Values(
        model
            .fields
            .iter()
            .map(|field| {
                let value = match field.default {
                    Initial::Value(value) => Some(value.to_string()),
                    _ => None,
                };
                (field.name, value)
            })
            .collect(),
    )
}

/// GET `/admin/<app>/<model>/add/`.
pub async fn add_form(
    State(console): State<Console>,
    staff: Staff,
    UrlPath((app, name)): UrlPath<(String, String)>,
) -> AdminResult<Response> {
    let model = find_model(&console, &app, &name)?;
    if !staff.perms.add(model) {
        return Err(AdminError::Forbidden);
    }
    let mut db = console.db().acquire().await?;
    let values = initial_values(model);
    let view = FormView {
        model,
        key: None,
        values: &values,
        m2m: &[],
        errors: &[],
        view_only: false,
        add: true,
    };
    let content = render_form(&console, &mut db, &staff, &view).await?;
    let title = format!("Add {}", model.verbose);
    Ok(staff.page(
        &title,
        &crumbs(&console, model, &[(title.clone(), String::new())]),
        "change-form",
        &content,
    ))
}

/// POST `/admin/<app>/<model>/add/`.
pub async fn add(
    State(console): State<Console>,
    staff: Staff,
    UrlPath((app, name)): UrlPath<(String, String)>,
    headers: HeaderMap,
    form: Form,
) -> AdminResult<Response> {
    let model = find_model(&console, &app, &name)?;
    staff.check_csrf(&headers, &form)?;
    if !staff.perms.add(model) {
        return Err(AdminError::Forbidden);
    }
    let sets = fieldsets(model, true);
    let mut tx = console.db().begin().await?;
    let mut submitted = read_form(
        &console,
        &mut tx,
        model,
        &form,
        &sets,
        &initial_values(model),
        true,
    )
    .await?;
    if submitted.errors.is_empty() {
        let names: Vec<&'static str> = edited(model, &sets, false, true)
            .iter()
            .map(|f| f.name)
            .collect();
        let outcome = guarded(&mut tx, model, async |db| {
            let key = match model.create {
                Some(create) => create(db, &submitted.values).await?,
                None => insert(&console, db, model, &submitted, &sets).await?,
            };
            write_m2m(&console, db, model, &key, &submitted.m2m).await?;
            if let Some(hook) = model.after_save {
                hook(
                    db,
                    &Saved {
                        key: key.clone(),
                        created: true,
                        changed: &names,
                        old: None,
                        new: &submitted.values,
                    },
                )
                .await?;
            }
            Ok(key)
        })
        .await?;
        match outcome {
            Ok(key) => {
                let (_, text) = query::load(&console, &mut tx, model, &key)
                    .await?
                    .ok_or(AdminError::NotFound)?;
                record(&console, &mut tx, &staff, model, &key, &text, Event::Added).await?;
                tx.commit().await?;
                // Django's UserAdmin continues to the change page after an add.
                if sets.iter().any(|s| s.fields.contains(&PASSWORD1))
                    && !form.has("_addanother")
                    && !form.has("_save")
                {
                    return Ok(after_save(
                        &staff,
                        model,
                        &Form(vec![("_continue".into(), String::new())]),
                        &key,
                        &text,
                        true,
                    ));
                }
                return Ok(after_save(&staff, model, &form, &key, &text, true));
            }
            Err(message) => submitted.errors.push((String::new(), message)),
        }
    }
    let view = FormView {
        model,
        key: None,
        values: &submitted.values,
        m2m: &submitted.m2m,
        errors: &submitted.errors,
        view_only: false,
        add: true,
    };
    let content = render_form(&console, &mut tx, &staff, &view).await?;
    tx.rollback().await?;
    let title = format!("Add {}", model.verbose);
    Ok(staff.page(
        &title,
        &crumbs(&console, model, &[(title.clone(), String::new())]),
        "change-form",
        &content,
    ))
}

/// GET and POST `/admin/<app>/<model>/<id>/delete/`.
pub async fn delete(
    State(console): State<Console>,
    staff: Staff,
    UrlPath((app, name, id)): UrlPath<(String, String, String)>,
    headers: HeaderMap,
    method: axum::http::Method,
    form: Form,
) -> AdminResult<Response> {
    let model = find_model(&console, &app, &name)?;
    let posted = method == axum::http::Method::POST;
    if posted {
        staff.check_csrf(&headers, &form)?;
    }
    if !staff.perms.delete(model) {
        return Err(AdminError::Forbidden);
    }
    let key = parse_key(model, &id)?;
    let plan = Plan::new(console.schema(), model)?;
    let mut tx = console.db().begin().await?;
    let (_, text) = query::load(&console, &mut tx, model, &key)
        .await?
        .ok_or(AdminError::NotFound)?;
    let keys = [key.clone()];
    let summary = plan.summary(&mut tx, &keys, &staff.perms).await?;
    if posted && form.get("post") == Some("yes") && !summary.blocked() {
        plan.execute(&mut tx, &keys).await?;
        record(
            &console,
            &mut tx,
            &staff,
            model,
            &key,
            &text,
            Event::Deleted,
        )
        .await?;
        tx.commit().await?;
        return Ok(staff.redirect(
            &model.url(),
            "success",
            &format!("The {} “{}” was deleted successfully.", model.verbose, text),
        ));
    }
    tx.rollback().await?;
    let content = if summary.blocked() {
        blocked_html(model, &summary)
    } else {
        format!(
            "<p>Are you sure you want to delete the {} “{}”? All of the following related items will be deleted:</p>{}\
             <form method=\"post\"><input type=\"hidden\" name=\"csrfmiddlewaretoken\" value=\"{}\">\
             <input type=\"hidden\" name=\"post\" value=\"yes\"><div class=\"submit-row\">\
             <input type=\"submit\" class=\"deletelink\" value=\"Yes, I’m sure\"> \
             <a href=\"{}\" class=\"button cancel-link\">No, take me back</a></div></form>",
            esc(model.verbose),
            esc(&text),
            summary.html(),
            esc(&staff.token()),
            esc(&model.object_url(&key))
        )
    };
    let crumbs = crumbs(
        &console,
        model,
        &[
            (text, model.object_url(&key)),
            ("Delete".to_string(), String::new()),
        ],
    );
    Ok(staff.page("Are you sure?", &crumbs, "delete-confirmation", &content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations_like_django() {
        assert_eq!(parse_duration("-1 00:00:00"), Some(-86_400));
        assert_eq!(parse_duration("1 day, 2:03:04"), Some(93_784));
        assert_eq!(parse_duration("01:00:00"), Some(3_600));
        assert_eq!(parse_duration("90"), Some(90));
        assert_eq!(parse_duration("abc"), None);
    }

    #[test]
    fn decimals_respect_their_places() {
        let field = Field::decimal("x", "X", 2);
        assert_eq!(
            parse(&field, Some("10.5"), KeyKind::Int),
            Ok(Some("10.5".into()))
        );
        assert!(parse(&field, Some("1.234"), KeyKind::Int).is_err());
        assert!(same(Kind::Decimal(2), Some("600.00"), Some("600")));
    }

    #[test]
    fn checkboxes_and_required_fields() {
        let int = KeyKind::Int;
        assert_eq!(
            parse(&Field::bool("b", "B"), None, int),
            Ok(Some("false".into()))
        );
        assert_eq!(
            parse(&Field::bool("b", "B"), Some("on"), int),
            Ok(Some("true".into()))
        );
        assert_eq!(
            parse(&Field::char("c", "C", 5), Some("  "), int),
            Err(REQUIRED.to_string())
        );
        assert_eq!(
            parse(&Field::char("c", "C", 5).null(), Some(""), int),
            Ok(None)
        );
        assert!(parse(&Field::char("c", "C", 2), Some("abc"), int).is_err());
    }

    #[test]
    fn foreign_keys_parse_as_their_target_key() {
        let fk = Field::fk("author", "author_id", "Author", "author");
        assert_eq!(parse(&fk, Some(" 7 "), KeyKind::Int), Ok(Some("7".into())));
        assert!(parse(&fk, Some("x"), KeyKind::Int).is_err());
        let id = "67E55044-10B1-426F-9247-BB680E5FE0C8";
        assert_eq!(
            parse(&fk, Some(id), KeyKind::Uuid),
            Ok(Some(id.to_lowercase()))
        );
        assert!(parse(&fk, Some("7"), KeyKind::Uuid).is_err());
        assert_eq!(
            parse(&fk, Some("en-gb"), KeyKind::Text),
            Ok(Some("en-gb".into()))
        );
    }
}
