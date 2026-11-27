//! HTML building: one escaping helper, the page layout, and value
//! formatting in the style of Django's admin. Every dynamic value goes
//! through [`esc`].

use std::fmt::Write;

use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike};

use crate::registry::Kind;

/// Escapes text for HTML content and attribute values.
pub fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Percent-encodes a query-string value.
pub fn url_value(text: &str) -> String {
    serde_urlencoded::to_string([("", text)])
        .map(|encoded| encoded.trim_start_matches('=').to_string())
        .unwrap_or_default()
}

/// How the header offers signing out.
pub enum Logout<'a> {
    /// A POST to the console's `/admin/logout/`.
    Form,
    /// A link to the host's page.
    Link(&'a str),
    None,
}

/// What the header and breadcrumbs need.
pub struct Chrome<'a> {
    pub site_title: &'a str,
    pub site_header: &'a str,
    pub title: &'a str,
    /// `(label, url)`; the last one is the current page, without a link.
    pub crumbs: &'a [(String, String)],
    pub username: Option<&'a str>,
    /// Masked CSRF token for the log-out form.
    pub csrf: &'a str,
    pub logout: Logout<'a>,
    pub messages: &'a [(String, String)],
    pub body_class: &'a str,
}

pub fn page(chrome: &Chrome, content: &str) -> String {
    let mut out = String::with_capacity(content.len() + 2048);
    let _ = write!(
        out,
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"robots\" content=\"noindex, nofollow\">\
         <title>{} | {}</title>\
         <link rel=\"stylesheet\" href=\"/admin/static/admin.css\"></head>\
         <body class=\"{}\"><header id=\"header\"><div id=\"branding\">\
         <a href=\"/admin/\">{}</a></div>",
        esc(chrome.title),
        esc(chrome.site_title),
        esc(chrome.body_class),
        esc(chrome.site_header)
    );
    if let Some(username) = chrome.username {
        let _ = write!(
            out,
            "<div id=\"user-tools\">Welcome, <strong>{}</strong>.",
            esc(username)
        );
        match chrome.logout {
            Logout::Form => {
                let _ = write!(
                    out,
                    " <form method=\"post\" action=\"/admin/logout/\">\
                     <input type=\"hidden\" name=\"csrfmiddlewaretoken\" value=\"{}\">\
                     <button type=\"submit\">Log out</button></form>",
                    esc(chrome.csrf)
                );
            }
            Logout::Link(url) => {
                let _ = write!(out, " <a href=\"{}\">Log out</a>", esc(url));
            }
            Logout::None => {}
        }
        out.push_str("</div>");
    }
    out.push_str("</header>");
    if !chrome.crumbs.is_empty() {
        out.push_str("<nav class=\"breadcrumbs\"><a href=\"/admin/\">Home</a>");
        let last = chrome.crumbs.len() - 1;
        for (index, (label, url)) in chrome.crumbs.iter().enumerate() {
            if index == last || url.is_empty() {
                let _ = write!(out, " &rsaquo; {}", esc(label));
            } else {
                let _ = write!(out, " &rsaquo; <a href=\"{}\">{}</a>", esc(url), esc(label));
            }
        }
        out.push_str("</nav>");
    }
    if !chrome.messages.is_empty() {
        out.push_str("<ul class=\"messagelist\">");
        for (level, text) in chrome.messages {
            let _ = write!(out, "<li class=\"{}\">{}</li>", esc(level), esc(text));
        }
        out.push_str("</ul>");
    }
    let _ = write!(
        out,
        "<main id=\"content\"><h1>{}</h1>{content}</main></body></html>",
        esc(chrome.title)
    );
    out
}

pub fn boolean_icon(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "<span class=\"icon-yes\" title=\"True\">&#10004;</span>",
        Some(false) => "<span class=\"icon-no\" title=\"False\">&#10008;</span>",
        None => "<span class=\"icon-unknown\" title=\"None\">?</span>",
    }
}

/// Django's empty value display.
pub const EMPTY: &str = "-";

/// A stored value (as text) shown as HTML, Django style. Datetimes come
/// already in the console's time zone (see [`crate::Console::as_text`]). `list` shortens
/// long text for result tables.
pub fn display(kind: Kind, value: Option<&str>, list: bool) -> String {
    let Some(value) = value else {
        return match kind {
            Kind::Bool => boolean_icon(None).to_string(),
            _ => EMPTY.to_string(),
        };
    };
    match kind {
        Kind::Bool => boolean_icon(Some(value == "true")).to_string(),
        Kind::Choice(choices) => esc(choices
            .iter()
            .find(|(stored, _)| *stored == value)
            .map_or(value, |(_, label)| label)),
        Kind::DateTime => NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S")
            .map(|at| esc(&django_datetime(at)))
            .unwrap_or_else(|_| esc(value)),
        Kind::Date => NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .map(|day| esc(&django_date(day)))
            .unwrap_or_else(|_| esc(value)),
        Kind::Duration => value
            .parse::<i64>()
            .map(|seconds| esc(&timedelta_str(seconds)))
            .unwrap_or_else(|_| esc(value)),
        Kind::Password => esc(&password_summary(value)),
        Kind::Json | Kind::Text | Kind::Char if list => esc(&shorten(value, 120)),
        Kind::Text => format!("<div class=\"readonly-text\">{}</div>", esc(value)),
        Kind::Json => format!("<pre>{}</pre>", esc(&pretty_json(value))),
        _ => esc(value),
    }
}

pub fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max).collect();
        format!("{cut}…")
    }
}

pub fn pretty_json(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| text.to_string())
}

const MONTHS: [&str; 12] = [
    "Jan.", "Feb.", "March", "April", "May", "June", "July", "Aug.", "Sept.", "Oct.", "Nov.",
    "Dec.",
];

/// Django's `DATE_FORMAT` (`N j, Y`).
pub fn django_date(day: NaiveDate) -> String {
    format!(
        "{} {}, {}",
        MONTHS[day.month0() as usize],
        day.day(),
        day.year()
    )
}

/// Django's `DATETIME_FORMAT` (`N j, Y, P`): "Nov. 25, 2026, 3:04 p.m.".
pub fn django_datetime(at: NaiveDateTime) -> String {
    let time = match (at.hour(), at.minute()) {
        (0, 0) => "midnight".to_string(),
        (12, 0) => "noon".to_string(),
        (hour, minute) => {
            let suffix = if hour < 12 { "a.m." } else { "p.m." };
            let hour12 = match hour % 12 {
                0 => 12,
                other => other,
            };
            if minute == 0 {
                format!("{hour12} {suffix}")
            } else {
                format!("{hour12}:{minute:02} {suffix}")
            }
        }
    };
    format!("{}, {time}", django_date(at.date()))
}

/// Python's `(days, seconds)` split of a duration in seconds.
pub fn split_duration(total: i64) -> (i64, i64) {
    (total.div_euclid(86_400), total.rem_euclid(86_400))
}

/// `str(timedelta)`: "1 day, 2:03:04", "-1 day, 0:00:00".
pub fn timedelta_str(total: i64) -> String {
    let (days, seconds) = split_duration(total);
    let clock = format!(
        "{}:{:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    );
    match days {
        0 => clock,
        1 | -1 => format!("{days} day, {clock}"),
        _ => format!("{days} days, {clock}"),
    }
}

/// Django's `duration_string`, the form widget's text: "-1 00:00:00".
pub fn duration_string(total: i64) -> String {
    let (days, seconds) = split_duration(total);
    let clock = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    );
    if days == 0 {
        clock
    } else {
        format!("{days} {clock}")
    }
}

/// `mask_hash`: the first six characters, the rest as asterisks.
fn mask(text: &str) -> String {
    let shown: String = text.chars().take(6).collect();
    let hidden = text.chars().count().saturating_sub(6);
    format!("{shown}{}", "*".repeat(hidden))
}

/// What Django's `ReadOnlyPasswordHashWidget` shows for a stored hash.
pub fn password_summary(encoded: &str) -> String {
    if encoded.is_empty() || encoded.starts_with('!') {
        return "No password set.".to_string();
    }
    let parts: Vec<&str> = encoded.splitn(4, '$').collect();
    match parts.as_slice() {
        [algorithm, iterations, salt, hash] if algorithm.starts_with("pbkdf2") => format!(
            "algorithm: {algorithm} iterations: {iterations} salt: {} hash: {}",
            mask(salt),
            mask(hash)
        ),
        _ => "Invalid password format or unknown hashing algorithm.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_markup_and_quotes() {
        assert_eq!(
            esc("<a href=\"x\">'&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&#x27;&amp;&#x27;&lt;/a&gt;"
        );
    }

    #[test]
    fn formats_like_django() {
        let at = NaiveDateTime::parse_from_str("2026-11-25T15:04:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        assert_eq!(django_datetime(at), "Nov. 25, 2026, 3:04 p.m.");
        let at = NaiveDateTime::parse_from_str("2026-03-01T00:00:00", "%Y-%m-%dT%H:%M:%S").unwrap();
        assert_eq!(django_datetime(at), "March 1, 2026, midnight");
        assert_eq!(timedelta_str(-86_400), "-1 day, 0:00:00");
        assert_eq!(timedelta_str(93_784), "1 day, 2:03:04");
        assert_eq!(duration_string(-86_400), "-1 00:00:00");
        assert_eq!(duration_string(3_600), "01:00:00");
    }

    #[test]
    fn summarises_password_hashes_without_revealing_them() {
        let summary = password_summary("pbkdf2_sha256$1000000$abcdefghij$0123456789abcdef");
        assert_eq!(
            summary,
            "algorithm: pbkdf2_sha256 iterations: 1000000 salt: abcdef**** hash: 012345**********"
        );
        assert_eq!(password_summary("!unusable"), "No password set.");
    }
}
