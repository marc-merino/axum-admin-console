//! A Django-style admin console for axum and sqlx services on Postgres:
//! server-rendered list, add, change and delete pages over the host's own
//! tables, declared as `static` [`registry::Model`]s, for staff only, with
//! Django's permission model.
//!
//! The host describes its tables once in a `static` [`Schema`] (its apps,
//! delete rules and table keys), builds one [`Console`] from its pool, the
//! schema and its hooks ([`Auth`], and optionally [`PasswordSignIn`],
//! [`History`] and [`Passwords`]) and merges [`Console::router`] into its
//! own router. See `README.md` for a complete example.

mod auth;
mod changelist;
mod cookies;
pub mod csrf;
pub mod delete;
mod form;
pub mod history;
pub mod html;
mod key;
mod pages;
pub mod query;
pub mod registry;
pub mod relations;
mod session;

use std::sync::Arc;

use axum::{
    Extension, Router,
    extract::State,
    middleware::map_response_with_state,
    response::Response,
    routing::{MethodRouter, get},
};
use sqlx::PgPool;

pub use auth::{Auth, Identity, PasswordSignIn, Passwords, Perms, SignInOutcome, StaffMember};
pub use chrono_tz::Tz;
pub use form::validate_password;
pub use history::{Change, Entry, EntryKind, Event, History};
pub use key::{Key, KeyKind};
pub use query::Values;
pub use relations::{AlsoDelete, OnDelete, Relation};
pub use session::{AdminError, AdminResult, CSRF_FIELD, Form, InternalError, Staff};

use registry::{App, Model};

/// Where the console is mounted. Every URL it writes starts here.
pub const BASE: &str = "/admin";

/// How a server error reaches the host's logs: called with the error and
/// the error page about to be sent.
pub type ErrorReporter = Arc<dyn Fn(anyhow::Error, &mut Response) + Send + Sync>;

/// The console: cheap to clone, shared by every request.
#[derive(Clone)]
pub struct Console(Arc<Inner>);

pub(crate) enum SignIn {
    /// The console's own form, checked by the host.
    Password(Box<dyn PasswordSignIn>),
    /// The host's page: `url`, with `param` (when given) set to the console
    /// path to return to.
    Url {
        url: String,
        param: Option<String>,
    },
    None,
}

/// A link in the "Tools" box of the index page.
pub(crate) struct IndexLink {
    pub label: String,
    pub url: String,
    pub visible: fn(&Perms) -> bool,
}

pub(crate) struct Inner {
    pub db: PgPool,
    pub schema: &'static Schema,
    pub auth: Box<dyn Auth>,
    pub sign_in: SignIn,
    pub sign_out_url: Option<String>,
    pub history: Option<Box<dyn History>>,
    pub passwords: Option<Box<dyn Passwords>>,
    pub links: Vec<IndexLink>,
    pub site_title: String,
    pub site_header: String,
    pub time_zone: Tz,
    pub secure_cookies: bool,
    pub csrf_cookie: String,
    pub report_error: ErrorReporter,
}

/// The key column and type of a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableKey {
    pub column: &'static str,
    pub kind: KeyKind,
}

/// The tables the console works on, as `static` data: the registered
/// apps, the delete rules, and the key of each table foreign keys point to.
pub struct Schema {
    pub apps: &'static [&'static App],
    pub relations: &'static [Relation],
    pub also_delete: &'static [AlsoDelete],
    /// `(table, key column, key type)` of tables foreign keys point to
    /// whose key no registered model gives.
    pub tables: &'static [(&'static str, &'static str, KeyKind)],
    /// The key of every other table.
    pub default_key: (&'static str, KeyKind),
}

impl Schema {
    /// `apps`, no delete rules (deleting a row deletes just that row), and
    /// `id` integer keys for tables no model or [`Schema::tables`] entry
    /// describes.
    pub const fn new(apps: &'static [&'static App]) -> Self {
        Self {
            apps,
            relations: &[],
            also_delete: &[],
            tables: &[],
            default_key: ("id", KeyKind::Int),
        }
    }

    /// The foreign keys deletes follow, and the extra deletions.
    pub const fn delete_rules(
        mut self,
        relations: &'static [Relation],
        also_delete: &'static [AlsoDelete],
    ) -> Self {
        self.relations = relations;
        self.also_delete = also_delete;
        self
    }

    /// `(table, key column, key type)` of tables foreign keys point to that
    /// no registered model is stored in (or whose model's key is not the
    /// table's).
    pub const fn tables(
        mut self,
        tables: &'static [(&'static str, &'static str, KeyKind)],
    ) -> Self {
        self.tables = tables;
        self
    }

    /// The key of any other table (default: `id`, an integer).
    pub const fn default_key(mut self, column: &'static str, kind: KeyKind) -> Self {
        self.default_key = (column, kind);
        self
    }

    /// Every registered model.
    pub fn models(&self) -> impl Iterator<Item = &'static Model> + use<> {
        self.apps.iter().flat_map(|app| app.models.iter().copied())
    }

    /// The app with this label, when it has models.
    pub fn app(&self, label: &str) -> Option<&'static App> {
        self.apps
            .iter()
            .copied()
            .find(|app| app.label == label && !app.models.is_empty())
    }

    /// The model `name` of app `app`.
    pub fn model(&self, app: &str, name: &str) -> Option<&'static Model> {
        self.models()
            .find(|model| model.app == app && model.name == name)
    }

    /// The key column and type of `table`: from [`Schema::tables`], else
    /// the first registered model stored in it, else the default.
    pub fn table_key(&self, table: &str) -> TableKey {
        if let Some((_, column, kind)) = self.tables.iter().find(|(t, _, _)| *t == table) {
            return TableKey {
                column,
                kind: *kind,
            };
        }
        if let Some(model) = self.models().find(|model| model.table == table) {
            return TableKey {
                column: model.pk,
                kind: model.key,
            };
        }
        let (column, kind) = self.default_key;
        TableKey { column, kind }
    }

    /// The registered model whose rows are the rows of `table` by its key
    /// column: the model a foreign key to `table` links to.
    pub fn model_for_table(&self, table: &str) -> Option<&'static Model> {
        let key = self.table_key(table);
        self.models()
            .find(|model| model.table == table && model.pk == key.column)
    }

    /// Builds every model's delete plan: fails when the delete rules make a
    /// cycle of cascades or a cascade over too many tables.
    pub fn check(&'static self) -> anyhow::Result<()> {
        for model in self.models() {
            delete::Plan::new(self, model)
                .map_err(|error| anyhow::anyhow!("{}: {error}", model.table))?;
        }
        Ok(())
    }
}

/// Collects what [`Console::builder`] did not need up front.
pub struct ConsoleBuilder {
    inner: Inner,
}

impl Console {
    /// A console over `db` showing `schema`'s apps, with `auth` telling who
    /// is asking. Everything else has a default: no sign-in page (an
    /// unauthenticated request gets a 403 page), no history, UTC, insecure
    /// cookies, the CSRF cookie `csrftoken`.
    pub fn builder(db: PgPool, schema: &'static Schema, auth: impl Auth) -> ConsoleBuilder {
        ConsoleBuilder {
            inner: Inner {
                db,
                schema,
                auth: Box::new(auth),
                sign_in: SignIn::None,
                sign_out_url: None,
                history: None,
                passwords: None,
                links: Vec::new(),
                site_title: "Administration".to_string(),
                site_header: "Administration".to_string(),
                time_zone: Tz::UTC,
                secure_cookies: false,
                csrf_cookie: "csrftoken".to_string(),
                report_error: Arc::new(|error, response| {
                    response
                        .extensions_mut()
                        .insert(InternalError(Arc::new(error)));
                }),
            },
        }
    }

    /// Every console route, ready to merge into the host's router.
    pub fn router<S: Clone + Send + Sync + 'static>(&self) -> Router<S> {
        self.router_with(Pages::new())
    }

    /// Every console route and the host's own `pages`, which get the
    /// [`Staff`] extractor and the console's error pages.
    pub fn router_with<S: Clone + Send + Sync + 'static>(&self, pages: Pages<S>) -> Router<S> {
        let mut own: Router<Console> = Router::new();
        own = both(own, BASE, get(pages::index));
        own = both(own, "/admin/static/admin.css", get(pages::stylesheet));
        own = both(own, "/admin/static/admin.js", get(pages::script));
        if matches!(self.0.sign_in, SignIn::Password(_)) {
            own = both(
                own,
                "/admin/login",
                get(session::login_form).post(session::login),
            );
            own = both(
                own,
                "/admin/logout",
                get(session::logout_get).post(session::logout),
            );
        }
        own = both(own, "/admin/{app}", get(pages::app_index));
        own = both(
            own,
            "/admin/{app}/{model}",
            get(changelist::page).post(changelist::action),
        );
        own = both(
            own,
            "/admin/{app}/{model}/add",
            get(form::add_form).post(form::add),
        );
        own = both(
            own,
            "/admin/{app}/{model}/{id}",
            get(pages::object_redirect),
        );
        own = both(
            own,
            "/admin/{app}/{model}/{id}/change",
            get(form::change_form).post(form::change),
        );
        own = both(
            own,
            "/admin/{app}/{model}/{id}/delete",
            get(form::delete).post(form::delete),
        );
        own = both(
            own,
            "/admin/{app}/{model}/{id}/history",
            get(pages::history),
        );

        let mut router: Router<S> = own.with_state(self.clone());
        for (path, method) in pages.routes {
            router = both(router, &path, method);
        }
        router
            .layer(map_response_with_state(self.clone(), render_errors))
            .layer(Extension(self.clone()))
    }

    /// The console's page for `error`, with its security headers: what a
    /// host answers itself for a request under [`BASE`] that no console
    /// route matched (a fallback of its own, see `README.md`).
    pub fn error_page(&self, error: AdminError) -> Response {
        let mut response = self.error_response(error);
        add_security_headers(&mut response);
        response
    }

    pub fn db(&self) -> &PgPool {
        &self.0.db
    }

    pub fn schema(&self) -> &'static Schema {
        self.0.schema
    }

    pub fn apps(&self) -> &'static [&'static App] {
        self.0.schema.apps
    }

    /// Every registered model.
    pub fn models(&self) -> impl Iterator<Item = &'static Model> + use<> {
        self.0.schema.models()
    }

    /// The app with this label, when it has models.
    pub fn app(&self, label: &str) -> Option<&'static App> {
        self.0.schema.app(label)
    }

    /// The model `name` of app `app`.
    pub fn model(&self, app: &str, name: &str) -> Option<&'static Model> {
        self.0.schema.model(app, name)
    }

    /// See [`Schema::table_key`].
    pub fn table_key(&self, table: &str) -> TableKey {
        self.0.schema.table_key(table)
    }

    /// See [`Schema::model_for_table`].
    pub fn model_for_table(&self, table: &str) -> Option<&'static Model> {
        self.0.schema.model_for_table(table)
    }

    /// The time zone datetimes are shown and edited in.
    pub fn time_zone(&self) -> Tz {
        self.0.time_zone
    }

    /// `expr` (of type `kind`) as the text the console shows: datetimes in
    /// the console's time zone. For SQL in computed fields.
    pub fn as_text(&self, expr: &str, kind: registry::Kind) -> String {
        query::as_text(expr, kind, self.0.time_zone)
    }

    /// Breadcrumbs from the model's app down to the model, then `extra`.
    pub fn crumbs(&self, model: &Model, extra: &[(String, String)]) -> Vec<(String, String)> {
        pages::crumbs(self, model, extra)
    }

    /// One row's field values as text and its text (`__str__`), or `None`
    /// when no row has this key.
    pub async fn load(
        &self,
        db: &mut sqlx::PgConnection,
        model: &Model,
        key: &Key,
    ) -> sqlx::Result<Option<(Values, String)>> {
        query::load(self, db, model, key).await
    }

    pub(crate) fn inner(&self) -> &Inner {
        &self.0
    }
}

impl ConsoleBuilder {
    /// Username and password sign-in on the console's own form.
    pub fn password_sign_in(mut self, sign_in: impl PasswordSignIn) -> Self {
        self.inner.sign_in = SignIn::Password(Box::new(sign_in));
        self
    }

    /// The host's sign-in page: an unauthenticated request is redirected to
    /// `url` with `param` set to the console path to return to
    /// (`/login?next=/admin/`).
    pub fn sign_in_url(mut self, url: impl Into<String>, param: impl Into<String>) -> Self {
        self.inner.sign_in = SignIn::Url {
            url: url.into(),
            param: Some(param.into()),
        };
        self
    }

    /// The host's sign-in page, for a host whose sign-in cannot return to
    /// the console: an unauthenticated request is redirected to `url` as it
    /// is, without a return path.
    pub fn sign_in_page(mut self, url: impl Into<String>) -> Self {
        self.inner.sign_in = SignIn::Url {
            url: url.into(),
            param: None,
        };
        self
    }

    /// The host's sign-out page, linked from every console page (when there
    /// is no [`PasswordSignIn`], whose sign-out the console offers itself).
    pub fn sign_out_url(mut self, url: impl Into<String>) -> Self {
        self.inner.sign_out_url = Some(url.into());
        self
    }

    pub fn history(mut self, history: impl History) -> Self {
        self.inner.history = Some(Box::new(history));
        self
    }

    pub fn passwords(mut self, passwords: impl Passwords) -> Self {
        self.inner.passwords = Some(Box::new(passwords));
        self
    }

    /// A link in the "Tools" box of the index, shown to staff for whom
    /// `visible` is true.
    pub fn index_link(
        mut self,
        label: impl Into<String>,
        url: impl Into<String>,
        visible: fn(&Perms) -> bool,
    ) -> Self {
        self.inner.links.push(IndexLink {
            label: label.into(),
            url: url.into(),
            visible,
        });
        self
    }

    /// The text after each page title in `<title>` (Django's `site_title`).
    pub fn site_title(mut self, title: impl Into<String>) -> Self {
        self.inner.site_title = title.into();
        self
    }

    /// The text in the page header (Django's `site_header`).
    pub fn site_header(mut self, header: impl Into<String>) -> Self {
        self.inner.site_header = header.into();
        self
    }

    pub fn time_zone(mut self, time_zone: Tz) -> Self {
        self.inner.time_zone = time_zone;
        self
    }

    /// Marks the console's cookies `Secure`.
    pub fn secure_cookies(mut self, secure: bool) -> Self {
        self.inner.secure_cookies = secure;
        self
    }

    /// The CSRF cookie's name (default `csrftoken`, Django's): a host whose
    /// API checks the same cookie shares it with the console.
    pub fn csrf_cookie(mut self, name: impl Into<String>) -> Self {
        self.inner.csrf_cookie = name.into();
        self
    }

    /// How server errors are reported. The default puts an
    /// [`InternalError`] in the error page's response extensions.
    pub fn report_errors(
        mut self,
        report: impl Fn(anyhow::Error, &mut Response) + Send + Sync + 'static,
    ) -> Self {
        self.inner.report_error = Arc::new(report);
        self
    }

    pub fn build(self) -> Console {
        Console(Arc::new(self.inner))
    }
}

/// The host's own console pages: routes under [`BASE`] whose handlers use
/// the [`Staff`] extractor to render inside the console (see
/// [`Console::router_with`]). Each path answers with and without a trailing
/// slash.
pub struct Pages<S> {
    routes: Vec<(String, MethodRouter<S>)>,
}

impl<S: Clone + Send + Sync + 'static> Pages<S> {
    pub fn new() -> Self {
        Self { routes: Vec::new() }
    }

    /// A page at `path` (`/admin/reports`).
    pub fn route(mut self, path: &str, method: MethodRouter<S>) -> Self {
        self.routes
            .push((path.trim_end_matches('/').to_string(), method));
        self
    }
}

impl<S: Clone + Send + Sync + 'static> Default for Pages<S> {
    fn default() -> Self {
        Self::new()
    }
}

/// `path` and `path/`, so the console answers both with or without a
/// host's trailing-slash handling in front of it.
fn both<S: Clone + Send + Sync + 'static>(
    router: Router<S>,
    path: &str,
    method: MethodRouter<S>,
) -> Router<S> {
    router
        .route(path, method.clone())
        .route(&format!("{path}/"), method)
}

/// Headers every console response carries unless the handler set its own:
/// pages load only the console's stylesheet and script, post only to this
/// host, and cannot be framed. Inline styles stay allowed for the HTML of
/// a host's computed fields.
const SECURITY_HEADERS: [(&str, &str); 4] = [
    (
        "content-security-policy",
        "default-src 'none'; style-src 'self' 'unsafe-inline'; script-src 'self'; \
         img-src 'self' data:; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
    ),
    ("x-content-type-options", "nosniff"),
    ("x-frame-options", "DENY"),
    ("referrer-policy", "same-origin"),
];

/// Renders the page of an [`AdminError`] a handler or extractor returned,
/// and adds the console's security headers.
async fn render_errors(State(console): State<Console>, mut response: Response) -> Response {
    let mut response = match session::take_error(&mut response) {
        Some(error) => console.error_response(error),
        None => response,
    };
    add_security_headers(&mut response);
    response
}

fn add_security_headers(response: &mut Response) {
    for (name, value) in SECURITY_HEADERS {
        response
            .headers_mut()
            .entry(name)
            .or_insert(axum::http::HeaderValue::from_static(value));
    }
}
