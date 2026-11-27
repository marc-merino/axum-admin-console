//! The console mounted over a small library schema (`tests/migrations`):
//! integer-keyed authors, UUID-keyed books, text-keyed tags, books tagged
//! many-to-many and reviews that protect their book. Who is asking comes
//! from a `who` cookie; the history hook writes `console_history`.

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, Request, StatusCode, header, request::Parts},
    response::Response,
    routing::get,
};
use axum_admin_console::{
    AdminResult, Auth, Change, Console, Entry, EntryKind, Event, Form, History, Identity, Key,
    KeyKind, OnDelete, Pages, PasswordSignIn, Perms, Relation, Schema, SignInOutcome, Staff,
    StaffMember, csrf,
    html::esc,
    registry::{
        Action, App, BoxFuture, Column, Computed, ComputedRender, Download, Export, Field,
        Fieldset, Filter, Inline, Kind, ManyToMany, Model, Path, Sort,
    },
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use http_body_util::BodyExt;
use sqlx::{PgConnection, PgPool};
use tower::ServiceExt;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// The registered models

static AUTHOR: Model = Model::new("library", "author", "library_author", "author", "authors")
    .repr("{t}.name")
    .fields(&[Field::char("name", "Name", 100)])
    .list_display(&[Column::field("name")])
    .search(&[Path::col("name")])
    .ordering(&[Sort::asc(Path::col("name"))])
    .inlines(&[Inline {
        label: "Books",
        table: "library_book",
        fk: "author_id",
        columns: &[("Title", "{t}.title", Kind::Char)],
        order: "{t}.title",
        link: None,
    }]);

const GENRES: &[(&str, &str)] = &[
    ("novel", "Novel"),
    ("poetry", "Poetry"),
    ("classic", "Classic"),
];

static BOOK: Model = Model::new("library", "book", "library_book", "book", "books")
    .key(KeyKind::Uuid)
    .repr("{t}.title")
    .fields(&[
        Field::char("title", "Title", 200),
        Field::choice("genre", "Genre", GENRES).default("novel"),
        Field::date("published", "Published").null(),
        Field::fk("author", "author_id", "Author", "library_author"),
        Field::fk("editor", "editor_id", "Editor", "library_author").null(),
    ])
    .m2m(&[ManyToMany {
        name: "tags",
        label: "Tags",
        through: "library_book_tags",
        own: "book_id",
        other: "tag_id",
        target: "library_tag",
        options_sql: "SELECT code, label FROM library_tag ORDER BY label",
        help: "",
    }])
    .computed(&[Computed {
        name: "key_text",
        label: "Key",
        render: ComputedRender::Html(key_text),
    }])
    .fieldsets(&[Fieldset::new(
        None,
        &[
            "title",
            "genre",
            "published",
            "author",
            "editor",
            "tags",
            "key_text",
        ],
    )])
    .readonly(&["key_text"])
    .list_display(&[
        Column::field("title"),
        Column::field("author"),
        Column::field("genre"),
    ])
    .list_filter(&[Filter::Field("genre"), Filter::Field("author")])
    .search(&[
        Path::col("title"),
        Path::via(&[("author_id", "library_author")], "name"),
    ])
    .ordering(&[Sort::asc(Path::col("title"))])
    .per_page(2)
    .actions(&[Action {
        name: "make_classic",
        label: "Mark as classic",
        run: make_classic,
    }])
    .exports(&[Export {
        name: "export_keys",
        label: "Export keys",
        run: export_keys,
    }]);

static TAG: Model = Model::new("library", "tag", "library_tag", "tag", "tags")
    .pk("code")
    .key(KeyKind::Text)
    .repr("{t}.label")
    .fields(&[
        Field::char("code", "Code", 50),
        Field::char("label", "Label", 100),
    ])
    .list_display(&[Column::field("code"), Column::field("label")])
    .ordering(&[Sort::asc(Path::col("code"))]);

static APP: App = App {
    label: "library",
    name: "Library",
    models: &[&AUTHOR, &BOOK, &TAG],
};

static RELATIONS: &[Relation] = &[
    (
        "library_book",
        "author_id",
        "library_author",
        OnDelete::Cascade,
    ),
    (
        "library_book",
        "editor_id",
        "library_author",
        OnDelete::SetNull,
    ),
    (
        "library_book_tags",
        "book_id",
        "library_book",
        OnDelete::Cascade,
    ),
    (
        "library_book_tags",
        "tag_id",
        "library_tag",
        OnDelete::Cascade,
    ),
    (
        "library_review",
        "book_id",
        "library_book",
        OnDelete::Protect,
    ),
];

static SCHEMA: Schema = Schema::new(&[&APP]).delete_rules(RELATIONS, &[]);

/// Shows which key the computed field was given.
fn key_text(_db: &mut PgConnection, key: Key) -> BoxFuture<'_, anyhow::Result<String>> {
    Box::pin(async move {
        let kind = if key.as_uuid().is_some() {
            "uuid"
        } else {
            "other"
        };
        Ok(format!(
            "<code id=\"computed-key\">{kind}:{}</code>",
            esc(&key.to_string())
        ))
    })
}

fn make_classic<'a>(
    db: &'a mut PgConnection,
    keys: &'a [Key],
) -> BoxFuture<'a, anyhow::Result<String>> {
    Box::pin(async move {
        let updated = sqlx::query("UPDATE library_book SET genre = 'classic' WHERE id = ANY($1)")
            .bind(Key::uuids(keys))
            .execute(&mut *db)
            .await?
            .rows_affected();
        Ok(format!("Marked {updated} books as classic."))
    })
}

fn export_keys<'a>(
    _db: &'a mut PgConnection,
    keys: &'a [Key],
) -> BoxFuture<'a, anyhow::Result<Download>> {
    Box::pin(async move {
        let lines: Vec<String> = Key::uuids(keys).iter().map(Uuid::to_string).collect();
        Ok(Download {
            filename: "keys.txt",
            content_type: "text/plain; charset=utf-8",
            body: lines.join("\n"),
        })
    })
}

// ---------------------------------------------------------------------------
// The host's hooks

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
}

fn staff_name(id: &str) -> &'static str {
    match id {
        "1" => "root",
        "2" => "clerk",
        "3" => "reader",
        _ => "someone",
    }
}

/// `who=root` is a superuser, `clerk` may view and change books, `reader`
/// may only view them; any other name is signed in but not staff.
struct CookieAuth;

impl Auth for CookieAuth {
    fn identify<'a>(&'a self, parts: &'a Parts) -> BoxFuture<'a, Identity> {
        Box::pin(async move {
            let member = |id: &str, perms: Perms| {
                Identity::Staff(StaffMember {
                    id: id.to_string(),
                    name: staff_name(id).to_string(),
                    perms,
                })
            };
            let grant = |codes: &[&str]| {
                Perms::granted(
                    codes
                        .iter()
                        .map(|code| ("library".to_string(), code.to_string()))
                        .collect(),
                )
            };
            match cookie(&parts.headers, "who") {
                None | Some("") => Identity::Anonymous,
                Some("root") => member("1", Perms::all()),
                Some("clerk") => member("2", grant(&["view_book", "change_book"])),
                Some("reader") => member("3", grant(&["view_book"])),
                Some(other) => Identity::NotStaff {
                    name: other.to_string(),
                },
            }
        })
    }
}

/// `root` / `secret` signs in by setting the `who` cookie.
struct CookieSignIn;

fn set_who(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let cookie = if value.is_empty() {
        "who=; Path=/; Max-Age=0".to_string()
    } else {
        format!("who={value}; Path=/; HttpOnly")
    };
    headers.append(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
    headers
}

impl PasswordSignIn for CookieSignIn {
    fn sign_in<'a>(
        &'a self,
        _parts: &'a Parts,
        username: &'a str,
        password: &'a str,
    ) -> BoxFuture<'a, SignInOutcome> {
        Box::pin(async move {
            match (username, password) {
                ("root", "secret") => SignInOutcome::SignedIn(set_who("root")),
                ("slow", _) => SignInOutcome::Throttled,
                _ => SignInOutcome::Refused,
            }
        })
    }

    fn sign_out<'a>(&'a self, _parts: &'a Parts) -> BoxFuture<'a, anyhow::Result<HeaderMap>> {
        Box::pin(async move { Ok(set_who("")) })
    }
}

/// History in `console_history`, written in the console's transaction.
struct TableHistory;

type HistoryRow = (
    chrono::DateTime<chrono::Utc>,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
);

fn entry(row: HistoryRow) -> Entry {
    let (at, staff, app, model, key, repr, kind, message) = row;
    Entry {
        at,
        staff_name: staff_name(&staff).to_string(),
        app: Some(app),
        model: Some(model),
        key: Some(key),
        repr,
        kind: match kind.as_str() {
            "added" => EntryKind::Added,
            "deleted" => EntryKind::Deleted,
            _ => EntryKind::Changed,
        },
        message,
    }
}

const HISTORY_SELECT: &str =
    "SELECT at, staff, app, model, key, repr, kind, message FROM console_history";

impl History for TableHistory {
    fn record<'a>(
        &'a self,
        db: &'a mut PgConnection,
        change: Change<'a>,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let (kind, message) = match change.event {
                Event::Added => ("added", "Added.".to_string()),
                Event::Changed(labels) => ("changed", format!("Changed {}.", labels.join(", "))),
                Event::Deleted => ("deleted", "Deleted.".to_string()),
                Event::Action(label) => ("changed", format!("Applied action: {label}.")),
            };
            sqlx::query(
                "INSERT INTO console_history (staff, app, model, key, repr, kind, message) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(change.staff_id)
            .bind(change.model.app)
            .bind(change.model.name)
            .bind(change.key.to_string())
            .bind(change.repr)
            .bind(kind)
            .bind(message)
            .execute(db)
            .await?;
            Ok(())
        })
    }

    fn recent<'a>(
        &'a self,
        db: &'a mut PgConnection,
        staff_id: &'a str,
        limit: i64,
    ) -> BoxFuture<'a, anyhow::Result<Vec<Entry>>> {
        Box::pin(async move {
            let rows: Vec<HistoryRow> = sqlx::query_as(&format!(
                "{HISTORY_SELECT} WHERE staff = $1 ORDER BY id DESC LIMIT $2"
            ))
            .bind(staff_id)
            .bind(limit)
            .fetch_all(db)
            .await?;
            Ok(rows.into_iter().map(entry).collect())
        })
    }

    fn object<'a>(
        &'a self,
        db: &'a mut PgConnection,
        model: &'a Model,
        key: &'a Key,
        limit: i64,
    ) -> BoxFuture<'a, anyhow::Result<Vec<Entry>>> {
        Box::pin(async move {
            let rows: Vec<HistoryRow> = sqlx::query_as(&format!(
                "{HISTORY_SELECT} WHERE app = $1 AND model = $2 AND key = $3 ORDER BY id LIMIT $4"
            ))
            .bind(model.app)
            .bind(model.name)
            .bind(key.to_string())
            .bind(limit)
            .fetch_all(db)
            .await?;
            Ok(rows.into_iter().map(entry).collect())
        })
    }
}

// ---------------------------------------------------------------------------
// The host: its own state and a page of its own in the console

#[derive(Clone)]
struct Host {
    greeting: &'static str,
}

async fn reports(State(host): State<Host>, staff: Staff) -> AdminResult<Response> {
    let content = format!(
        "<div id=\"content-main\"><p id=\"greeting\">{} {}</p></div>",
        esc(host.greeting),
        esc(&staff.name)
    );
    Ok(staff.page(
        "Reports",
        &[("Reports".to_string(), String::new())],
        "reports",
        &content,
    ))
}

async fn run_report(staff: Staff, headers: HeaderMap, form: Form) -> AdminResult<Response> {
    staff.check_csrf(&headers, &form)?;
    Ok(staff.redirect("/admin/reports/", "success", "Report queued."))
}

enum SignIn {
    Password,
    Url,
    None,
}

fn app(db: PgPool, sign_in: SignIn, history: bool) -> Router {
    let mut builder = Console::builder(db, &SCHEMA, CookieAuth)
        .site_title("Library administration")
        .site_header("Library")
        .index_link("Reports", "/admin/reports/", |_| true);
    builder = match sign_in {
        SignIn::Password => builder.password_sign_in(CookieSignIn),
        SignIn::Url => builder
            .sign_in_url("/accounts/sign-in", "return")
            .sign_out_url("/accounts/sign-out"),
        SignIn::None => builder,
    };
    if history {
        builder = builder.history(TableHistory);
    }
    builder
        .build()
        .router_with(Pages::new().route("/admin/reports", get(reports).post(run_report)))
        .with_state(Host { greeting: "Hello" })
}

// ---------------------------------------------------------------------------
// Requests

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn location(&self) -> &str {
        self.headers
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }

    /// The flash message the redirect leaves: `(level, text)`.
    fn flash(&self) -> Option<(String, String)> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|v| v.strip_prefix("adminmsg="))
            .and_then(|v| v.split(';').next())
            .and_then(|v| URL_SAFE_NO_PAD.decode(v).ok())
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|text| {
                text.split_once('\t')
                    .map(|(a, b)| (a.to_string(), b.to_string()))
            })
    }

    fn set_cookies(&self) -> Vec<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_string))
            .collect()
    }
}

struct Client {
    app: Router,
    who: Option<&'static str>,
    secret: String,
}

impl Client {
    fn new(app: Router, who: Option<&'static str>) -> Self {
        Self {
            app,
            who,
            secret: csrf::generate_token(),
        }
    }

    fn cookies(&self) -> String {
        let mut out = format!("csrftoken={}", self.secret);
        if let Some(who) = self.who {
            out.push_str(&format!("; who={who}"));
        }
        out
    }

    async fn send(&self, request: Request<Body>) -> Reply {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    async fn get(&self, uri: &str) -> Reply {
        self.send(
            Request::get(uri)
                .header(header::HOST, "admin.test")
                .header(header::COOKIE, self.cookies())
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    async fn post_raw(&self, uri: &str, fields: &[(&str, &str)], extra: &[(&str, &str)]) -> Reply {
        let mut request = Request::post(uri)
            .header(header::HOST, "admin.test")
            .header(header::COOKIE, self.cookies())
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        let body = serde_urlencoded::to_string(fields).unwrap();
        self.send(request.body(Body::from(body)).unwrap()).await
    }

    /// A POST carrying a masked CSRF token for the cookie's secret.
    async fn post(&self, uri: &str, fields: &[(&str, &str)]) -> Reply {
        let token = csrf::mask_token(&self.secret);
        let mut all = vec![("csrfmiddlewaretoken", token.as_str())];
        all.extend_from_slice(fields);
        self.post_raw(uri, &all, &[]).await
    }
}

// ---------------------------------------------------------------------------
// Fixtures

async fn author(db: &PgPool, name: &str) -> i64 {
    sqlx::query_scalar("INSERT INTO library_author (name) VALUES ($1) RETURNING id")
        .bind(name)
        .fetch_one(db)
        .await
        .unwrap()
}

async fn book(db: &PgPool, title: &str, genre: &str, author: i64, editor: Option<i64>) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO library_book (title, genre, author_id, editor_id) VALUES ($1, $2, $3, $4) \
         RETURNING id",
    )
    .bind(title)
    .bind(genre)
    .bind(author)
    .bind(editor)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn tag(db: &PgPool, code: &str, label: &str) {
    sqlx::query("INSERT INTO library_tag (code, label) VALUES ($1, $2)")
        .bind(code)
        .bind(label)
        .execute(db)
        .await
        .unwrap();
}

async fn tag_book(db: &PgPool, book: Uuid, code: &str) {
    sqlx::query("INSERT INTO library_book_tags (book_id, tag_id) VALUES ($1, $2)")
        .bind(book)
        .bind(code)
        .execute(db)
        .await
        .unwrap();
}

async fn history(db: &PgPool) -> Vec<(String, String, String, String)> {
    sqlx::query_as("SELECT model, key, kind, message FROM console_history ORDER BY id")
        .fetch_all(db)
        .await
        .unwrap()
}

fn position(body: &str, needle: &str) -> usize {
    body.find(needle)
        .unwrap_or_else(|| panic!("{needle:?} not in page"))
}

// ---------------------------------------------------------------------------
// Signing in

#[sqlx::test(migrations = "tests/migrations")]
async fn password_sign_in_redirects_signs_in_and_out(db: PgPool) {
    let anonymous = Client::new(app(db.clone(), SignIn::Password, true), None);
    let reply = anonymous.get("/admin/library/book/?q=x").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        reply.location(),
        "/admin/login/?next=%2Fadmin%2Flibrary%2Fbook%2F%3Fq%3Dx"
    );

    let form = anonymous.get("/admin/login/?next=/admin/library/").await;
    assert_eq!(form.status, StatusCode::OK);
    assert!(form.body.contains("name=\"username\""));
    assert!(form.body.contains("Library administration"));

    let wrong = anonymous
        .post(
            "/admin/login/",
            &[
                ("username", "root"),
                ("password", "nope"),
                ("next", "/admin/library/"),
            ],
        )
        .await;
    assert_eq!(wrong.status, StatusCode::OK);
    assert!(
        wrong
            .body
            .contains("Please enter the correct username and password")
    );

    let throttled = anonymous
        .post("/admin/login/", &[("username", "slow"), ("password", "x")])
        .await;
    assert_eq!(throttled.status, StatusCode::TOO_MANY_REQUESTS);

    let signed_in = anonymous
        .post(
            "/admin/login/",
            &[
                ("username", "root"),
                ("password", "secret"),
                ("next", "/admin/library/"),
            ],
        )
        .await;
    assert_eq!(signed_in.status, StatusCode::SEE_OTHER);
    assert_eq!(signed_in.location(), "/admin/library/");
    let cookies = signed_in.set_cookies();
    assert!(cookies.iter().any(|c| c.starts_with("who=root")));
    assert!(cookies.iter().any(|c| c.starts_with("csrftoken=")));

    // An outside `next` is not followed.
    let outside = anonymous
        .post(
            "/admin/login/",
            &[
                ("username", "root"),
                ("password", "secret"),
                ("next", "https://elsewhere.test/"),
            ],
        )
        .await;
    assert_eq!(outside.location(), "/admin/");

    let root = Client::new(app(db.clone(), SignIn::Password, true), Some("root"));
    let index = root.get("/admin/").await;
    assert!(index.body.contains("action=\"/admin/logout/\""));
    let out = root.post("/admin/logout/", &[]).await;
    assert_eq!(out.status, StatusCode::OK);
    assert!(out.body.contains("Logged out"));
    assert!(out.set_cookies().iter().any(|c| c.starts_with("who=;")));
}

#[sqlx::test(migrations = "tests/migrations")]
async fn without_password_sign_in_the_host_signs_in(db: PgPool) {
    let anonymous = Client::new(app(db.clone(), SignIn::Url, true), None);
    let reply = anonymous.get("/admin/library/book/").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(
        reply.location(),
        "/accounts/sign-in?return=%2Fadmin%2Flibrary%2Fbook%2F"
    );
    // The console has no sign-in page of its own.
    let login = anonymous.get("/admin/login/").await;
    assert_eq!(
        login.location(),
        "/accounts/sign-in?return=%2Fadmin%2Flogin%2F"
    );

    let root = Client::new(app(db.clone(), SignIn::Url, true), Some("root"));
    assert_eq!(
        root.get("/admin/login/").await.status,
        StatusCode::NOT_FOUND
    );
    let index = root.get("/admin/").await;
    assert!(
        index
            .body
            .contains("<a href=\"/accounts/sign-out\">Log out</a>")
    );
    assert!(!index.body.contains("/admin/logout/"));

    let nowhere = Client::new(app(db, SignIn::None, true), None);
    let reply = nowhere.get("/admin/").await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(reply.body.contains("You are not signed in."));
}

#[sqlx::test(migrations = "tests/migrations")]
async fn a_user_who_is_not_staff_is_refused(db: PgPool) {
    let visitor = Client::new(app(db.clone(), SignIn::Url, true), Some("visitor"));
    let reply = visitor.get("/admin/").await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(
        reply.body.contains(
            "You are authenticated as visitor, but are not authorized to access this page."
        )
    );

    // With password sign-in the console offers to sign in as someone else.
    let visitor = Client::new(app(db, SignIn::Password, true), Some("visitor"));
    let reply = visitor.get("/admin/").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert!(reply.location().starts_with("/admin/login/"));
    let form = visitor.get(reply.location()).await;
    assert!(
        form.body
            .contains("Would you like to login to a different account?")
    );
}

// ---------------------------------------------------------------------------
// Permissions and CSRF

#[sqlx::test(migrations = "tests/migrations")]
async fn staff_without_superuser_get_only_their_permissions(db: PgPool) {
    let writer = author(&db, "Ann").await;
    let dune = book(&db, "Dune", "novel", writer, None).await;
    let change_url = format!("/admin/library/book/{dune}/change/");

    let clerk = Client::new(app(db.clone(), SignIn::Password, true), Some("clerk"));
    let index = clerk.get("/admin/").await;
    assert!(index.body.contains("/admin/library/book/"));
    assert!(!index.body.contains("/admin/library/author/"));
    assert!(!index.body.contains("/admin/library/book/add/"));
    assert_eq!(
        clerk.get("/admin/library/book/").await.status,
        StatusCode::OK
    );
    assert_eq!(
        clerk.get("/admin/library/author/").await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        clerk.get("/admin/library/book/add/").await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        clerk
            .get(&format!("/admin/library/book/{dune}/delete/"))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    let page = clerk.get(&change_url).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("name=\"title\""));

    let reader = Client::new(app(db.clone(), SignIn::Password, true), Some("reader"));
    let page = reader.get(&change_url).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("View book"));
    assert!(!page.body.contains("name=\"title\""));
    let refused = reader
        .post(
            &change_url,
            &[("title", "Changed"), ("genre", "novel"), ("author", "1")],
        )
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    let title: String = sqlx::query_scalar("SELECT title FROM library_book")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(title, "Dune");
}

#[sqlx::test(migrations = "tests/migrations")]
async fn posts_without_a_matching_csrf_token_are_refused(db: PgPool) {
    let writer = author(&db, "Ann").await;
    let dune = book(&db, "Dune", "novel", writer, None).await;
    let root = Client::new(app(db.clone(), SignIn::Password, true), Some("root"));
    let url = format!("/admin/library/book/{dune}/change/");
    let writer_text = writer.to_string();
    let fields = [
        ("title", "Changed"),
        ("genre", "novel"),
        ("author", writer_text.as_str()),
    ];

    let missing = root.post_raw(&url, &fields, &[]).await;
    assert_eq!(missing.status, StatusCode::FORBIDDEN);
    assert!(
        missing
            .body
            .contains("CSRF verification failed. Request aborted.")
    );

    let other = csrf::mask_token(&csrf::generate_token());
    let mut wrong = vec![("csrfmiddlewaretoken", other.as_str())];
    wrong.extend_from_slice(&fields);
    assert_eq!(
        root.post_raw(&url, &wrong, &[]).await.status,
        StatusCode::FORBIDDEN
    );

    let token = csrf::mask_token(&root.secret);
    let mut right = vec![("csrfmiddlewaretoken", token.as_str())];
    right.extend_from_slice(&fields);
    let foreign = root
        .post_raw(&url, &right, &[("origin", "https://elsewhere.test")])
        .await;
    assert_eq!(foreign.status, StatusCode::FORBIDDEN);

    let host_page = root.post_raw("/admin/reports/", &[], &[]).await;
    assert_eq!(host_page.status, StatusCode::FORBIDDEN);

    let title: String = sqlx::query_scalar("SELECT title FROM library_book")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(title, "Dune");

    let accepted = root
        .post_raw(&url, &right, &[("origin", "http://admin.test")])
        .await;
    assert_eq!(accepted.status, StatusCode::SEE_OTHER);
}

// ---------------------------------------------------------------------------
// The change list of a UUID-keyed model

#[sqlx::test(migrations = "tests/migrations")]
async fn uuid_list_searches_filters_sorts_and_pages(db: PgPool) {
    let ann = author(&db, "Ann").await;
    let bob = author(&db, "Bob").await;
    book(&db, "Alpha", "novel", ann, None).await;
    book(&db, "Beta", "poetry", bob, None).await;
    book(&db, "Gamma", "poetry", ann, None).await;
    book(&db, "Delta", "novel", bob, None).await;
    book(&db, "Epsilon", "classic", ann, None).await;
    let root = Client::new(app(db.clone(), SignIn::Password, true), Some("root"));

    // Two per page, by title.
    let first = root.get("/admin/library/book/").await;
    assert_eq!(first.status, StatusCode::OK);
    assert!(first.body.contains("Alpha") && first.body.contains("Beta"));
    assert!(!first.body.contains("Delta"));
    assert!(first.body.contains("5 books"));
    let second = root.get("/admin/library/book/?p=2").await;
    assert!(second.body.contains("Delta") && second.body.contains("Epsilon"));
    assert!(!second.body.contains("Alpha"));
    let third = root.get("/admin/library/book/?p=3").await;
    assert!(third.body.contains("Gamma"));

    // Reverse order of the first column.
    let reversed = root.get("/admin/library/book/?o=-1").await;
    assert!(position(&reversed.body, "Gamma") < position(&reversed.body, "Epsilon"));
    assert!(!reversed.body.contains("Alpha"));

    // Search through the author's name, and the title.
    let by_author = root.get("/admin/library/book/?q=bob").await;
    assert!(by_author.body.contains("Beta") && by_author.body.contains("Delta"));
    assert!(!by_author.body.contains("Alpha"));
    let by_title = root.get("/admin/library/book/?q=psil").await;
    assert!(by_title.body.contains("Epsilon"));
    assert!(!by_title.body.contains("Gamma"));

    // Choice and foreign-key filters.
    let poetry = root.get("/admin/library/book/?genre__exact=poetry").await;
    assert!(poetry.body.contains("Beta") && poetry.body.contains("Gamma"));
    assert!(!poetry.body.contains("Delta"));
    let anns = root
        .get(&format!("/admin/library/book/?author__id__exact={ann}&o=1"))
        .await;
    assert!(anns.body.contains("Alpha") && anns.body.contains("Epsilon"));
    assert!(!anns.body.contains("Beta"));

    // Each row links to its change page by UUID.
    let alpha: Uuid = sqlx::query_scalar("SELECT id FROM library_book WHERE title = 'Alpha'")
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(
        first
            .body
            .contains(&format!("/admin/library/book/{alpha}/change/"))
    );
    assert!(first.body.contains(&format!("value=\"{alpha}\"")));
}

// ---------------------------------------------------------------------------
// Forms and history

#[sqlx::test(migrations = "tests/migrations")]
async fn add_change_and_delete_are_recorded_in_history(db: PgPool) {
    let ann = author(&db, "Ann").await;
    tag(&db, "space", "Space").await;
    tag(&db, "sand", "Sand").await;
    let root = Client::new(app(db.clone(), SignIn::Password, true), Some("root"));
    let ann_text = ann.to_string();

    let form = root.get("/admin/library/book/add/").await;
    assert_eq!(form.status, StatusCode::OK);
    assert!(form.body.contains("name=\"tags\""));

    let invalid = root
        .post(
            "/admin/library/book/add/",
            &[("title", ""), ("genre", "novel"), ("author", &ann_text)],
        )
        .await;
    assert_eq!(invalid.status, StatusCode::OK);
    assert!(invalid.body.contains("This field is required."));

    let added = root
        .post(
            "/admin/library/book/add/",
            &[
                ("title", "Dune"),
                ("genre", "novel"),
                ("published", "1965-08-01"),
                ("author", &ann_text),
                ("editor", ""),
                ("tags", "space"),
                ("tags", "sand"),
            ],
        )
        .await;
    assert_eq!(added.status, StatusCode::SEE_OTHER, "{}", added.body);
    assert_eq!(added.location(), "/admin/library/book/");
    assert_eq!(
        added.flash(),
        Some((
            "success".to_string(),
            "The book “Dune” was added successfully.".to_string()
        ))
    );
    let dune: Uuid = sqlx::query_scalar("SELECT id FROM library_book WHERE title = 'Dune'")
        .fetch_one(&db)
        .await
        .unwrap();
    let tags: Vec<String> = sqlx::query_scalar(
        "SELECT tag_id FROM library_book_tags WHERE book_id = $1 ORDER BY tag_id",
    )
    .bind(dune)
    .fetch_all(&db)
    .await
    .unwrap();
    assert_eq!(tags, ["sand", "space"]);

    let url = format!("/admin/library/book/{dune}/change/");
    let page = root.get(&url).await;
    assert!(page.body.contains(&format!("uuid:{dune}")));
    assert!(
        page.body
            .contains(&format!("/admin/library/book/{dune}/history/"))
    );

    let changed = root
        .post(
            &url,
            &[
                ("title", "Dune Messiah"),
                ("genre", "novel"),
                ("published", "1965-08-01"),
                ("author", &ann_text),
                ("editor", ""),
                ("tags", "sand"),
                ("_continue", "1"),
            ],
        )
        .await;
    assert_eq!(changed.status, StatusCode::SEE_OTHER, "{}", changed.body);
    assert_eq!(changed.location(), url);
    let tags: i64 = sqlx::query_scalar("SELECT count(*) FROM library_book_tags")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(tags, 1);

    let page = root
        .get(&format!("/admin/library/book/{dune}/history/"))
        .await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Added."));
    assert!(page.body.contains("Changed Title, Tags."));
    assert!(page.body.contains("root"));

    let index = root.get("/admin/").await;
    assert!(index.body.contains("Recent actions"));
    assert!(index.body.contains(&format!(
        "<a href=\"/admin/library/book/{dune}/change/\">Dune Messiah</a>"
    )));

    let confirm = root
        .get(&format!("/admin/library/book/{dune}/delete/"))
        .await;
    assert!(confirm.body.contains("Are you sure"));
    let deleted = root
        .post(
            &format!("/admin/library/book/{dune}/delete/"),
            &[("post", "yes")],
        )
        .await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER);
    assert_eq!(
        deleted.flash().unwrap().1,
        "The book “Dune Messiah” was deleted successfully."
    );
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM library_book")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(left, 0);

    let dune_text = dune.to_string();
    let recorded = history(&db).await;
    let expected = [
        ("added", "Added."),
        ("changed", "Changed Title, Tags."),
        ("deleted", "Deleted."),
    ];
    assert_eq!(recorded.len(), expected.len());
    for ((model, key, kind, message), (want_kind, want_message)) in recorded.iter().zip(expected) {
        assert_eq!(model, "book");
        assert_eq!(key, &dune_text);
        assert_eq!(kind, want_kind);
        assert_eq!(message, want_message);
    }

    // A malformed key is a page that does not exist.
    assert_eq!(
        root.get("/admin/library/book/42/change/").await.status,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrations = "tests/migrations")]
async fn text_keys_are_escaped_in_urls_and_edited(db: PgPool) {
    tag(&db, "sci fi", "Science fiction").await;
    let ann = author(&db, "Ann").await;
    let dune = book(&db, "Dune", "novel", ann, None).await;
    tag_book(&db, dune, "sci fi").await;
    let root = Client::new(app(db.clone(), SignIn::Password, true), Some("root"));

    let list = root.get("/admin/library/tag/").await;
    assert!(list.body.contains("/admin/library/tag/sci%20fi/change/"));
    let page = root.get("/admin/library/tag/sci%20fi/change/").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("Science fiction"));

    let changed = root
        .post(
            "/admin/library/tag/sci%20fi/change/",
            &[("code", "sci fi"), ("label", "Sci-fi")],
        )
        .await;
    assert_eq!(changed.status, StatusCode::SEE_OTHER, "{}", changed.body);
    let label: String = sqlx::query_scalar("SELECT label FROM library_tag")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(label, "Sci-fi");
    assert_eq!(history(&db).await[0].1, "sci fi");

    // The book's tags show the text-keyed tag by its text.
    let book_page = root
        .get(&format!("/admin/library/book/{dune}/change/"))
        .await;
    assert!(book_page.body.contains("Sci-fi"));
    assert!(book_page.body.contains("value=\"sci fi\" selected"));

    // The author's page lists the book inline, linked by its UUID.
    let author_page = root
        .get(&format!("/admin/library/author/{ann}/change/"))
        .await;
    assert!(
        author_page
            .body
            .contains(&format!("/admin/library/book/{dune}/change/"))
    );
}

// ---------------------------------------------------------------------------
// Deleting across key types

#[sqlx::test(migrations = "tests/migrations")]
async fn delete_plan_cascades_clears_and_protects_across_key_types(db: PgPool) {
    let ann = author(&db, "Ann").await;
    let bob = author(&db, "Bob").await;
    let cid = author(&db, "Cid").await;
    tag(&db, "space", "Space").await;
    let dune = book(&db, "Dune", "novel", ann, None).await;
    tag_book(&db, dune, "space").await;
    let edited = book(&db, "Edited by Ann", "novel", bob, Some(ann)).await;
    let reviewed = book(&db, "Reviewed", "novel", cid, None).await;
    sqlx::query("INSERT INTO library_review (book_id, body) VALUES ($1, 'Fine')")
        .bind(reviewed)
        .execute(&db)
        .await
        .unwrap();
    let root = Client::new(app(db.clone(), SignIn::Password, true), Some("root"));

    let confirm = root
        .get(&format!("/admin/library/author/{ann}/delete/"))
        .await;
    assert_eq!(confirm.status, StatusCode::OK);
    assert!(confirm.body.contains("Dune"));
    assert!(confirm.body.contains("value=\"Yes, I’m sure\""));
    assert!(!confirm.body.contains("Edited by Ann"));

    let deleted = root
        .post(
            &format!("/admin/library/author/{ann}/delete/"),
            &[("post", "yes")],
        )
        .await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER, "{}", deleted.body);
    let books: Vec<String> = sqlx::query_scalar("SELECT title FROM library_book ORDER BY title")
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(books, ["Edited by Ann", "Reviewed"]);
    let links: i64 = sqlx::query_scalar("SELECT count(*) FROM library_book_tags")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(links, 0);
    let editor: Option<i64> =
        sqlx::query_scalar("SELECT editor_id FROM library_book WHERE id = $1")
            .bind(edited)
            .fetch_one(&db)
            .await
            .unwrap();
    assert_eq!(editor, None);
    let tags: i64 = sqlx::query_scalar("SELECT count(*) FROM library_tag")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(tags, 1);

    // A review protects its book, so its author cannot be deleted.
    let blocked = root
        .get(&format!("/admin/library/author/{cid}/delete/"))
        .await;
    assert!(
        blocked
            .body
            .contains("would require deleting the following protected related objects")
    );
    let refused = root
        .post(
            &format!("/admin/library/author/{cid}/delete/"),
            &[("post", "yes")],
        )
        .await;
    assert_eq!(refused.status, StatusCode::OK);
    let authors: i64 = sqlx::query_scalar("SELECT count(*) FROM library_author")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(authors, 2);

    // "Delete selected" on UUID keys uses the same plan.
    let dune_again = book(&db, "Dune", "novel", bob, None).await;
    let selected = [dune_again.to_string(), reviewed.to_string()];
    let blocked = root
        .post(
            "/admin/library/book/",
            &[
                ("action", "delete_selected"),
                ("_selected_action", &selected[0]),
                ("_selected_action", &selected[1]),
                ("post", "yes"),
            ],
        )
        .await;
    assert_eq!(blocked.status, StatusCode::OK);
    assert!(blocked.body.contains("protected related objects"));
    let gone = root
        .post(
            "/admin/library/book/",
            &[
                ("action", "delete_selected"),
                ("_selected_action", &selected[0]),
                ("post", "yes"),
            ],
        )
        .await;
    assert_eq!(gone.flash().unwrap().1, "Successfully deleted 1 book.");
}

// ---------------------------------------------------------------------------
// Actions and exports

#[sqlx::test(migrations = "tests/migrations")]
async fn actions_and_exports_receive_the_selected_uuid_keys(db: PgPool) {
    let ann = author(&db, "Ann").await;
    let one = book(&db, "One", "novel", ann, None).await;
    let two = book(&db, "Two", "novel", ann, None).await;
    let three = book(&db, "Three", "poetry", ann, None).await;
    let root = Client::new(app(db.clone(), SignIn::Password, true), Some("root"));
    let (one_text, two_text) = (one.to_string(), two.to_string());

    let ran = root
        .post(
            "/admin/library/book/?genre__exact=novel",
            &[
                ("action", "make_classic"),
                ("_selected_action", &one_text),
                ("_selected_action", &two_text),
                ("_selected_action", "not-a-uuid"),
            ],
        )
        .await;
    assert_eq!(ran.status, StatusCode::SEE_OTHER);
    assert_eq!(ran.location(), "/admin/library/book/?genre__exact=novel");
    assert_eq!(
        ran.flash(),
        Some((
            "success".to_string(),
            "Marked 2 books as classic.".to_string()
        ))
    );
    let classic: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM library_book WHERE genre = 'classic' ORDER BY id")
            .fetch_all(&db)
            .await
            .unwrap();
    let mut expected = vec![one, two];
    expected.sort();
    assert_eq!(classic, expected);
    let recorded = history(&db).await;
    assert_eq!(recorded.len(), 2);
    assert!(
        recorded
            .iter()
            .all(|(_, _, _, message)| message == "Applied action: Mark as classic.")
    );

    let clerk = Client::new(app(db.clone(), SignIn::Password, true), Some("reader"));
    let refused = clerk
        .post(
            "/admin/library/book/",
            &[("action", "make_classic"), ("_selected_action", &one_text)],
        )
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    let three_text = three.to_string();
    let exported = root
        .post(
            "/admin/library/book/",
            &[
                ("action", "export_keys"),
                ("_selected_action", &three_text),
                ("_selected_action", &one_text),
            ],
        )
        .await;
    assert_eq!(exported.status, StatusCode::OK);
    assert_eq!(
        exported.headers[header::CONTENT_DISPOSITION],
        "attachment; filename=\"keys.txt\""
    );
    let mut keys = [one_text.clone(), three_text.clone()];
    keys.sort();
    assert_eq!(exported.body, keys.join("\n"));
}

// ---------------------------------------------------------------------------
// Routing, the optional history and host pages

#[sqlx::test(migrations = "tests/migrations")]
async fn every_page_answers_with_and_without_the_trailing_slash(db: PgPool) {
    let ann = author(&db, "Ann").await;
    let dune = book(&db, "Dune", "novel", ann, None).await;
    let root = Client::new(app(db, SignIn::Password, true), Some("root"));
    for path in [
        "/admin".to_string(),
        "/admin/library".to_string(),
        "/admin/library/book".to_string(),
        "/admin/library/book/add".to_string(),
        format!("/admin/library/book/{dune}/change"),
        format!("/admin/library/book/{dune}/delete"),
        format!("/admin/library/book/{dune}/history"),
        "/admin/login".to_string(),
        "/admin/reports".to_string(),
        "/admin/static/admin.css".to_string(),
        "/admin/static/admin.js".to_string(),
    ] {
        for uri in [path.clone(), format!("{path}/")] {
            let reply = root.get(&uri).await;
            let expected = if path == "/admin/login" {
                StatusCode::SEE_OTHER
            } else {
                StatusCode::OK
            };
            assert_eq!(reply.status, expected, "{uri}");
        }
    }
    let redirect = root.get(&format!("/admin/library/book/{dune}")).await;
    assert_eq!(
        redirect.location(),
        format!("/admin/library/book/{dune}/change/")
    );
    assert_eq!(
        root.get("/admin/library/nothing/").await.status,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrations = "tests/migrations")]
async fn responses_carry_the_security_headers(db: PgPool) {
    let root = Client::new(app(db.clone(), SignIn::Password, true), Some("root"));
    let anonymous = Client::new(app(db, SignIn::None, true), None);
    for reply in [
        root.get("/admin/library/book/").await,
        root.get("/admin/library/nothing/").await,
        anonymous.get("/admin/").await,
    ] {
        let header = |name: &str| {
            reply
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string()
        };
        let policy = header("content-security-policy");
        assert!(policy.starts_with("default-src 'none';"), "{policy}");
        assert!(policy.contains("script-src 'self'"), "{policy}");
        assert!(policy.contains("frame-ancestors 'none'"), "{policy}");
        assert_eq!(header("x-frame-options"), "DENY");
        assert_eq!(header("x-content-type-options"), "nosniff");
    }
    // The list's script is a file the policy allows, not inline.
    let list = root.get("/admin/library/book/").await;
    assert!(
        list.body
            .contains("<script src=\"/admin/static/admin.js\" defer></script>")
    );
    assert!(!list.body.contains("<script>"));
}

#[sqlx::test(migrations = "tests/migrations")]
async fn without_a_history_hook_there_is_no_history(db: PgPool) {
    let ann = author(&db, "Ann").await;
    let dune = book(&db, "Dune", "novel", ann, None).await;
    let root = Client::new(app(db.clone(), SignIn::Password, false), Some("root"));

    let index = root.get("/admin/").await;
    assert_eq!(index.status, StatusCode::OK);
    assert!(!index.body.contains("Recent actions"));
    let page = root
        .get(&format!("/admin/library/book/{dune}/change/"))
        .await;
    assert!(!page.body.contains("/history/"));
    assert_eq!(
        root.get(&format!("/admin/library/book/{dune}/history/"))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let ann_text = ann.to_string();
    let added = root
        .post(
            "/admin/library/book/add/",
            &[("title", "Emma"), ("genre", "novel"), ("author", &ann_text)],
        )
        .await;
    assert_eq!(added.status, StatusCode::SEE_OTHER, "{}", added.body);
    let ran = root
        .post(
            "/admin/library/book/",
            &[
                ("action", "make_classic"),
                ("_selected_action", &dune.to_string()),
            ],
        )
        .await;
    assert_eq!(ran.status, StatusCode::SEE_OTHER);
    assert!(history(&db).await.is_empty());
}

#[sqlx::test(migrations = "tests/migrations")]
async fn host_pages_get_the_console_chrome_and_checks(db: PgPool) {
    let root = Client::new(app(db.clone(), SignIn::Password, true), Some("root"));
    let index = root.get("/admin/").await;
    assert!(
        index
            .body
            .contains("<a href=\"/admin/reports/\">Reports</a>")
    );

    let page = root.get("/admin/reports/").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("<p id=\"greeting\">Hello root</p>"));
    assert!(
        page.body
            .contains("<title>Reports | Library administration</title>")
    );
    assert!(page.body.contains("<a href=\"/admin/\">Library</a>"));
    assert!(page.body.contains("&rsaquo; Reports"));

    let posted = root.post("/admin/reports/", &[]).await;
    assert_eq!(posted.status, StatusCode::SEE_OTHER);
    assert_eq!(
        posted.flash(),
        Some(("success".to_string(), "Report queued.".to_string()))
    );

    let anonymous = Client::new(app(db.clone(), SignIn::Password, true), None);
    let reply = anonymous.get("/admin/reports").await;
    assert_eq!(reply.location(), "/admin/login/?next=%2Fadmin%2Freports");

    let visitor = Client::new(app(db, SignIn::Url, true), Some("visitor"));
    assert_eq!(
        visitor.get("/admin/reports/").await.status,
        StatusCode::FORBIDDEN
    );
}
