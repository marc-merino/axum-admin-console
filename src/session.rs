//! Who may use the console: the staff check through the host's [`Auth`]
//! hook, CSRF on every POST, the flash message cookie, the console's error
//! pages and, with a [`PasswordSignIn`] hook, the sign-in and sign-out
//! pages.
//!
//! [`Auth`]: crate::Auth
//! [`PasswordSignIn`]: crate::PasswordSignIn

use std::sync::{Arc, Mutex};

use axum::{
    extract::{FromRequest, FromRequestParts, Request, State},
    http::{HeaderMap, StatusCode, header, request::Parts},
    response::{Html, IntoResponse, Redirect, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

use crate::{
    Console, SignIn,
    auth::{Identity, Perms, SignInOutcome},
    cookies::{self, SetCookie},
    csrf,
    html::{self, Chrome, Logout, esc},
};

const FLASH_COOKIE: &str = "adminmsg";
/// The form field every console form carries its CSRF token in.
pub const CSRF_FIELD: &str = "csrfmiddlewaretoken";

// ---------------------------------------------------------------------------
// Errors

/// A request the console refuses. Returned from a handler (the console's
/// or a host page's), it becomes the console's error page.
#[derive(Debug)]
pub enum AdminError {
    /// Not signed in as staff: to the sign-in page, then back to `next`.
    Login {
        next: String,
    },
    /// Signed in, not staff, and the console has no sign-in form to offer
    /// another account.
    NotStaff {
        name: String,
    },
    NotFound,
    Forbidden,
    Csrf,
    BadRequest(String),
    Internal(anyhow::Error),
}

impl From<sqlx::Error> for AdminError {
    fn from(error: sqlx::Error) -> Self {
        Self::Internal(error.into())
    }
}

impl From<anyhow::Error> for AdminError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

pub type AdminResult<T> = Result<T, AdminError>;

/// A server error's cause, in the error page's response extensions unless
/// the host reports errors itself ([`crate::ConsoleBuilder::report_errors`]).
#[derive(Clone, Debug)]
pub struct InternalError(pub Arc<anyhow::Error>);

/// An error on its way to the console's error-page middleware, which has
/// the console's settings to render it with.
#[derive(Clone)]
struct PendingError(Arc<Mutex<Option<AdminError>>>);

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::Login { .. } => StatusCode::SEE_OTHER,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::NotStaff { .. } | Self::Forbidden | Self::Csrf => StatusCode::FORBIDDEN,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let mut response = status.into_response();
        response
            .extensions_mut()
            .insert(PendingError(Arc::new(Mutex::new(Some(self)))));
        response
    }
}

/// The error a response stands for, if it is an unrendered error page.
pub(crate) fn take_error(response: &mut Response) -> Option<AdminError> {
    let pending = response.extensions_mut().remove::<PendingError>()?;
    let mut slot = pending.0.lock().unwrap_or_else(|e| e.into_inner());
    slot.take()
}

impl Console {
    fn bare_page(&self, status: StatusCode, title: &str, text: &str) -> Response {
        let chrome = self.chrome(title, &[], None, "", &[], "error-page");
        let body = format!("<p>{}</p><p><a href=\"/admin/\">Home</a></p>", esc(text));
        (status, Html(html::page(&chrome, &body))).into_response()
    }

    pub(crate) fn error_response(&self, error: AdminError) -> Response {
        match error {
            AdminError::Login { next } => match &self.inner().sign_in {
                SignIn::Password(_) => {
                    Redirect::to(&format!("/admin/login/?next={}", html::url_value(&next)))
                        .into_response()
                }
                SignIn::Url { url, param } => {
                    let separator = if url.contains('?') { '&' } else { '?' };
                    Redirect::to(&format!(
                        "{url}{separator}{}={}",
                        html::url_value(param),
                        html::url_value(&next)
                    ))
                    .into_response()
                }
                SignIn::None => self.bare_page(
                    StatusCode::FORBIDDEN,
                    "Permission denied",
                    "You are not signed in.",
                ),
            },
            AdminError::NotStaff { name } => self.bare_page(
                StatusCode::FORBIDDEN,
                "Permission denied",
                &format!(
                    "You are authenticated as {name}, but are not authorized to access this page."
                ),
            ),
            AdminError::NotFound => self.bare_page(
                StatusCode::NOT_FOUND,
                "Not found",
                "The requested page or object does not exist.",
            ),
            AdminError::Forbidden => self.bare_page(
                StatusCode::FORBIDDEN,
                "Permission denied",
                "Your account does not have permission to do this.",
            ),
            AdminError::Csrf => self.bare_page(
                StatusCode::FORBIDDEN,
                "Forbidden",
                "CSRF verification failed. Request aborted.",
            ),
            AdminError::BadRequest(text) => {
                self.bare_page(StatusCode::BAD_REQUEST, "Bad request", &text)
            }
            AdminError::Internal(error) => {
                let mut response = self.bare_page(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Server error",
                    "There was an error. It has been logged.",
                );
                (self.inner().report_error)(error, &mut response);
                response
            }
        }
    }

    /// The header and breadcrumbs of a page.
    pub(crate) fn chrome<'a>(
        &'a self,
        title: &'a str,
        crumbs: &'a [(String, String)],
        username: Option<&'a str>,
        csrf: &'a str,
        messages: &'a [(String, String)],
        body_class: &'a str,
    ) -> Chrome<'a> {
        let inner = self.inner();
        let logout = match (&inner.sign_in, &inner.sign_out_url) {
            (SignIn::Password(_), _) => Logout::Form,
            (_, Some(url)) => Logout::Link(url),
            _ => Logout::None,
        };
        Chrome {
            site_title: &inner.site_title,
            site_header: &inner.site_header,
            title,
            crumbs,
            username,
            csrf,
            logout,
            messages,
            body_class,
        }
    }

    /// The CSRF secret from the cookie, or a new one to set.
    fn csrf_secret(&self, headers: &HeaderMap) -> (String, bool) {
        cookies::get(headers, &self.inner().csrf_cookie)
            .filter(|token| csrf::valid_token(token))
            .and_then(|token| csrf::token_secret(&token))
            .map_or_else(|| (csrf::generate_token(), true), |secret| (secret, false))
    }

    /// `Set-Cookie` for the CSRF cookie, with Django's attributes.
    fn csrf_cookie(&self, token: &str) -> (header::HeaderName, axum::http::HeaderValue) {
        let inner = self.inner();
        let cookie = SetCookie {
            name: &inner.csrf_cookie,
            value: token,
            path: "/",
            http_only: false,
            secure: inner.secure_cookies,
            max_age: Some(csrf::COOKIE_MAX_AGE_SECONDS),
        };
        (header::SET_COOKIE, cookie.header())
    }

    fn flash_cookie(
        &self,
        level: &str,
        text: &str,
    ) -> (header::HeaderName, axum::http::HeaderValue) {
        let text = html::shorten(text, 600);
        let value = URL_SAFE_NO_PAD.encode(format!("{level}\t{text}"));
        let cookie = SetCookie {
            name: FLASH_COOKIE,
            value: &value,
            path: "/admin/",
            http_only: true,
            secure: self.inner().secure_cookies,
            max_age: None,
        };
        (header::SET_COOKIE, cookie.header())
    }

    fn flash_removal(&self) -> (header::HeaderName, axum::http::HeaderValue) {
        let cookie = SetCookie {
            name: FLASH_COOKIE,
            value: "",
            path: "/admin/",
            http_only: true,
            secure: self.inner().secure_cookies,
            max_age: Some(0),
        };
        (header::SET_COOKIE, cookie.header())
    }
}

// ---------------------------------------------------------------------------
// The signed-in staff member

/// A signed-in staff member with the request's CSRF secret and pending
/// flash messages: the extractor of every console page, and of a host's
/// own pages (see [`crate::Pages`]).
pub struct Staff {
    /// [`crate::StaffMember::id`].
    pub id: String,
    pub name: String,
    pub perms: Perms,
    /// `(level, text)` messages left by the previous request (`success`,
    /// `warning`, `error`), shown on this page.
    pub messages: Vec<(String, String)>,
    csrf_secret: String,
    new_csrf: bool,
    console: Console,
}

fn read_flash(headers: &HeaderMap) -> Vec<(String, String)> {
    cookies::get(headers, FLASH_COOKIE)
        .and_then(|value| URL_SAFE_NO_PAD.decode(value).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| {
            text.split_once('\t')
                .map(|(level, message)| vec![(level.to_string(), message.to_string())])
        })
        .unwrap_or_default()
}

/// Where the request came from, for the sign-in redirect.
fn next_of(parts: &Parts) -> String {
    parts
        .uri
        .path_and_query()
        .map_or("/admin/".to_string(), |pq| pq.as_str().to_string())
}

fn console_of(parts: &Parts) -> AdminResult<Console> {
    parts
        .extensions
        .get::<Console>()
        .cloned()
        .ok_or_else(|| AdminError::Internal(anyhow::anyhow!("route is not in the console router")))
}

impl<S: Send + Sync> FromRequestParts<S> for Staff {
    type Rejection = AdminError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> AdminResult<Self> {
        let console = console_of(parts)?;
        let member = match console.inner().auth.identify(parts).await {
            Identity::Staff(member) => member,
            Identity::Anonymous => {
                return Err(AdminError::Login {
                    next: next_of(parts),
                });
            }
            Identity::NotStaff { name } => {
                return Err(match console.inner().sign_in {
                    SignIn::Password(_) => AdminError::Login {
                        next: next_of(parts),
                    },
                    _ => AdminError::NotStaff { name },
                });
            }
            Identity::Unavailable(error) => return Err(AdminError::Internal(error)),
        };
        let (csrf_secret, new_csrf) = console.csrf_secret(&parts.headers);
        Ok(Self {
            id: member.id,
            name: member.name,
            perms: member.perms,
            messages: read_flash(&parts.headers),
            csrf_secret,
            new_csrf,
            console,
        })
    }
}

impl Staff {
    pub fn console(&self) -> &Console {
        &self.console
    }

    /// A masked token for a form, like `{% csrf_token %}`: put it in a
    /// hidden [`CSRF_FIELD`] input.
    pub fn token(&self) -> String {
        csrf::mask_token(&self.csrf_secret)
    }

    /// Cookies every console response carries: a new CSRF cookie and the
    /// removal of a flash message just shown.
    fn cookies(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if self.new_csrf {
            let (name, value) = self.console.csrf_cookie(&self.csrf_secret);
            headers.append(name, value);
        }
        if !self.messages.is_empty() {
            let (name, value) = self.console.flash_removal();
            headers.append(name, value);
        }
        headers
    }

    /// A full console page: `content` (HTML, every value escaped with
    /// [`esc`]) under the title, after breadcrumbs `(label, url)` that
    /// start after "Home"; the last one is the current page.
    pub fn page(
        &self,
        title: &str,
        crumbs: &[(String, String)],
        body_class: &str,
        content: &str,
    ) -> Response {
        self.page_with_status(StatusCode::OK, title, crumbs, body_class, content)
    }

    pub fn page_with_status(
        &self,
        status: StatusCode,
        title: &str,
        crumbs: &[(String, String)],
        body_class: &str,
        content: &str,
    ) -> Response {
        let token = self.token();
        let chrome = self.console.chrome(
            title,
            crumbs,
            Some(&self.name),
            &token,
            &self.messages,
            body_class,
        );
        (status, self.cookies(), Html(html::page(&chrome, content))).into_response()
    }

    /// A redirect after a POST, with a message for the next page; `level`
    /// is `success`, `warning` or `error`.
    pub fn redirect(&self, to: &str, level: &str, text: &str) -> Response {
        let mut headers = HeaderMap::new();
        if self.new_csrf {
            let (name, value) = self.console.csrf_cookie(&self.csrf_secret);
            headers.append(name, value);
        }
        let (name, value) = self.console.flash_cookie(level, text);
        headers.append(name, value);
        (headers, Redirect::to(to)).into_response()
    }

    /// Django's CSRF check for a POST: the form's `csrfmiddlewaretoken` (or
    /// the `X-CSRFToken` header) must carry the cookie's secret, and an
    /// `Origin` header, when sent, must name this host.
    pub fn check_csrf(&self, headers: &HeaderMap, form: &Form) -> AdminResult<()> {
        check_csrf(&self.console, headers, form)
    }
}

pub(crate) fn check_csrf(console: &Console, headers: &HeaderMap, form: &Form) -> AdminResult<()> {
    let cookie = cookies::get(headers, &console.inner().csrf_cookie).ok_or(AdminError::Csrf)?;
    let submitted = form
        .get(CSRF_FIELD)
        .map(str::to_string)
        .or_else(|| {
            headers
                .get("x-csrftoken")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        })
        .ok_or(AdminError::Csrf)?;
    if !csrf::tokens_match(&cookie, &submitted) || !same_origin(headers) {
        return Err(AdminError::Csrf);
    }
    Ok(())
}

/// An `Origin` header, when present, must be this host.
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let authority = origin
        .split_once("://")
        .map_or(origin, |(_, rest)| rest)
        .trim_end_matches('/');
    authority.eq_ignore_ascii_case(host)
}

// ---------------------------------------------------------------------------
// Form bodies

/// An `application/x-www-form-urlencoded` body, repeated keys kept.
#[derive(Debug, Default)]
pub struct Form(pub Vec<(String, String)>);

impl Form {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    pub fn all(&self, key: &str) -> impl Iterator<Item = &str> {
        self.0
            .iter()
            .filter(move |(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    pub fn has(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| k == key)
    }
}

impl<S: Send + Sync> FromRequest<S> for Form {
    type Rejection = AdminError;

    async fn from_request(request: Request, state: &S) -> AdminResult<Self> {
        let bytes = axum::body::Bytes::from_request(request, state)
            .await
            .map_err(|e| AdminError::BadRequest(e.body_text()))?;
        serde_urlencoded::from_bytes(&bytes)
            .map(Form)
            .map_err(|_| AdminError::BadRequest("The form could not be read.".into()))
    }
}

// ---------------------------------------------------------------------------
// Sign-in and sign-out

/// Only console paths are followed after signing in.
fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(next)
            if next.starts_with("/admin/")
                && !next.contains("//")
                && !next.contains('\\')
                && !next.starts_with("/admin/login")
                && !next.starts_with("/admin/logout") =>
        {
            next.to_string()
        }
        _ => "/admin/".to_string(),
    }
}

fn next_from_query(query: Option<&str>) -> Option<String> {
    let pairs: Vec<(String, String)> = serde_urlencoded::from_str(query.unwrap_or("")).ok()?;
    pairs.into_iter().find(|(k, _)| k == "next").map(|(_, v)| v)
}

struct LoginPage<'a> {
    secret: &'a str,
    status: StatusCode,
    next: &'a str,
    username: &'a str,
    error: Option<&'a str>,
    notice: Option<&'a str>,
}

fn login_page(console: &Console, cookies: HeaderMap, page: LoginPage<'_>) -> Response {
    let mut body = String::new();
    if let Some(notice) = page.notice {
        body.push_str(&format!("<p class=\"errornote\">{}</p>", esc(notice)));
    }
    if let Some(error) = page.error {
        body.push_str(&format!("<p class=\"errornote\">{}</p>", esc(error)));
    }
    body.push_str(&format!(
        "<form method=\"post\" action=\"/admin/login/\" id=\"login-form\">\
         <input type=\"hidden\" name=\"{CSRF_FIELD}\" value=\"{}\">\
         <input type=\"hidden\" name=\"next\" value=\"{}\">\
         <div class=\"form-row\"><label for=\"id_username\" class=\"required\">Username:</label>\
         <input type=\"text\" name=\"username\" id=\"id_username\" value=\"{}\" \
          autocomplete=\"username\" autofocus required maxlength=\"150\"></div>\
         <div class=\"form-row\"><label for=\"id_password\" class=\"required\">Password:</label>\
         <input type=\"password\" name=\"password\" id=\"id_password\" \
          autocomplete=\"current-password\" required></div>\
         <div class=\"submit-row\"><input type=\"submit\" class=\"default\" value=\"Log in\"></div>\
         </form>",
        esc(&csrf::mask_token(page.secret)),
        esc(page.next),
        esc(page.username),
    ));
    let chrome = console.chrome("Log in", &[], None, "", &[], "login");
    (page.status, cookies, Html(html::page(&chrome, &body))).into_response()
}

/// GET `/admin/login/`.
pub async fn login_form(State(console): State<Console>, parts: Parts) -> AdminResult<Response> {
    let next = safe_next(next_from_query(parts.uri.query()).as_deref());
    let notice = match console.inner().auth.identify(&parts).await {
        Identity::Staff(_) => return Ok(Redirect::to(&next).into_response()),
        Identity::Unavailable(error) => return Err(AdminError::Internal(error)),
        // Django's note for a signed-in user who is not staff.
        Identity::NotStaff { name } => Some(format!(
            "You are authenticated as {name}, but are not authorized to access this page. \
             Would you like to login to a different account?"
        )),
        Identity::Anonymous => None,
    };
    let (secret, new) = console.csrf_secret(&parts.headers);
    let mut cookies = HeaderMap::new();
    if new {
        let (name, value) = console.csrf_cookie(&secret);
        cookies.append(name, value);
    }
    Ok(login_page(
        &console,
        cookies,
        LoginPage {
            secret: &secret,
            status: StatusCode::OK,
            next: &next,
            username: "",
            error: None,
            notice: notice.as_deref(),
        },
    ))
}

const LOGIN_ERROR: &str = "Please enter the correct username and password for a staff \
     account. Note that both fields may be case-sensitive.";

/// POST `/admin/login/`: the host checks the credentials.
pub async fn login(
    State(console): State<Console>,
    parts: Parts,
    form: Form,
) -> AdminResult<Response> {
    let SignIn::Password(sign_in) = &console.inner().sign_in else {
        return Err(AdminError::NotFound);
    };
    check_csrf(&console, &parts.headers, &form)?;
    let (secret, _) = console.csrf_secret(&parts.headers);
    let next = safe_next(form.get("next"));
    let username = form.get("username").unwrap_or("").to_string();
    let password = form.get("password").unwrap_or("").to_string();
    let refuse = |status, error: &str| {
        login_page(
            &console,
            HeaderMap::new(),
            LoginPage {
                secret: &secret,
                status,
                next: &next,
                username: &username,
                error: Some(error),
                notice: None,
            },
        )
    };
    if username.is_empty() || password.is_empty() {
        return Ok(refuse(StatusCode::OK, LOGIN_ERROR));
    }
    match sign_in.sign_in(&parts, &username, &password).await {
        SignInOutcome::SignedIn(mut headers) => {
            let (name, value) = console.csrf_cookie(&csrf::generate_token());
            headers.append(name, value);
            Ok((headers, Redirect::to(&next)).into_response())
        }
        SignInOutcome::Refused => Ok(refuse(StatusCode::OK, LOGIN_ERROR)),
        SignInOutcome::Throttled => Ok(refuse(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many failed sign-in attempts. Try again later.",
        )),
        SignInOutcome::Unavailable(error) => Err(AdminError::Internal(error)),
    }
}

/// POST `/admin/logout/` (GET only offers the button, as in Django 5).
pub async fn logout(
    State(console): State<Console>,
    parts: Parts,
    form: Form,
) -> AdminResult<Response> {
    let SignIn::Password(sign_in) = &console.inner().sign_in else {
        return Err(AdminError::NotFound);
    };
    check_csrf(&console, &parts.headers, &form)?;
    let headers = sign_in.sign_out(&parts).await?;
    let chrome = console.chrome("Logged out", &[], None, "", &[], "logout");
    let body = "<p>Thanks for spending some quality time with the web site today.</p>\
                <p><a href=\"/admin/login/\">Log in again</a></p>";
    Ok((headers, Html(html::page(&chrome, body))).into_response())
}

pub async fn logout_get() -> Response {
    Redirect::to("/admin/").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_console_paths_are_followed() {
        assert_eq!(safe_next(Some("/admin/shop/book/")), "/admin/shop/book/");
        assert_eq!(safe_next(Some("https://evil.example/")), "/admin/");
        assert_eq!(safe_next(Some("/admin//evil.example")), "/admin/");
        assert_eq!(safe_next(Some("//evil.example")), "/admin/");
        assert_eq!(safe_next(None), "/admin/");
    }
}
