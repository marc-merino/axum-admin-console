//! The site index, app index and history pages, and helpers the other
//! pages share.

use std::fmt::Write;

use axum::{
    extract::{Path as UrlPath, State},
    http::header,
    response::{IntoResponse, Redirect, Response},
};

use crate::{
    Console,
    history::EntryKind,
    html::{django_datetime, esc},
    key::Key,
    query,
    registry::{App, Model},
    session::{AdminError, AdminResult, Staff},
};

/// How many entries "Recent actions" shows.
const RECENT_LIMIT: i64 = 10;

pub fn find_model(console: &Console, app: &str, name: &str) -> AdminResult<&'static Model> {
    console.model(app, name).ok_or(AdminError::NotFound)
}

/// The key in a URL; a key of the wrong type is a page that does not exist.
pub fn parse_key(model: &Model, raw: &str) -> AdminResult<Key> {
    model.key.parse(raw).ok_or(AdminError::NotFound)
}

/// Breadcrumbs from the app down to the model, then `extra`.
pub fn crumbs(
    console: &Console,
    model: &Model,
    extra: &[(String, String)],
) -> Vec<(String, String)> {
    let app_name = console.app(model.app).map_or(model.app, |app| app.name);
    let mut out = vec![
        (app_name.to_string(), format!("/admin/{}/", model.app)),
        (model.title_plural(), model.url()),
    ];
    out.extend(extra.iter().cloned());
    out
}

/// The apps and models this user may see, sorted like Django's index.
fn visible(
    console: &Console,
    staff: &Staff,
    only: Option<&str>,
) -> Vec<(&'static App, Vec<&'static Model>)> {
    let mut list: Vec<(&'static App, Vec<&'static Model>)> = console
        .apps()
        .iter()
        .copied()
        .filter(|app| only.is_none_or(|label| label == app.label))
        .filter(|app| staff.perms.module(app.label))
        .map(|app| {
            let mut models: Vec<&'static Model> = app
                .models
                .iter()
                .copied()
                .filter(|model| staff.perms.view(model) || staff.perms.add(model))
                .collect();
            models.sort_by_key(|model| model.title_plural().to_lowercase());
            (app, models)
        })
        .filter(|(_, models)| !models.is_empty())
        .collect();
    list.sort_by_key(|(app, _)| app.name.to_lowercase());
    list
}

fn app_tables(staff: &Staff, list: &[(&'static App, Vec<&'static Model>)]) -> String {
    let mut out = String::new();
    for (app, models) in list {
        let _ = write!(
            out,
            "<div class=\"app-{label} module\"><table><caption><a href=\"/admin/{label}/\" class=\"section\">{name}</a></caption>",
            label = esc(app.label),
            name = esc(app.name)
        );
        for model in models {
            let name = if staff.perms.view(model) {
                format!(
                    "<a href=\"{}\">{}</a>",
                    esc(&model.url()),
                    esc(&model.title_plural())
                )
            } else {
                esc(&model.title_plural())
            };
            let add = if staff.perms.add(model) {
                format!(
                    "<a href=\"{}add/\" class=\"addlink\">Add</a>",
                    esc(&model.url())
                )
            } else {
                String::new()
            };
            let change = if staff.perms.change(model) {
                format!(
                    "<a href=\"{}\" class=\"changelink\">Change</a>",
                    esc(&model.url())
                )
            } else if staff.perms.view(model) {
                format!(
                    "<a href=\"{}\" class=\"viewlink\">View</a>",
                    esc(&model.url())
                )
            } else {
                String::new()
            };
            let _ = write!(
                out,
                "<tr><th scope=\"row\">{name}</th><td>{add}</td><td>{change}</td></tr>"
            );
        }
        out.push_str("</table></div>");
    }
    if list.is_empty() {
        out.push_str("<p>You don’t have permission to view or edit anything.</p>");
    }
    out
}

/// The host's links the index shows under the app list.
fn index_links(console: &Console, staff: &Staff) -> String {
    let links: Vec<_> = console
        .inner()
        .links
        .iter()
        .filter(|link| (link.visible)(&staff.perms))
        .collect();
    if links.is_empty() {
        return String::new();
    }
    let mut out =
        String::from("<div class=\"module\" id=\"tools-module\"><table><caption>Tools</caption>");
    for link in links {
        let _ = write!(
            out,
            "<tr><th scope=\"row\"><a href=\"{}\">{}</a></th></tr>",
            esc(&link.url),
            esc(&link.label)
        );
    }
    out.push_str("</table></div>");
    out
}

/// GET `/admin/`.
pub async fn index(State(console): State<Console>, staff: Staff) -> AdminResult<Response> {
    let mut content = format!(
        "<div id=\"content-main\">{}{}</div>",
        app_tables(&staff, &visible(&console, &staff, None)),
        index_links(&console, &staff)
    );
    if let Some(history) = &console.inner().history {
        let mut db = console.db().acquire().await?;
        let entries = history.recent(&mut db, &staff.id, RECENT_LIMIT).await?;
        content.push_str(
            "<div id=\"content-related\"><div class=\"module\" id=\"recent-actions-module\"><h2>Recent actions</h2><h3>My actions</h3>",
        );
        if entries.is_empty() {
            content.push_str("<p>None available</p>");
        } else {
            content.push_str("<ul class=\"actionlist\">");
            for entry in &entries {
                let class = match entry.kind {
                    EntryKind::Added => "addlink",
                    EntryKind::Changed => "changelink",
                    EntryKind::Deleted => "deletelink",
                };
                let model = match (&entry.app, &entry.model) {
                    (Some(app), Some(name)) => console.model(app, name),
                    _ => None,
                };
                let key = model
                    .zip(entry.key.as_deref())
                    .and_then(|(m, k)| m.key.parse(k));
                let link = match (model, key) {
                    (Some(model), Some(key))
                        if entry.kind != EntryKind::Deleted && staff.perms.view(model) =>
                    {
                        format!(
                            "<a href=\"{}\">{}</a>",
                            esc(&model.object_url(key)),
                            esc(&entry.repr)
                        )
                    }
                    _ => esc(&entry.repr),
                };
                let kind = model.map_or_else(
                    || {
                        entry
                            .model
                            .clone()
                            .unwrap_or_else(|| "Unknown content".into())
                    },
                    |m| m.title(),
                );
                let _ = write!(
                    content,
                    "<li class=\"{class}\">{link}<br><span class=\"mini quiet\">{}</span></li>",
                    esc(&kind)
                );
            }
            content.push_str("</ul>");
        }
        content.push_str("</div></div>");
    }
    Ok(staff.page("Site administration", &[], "dashboard", &content))
}

/// GET `/admin/<app>/`.
pub async fn app_index(
    State(console): State<Console>,
    staff: Staff,
    UrlPath(label): UrlPath<String>,
) -> AdminResult<Response> {
    let app = console.app(&label).ok_or(AdminError::NotFound)?;
    let list = visible(&console, &staff, Some(app.label));
    if list.is_empty() {
        return Err(AdminError::Forbidden);
    }
    let title = format!("{} administration", app.name);
    let content = format!(
        "<div id=\"content-main\">{}</div>",
        app_tables(&staff, &list)
    );
    Ok(staff.page(
        &title,
        &[(app.name.to_string(), String::new())],
        "app-list",
        &content,
    ))
}

/// GET `/admin/<app>/<model>/<id>/`: Django redirects to the change page.
pub async fn object_redirect(
    State(console): State<Console>,
    UrlPath((app, name, id)): UrlPath<(String, String, String)>,
) -> AdminResult<Response> {
    let model = find_model(&console, &app, &name)?;
    Ok(Redirect::to(&model.object_url(parse_key(model, &id)?)).into_response())
}

/// The most history entries one page shows.
const HISTORY_LIMIT: i64 = 500;

/// GET `/admin/<app>/<model>/<id>/history/`: only with a history hook.
pub async fn history(
    State(console): State<Console>,
    staff: Staff,
    UrlPath((app, name, id)): UrlPath<(String, String, String)>,
) -> AdminResult<Response> {
    let history = console
        .inner()
        .history
        .as_deref()
        .ok_or(AdminError::NotFound)?;
    let model = find_model(&console, &app, &name)?;
    if !staff.perms.view(model) {
        return Err(AdminError::Forbidden);
    }
    let key = parse_key(model, &id)?;
    let mut db = console.db().acquire().await?;
    let (_, text) = query::load(&console, &mut db, model, &key)
        .await?
        .ok_or(AdminError::NotFound)?;
    let entries = history.object(&mut db, model, &key, HISTORY_LIMIT).await?;
    let mut content = String::from("<div id=\"content-main\"><div class=\"module\">");
    if entries.is_empty() {
        content.push_str("<p>This object doesn’t have a change history. It probably wasn’t added via this admin site.</p>");
    } else {
        content.push_str("<table id=\"change-history\"><thead><tr><th scope=\"col\">Date/time</th><th scope=\"col\">User</th><th scope=\"col\">Action</th></tr></thead><tbody>");
        for entry in &entries {
            let at = entry.at.with_timezone(&console.time_zone()).naive_local();
            let action = match entry.kind {
                EntryKind::Added => "Added.",
                EntryKind::Deleted => "Deleted.",
                EntryKind::Changed => entry.message.as_str(),
            };
            let _ = write!(
                content,
                "<tr><th scope=\"row\">{}</th><td>{}</td><td>{}</td></tr>",
                esc(&django_datetime(at)),
                esc(&entry.staff_name),
                esc(action)
            );
        }
        content.push_str("</tbody></table>");
    }
    content.push_str("</div></div>");
    let title = format!("Change history: {text}");
    let crumbs = crumbs(
        &console,
        model,
        &[
            (text.clone(), model.object_url(&key)),
            ("History".to_string(), String::new()),
        ],
    );
    Ok(staff.page(&title, &crumbs, "change-history", &content))
}

/// The console's stylesheet.
pub async fn stylesheet() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        include_str!("admin.css"),
    )
        .into_response()
}

/// The console's script (the list's "select all" box).
pub async fn script() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        include_str!("admin.js"),
    )
        .into_response()
}
