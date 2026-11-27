# axum-admin-console

A Django-style admin console for [axum](https://github.com/tokio-rs/axum)
services on Postgres with [sqlx](https://github.com/launchbadge/sqlx):
server-rendered change lists (search, filters, sorting, paging, date
hierarchy, actions, exports), add, change and delete pages with
validation, Django's delete collector (cascade, set null, protect), inlines,
many-to-many fields, computed fields, an index with recent actions, and
per-object history. It renders plain HTML with one stylesheet and a
dozen lines of script, uses no template engine, and keeps nothing in memory
between requests.

The models are `static` declarations, like a Django `ModelAdmin`. Your
service keeps everything about users: who is signed in, what they may do,
how they sign in and where history is stored come from hooks you implement.
Keys can be integers, UUIDs or text.

Rules the console keeps, and that every registration keeps too:

- Table and column names come only from the static declarations. Request
  values are always bound parameters, never part of the SQL text; keys are
  bound as text and cast to their column type.
- SQL fragments use `{t}` for the model's own row; the console replaces it
  with an alias.
- No page loads a whole table: lists are paged, filter and select options
  are capped, and a foreign key to a table of more than 100 rows is edited
  as a key input.

## Mounting it

```rust
use axum::{Router, http::request::Parts};
use axum_admin_console::{
    Auth, Console, Identity, KeyKind, OnDelete, Perms, Relation, Schema, StaffMember,
    registry::{App, BoxFuture, Column, Field, Filter, Model, Path, Sort},
};

static AUTHOR: Model = Model::new("library", "author", "library_author", "author", "authors")
    .repr("{t}.name") // Django's __str__, as SQL (default: "author object (<key>)")
    .fields(&[Field::char("name", "Name", 100)])
    .list_display(&[Column::field("name")])
    .search(&[Path::col("name")])
    .ordering(&[Sort::asc(Path::col("name"))]);

static BOOK: Model = Model::new("library", "book", "library_book", "book", "books")
    .key(KeyKind::Uuid) // `id uuid PRIMARY KEY`
    .repr("{t}.title")
    .fields(&[
        Field::char("title", "Title", 200),
        Field::fk("author", "author_id", "Author", "library_author"),
        Field::date("published", "Published").null(),
    ])
    .list_display(&[Column::field("title"), Column::field("author")])
    .list_filter(&[Filter::Field("author"), Filter::Field("published")])
    .search(&[Path::col("title"), Path::via(&[("author_id", "library_author")], "name")]);

static LIBRARY: App = App { label: "library", name: "Library", models: &[&AUTHOR, &BOOK] };

/// Every foreign key with its `on_delete` rule: `(table, column, referenced table, rule)`.
static RELATIONS: &[Relation] = &[("library_book", "author_id", "library_author", OnDelete::Cascade)];

static SCHEMA: Schema = Schema::new(&[&LIBRARY]).delete_rules(RELATIONS, &[]);

/// Who is asking: here a hypothetical session lookup of your own.
struct MyAuth;

impl Auth for MyAuth {
    fn identify<'a>(&'a self, parts: &'a Parts) -> BoxFuture<'a, Identity> {
        Box::pin(async move {
            match my_session_user(parts).await {
                None => Identity::Anonymous,
                Some(user) if !user.is_staff => Identity::NotStaff { name: user.name },
                Some(user) => Identity::Staff(StaffMember {
                    id: user.id.to_string(),
                    name: user.name,
                    perms: if user.is_superuser {
                        Perms::all()
                    } else {
                        // (app label, codename): "view_book", "change_book", ...
                        Perms::granted(user.permissions)
                    },
                }),
            }
        })
    }
}

fn routes(db: sqlx::PgPool) -> Router<MyState> {
    let console = Console::builder(db, &SCHEMA, MyAuth)
        .sign_in_url("/sign-in", "next") // or .password_sign_in(..), see below
        .sign_out_url("/sign-out")
        .site_title("Library administration")
        .build();
    Router::new()
        .merge(console.router()) // a Router<S> for any state type S
        // .merge(the rest of your service)
}
```

`console.router()` answers every page both as `/admin/x` and `/admin/x/`,
so it works with or without a trailing-slash normaliser in front of it. The
console is mounted at `/admin`; the path is fixed (`axum_admin_console::BASE`).

`Schema` is a `static` value so that code outside a request (an action, a
job) can build the same delete plans as the pages:
`delete::Plan::new(&SCHEMA, &BOOK)?.execute(&mut db, &keys)`. Call
`SCHEMA.check()` in a unit test: it builds every model's delete plan and
fails on a cycle of cascades.

## Keys

`Model::key(KeyKind::Int | KeyKind::Uuid | KeyKind::Text)` declares the type
of the primary key (`.pk("column")` names it; the default is an integer
`id`). Hooks receive keys as `Key`:

```rust
pub enum Key { Int(i64), Uuid(uuid::Uuid), Text(String) }
```

with `key.int()?`, `key.as_uuid()`, `Key::ints(&keys)`, `Key::uuids(&keys)`
and `Key::texts(&keys)` for binding. A key in a URL is percent-encoded; a
key of the wrong type is a page that does not exist.

A foreign key points at its target table's key: the key of the first
registered model stored in that table. For a table no model is registered
for, declare it with `Schema::tables(&[("table", "column", KeyKind::Text)])`;
any other table is keyed by `Schema::default_key` (`id`, integer).

## Hooks

### `Auth` (required)

`identify(&Parts) -> Identity`: `Staff(StaffMember { id, name, perms })`,
`Anonymous`, `NotStaff { name }` (signed in, but refused with Django's
message) or `Unavailable(error)` (a server error).

`Perms::all()` is a superuser; `Perms::granted(Vec<(app_label, codename)>)`
has Django's meaning: `view_<model>`, `add_<model>`, `change_<model>`,
`delete_<model>`; change implies view; any permission in an app shows the
app.

### Signing in

- `.password_sign_in(impl PasswordSignIn)`: the console serves
  `/admin/login/` and `/admin/logout/` (Django's pages) and asks your hook
  to check the credentials (`SignInOutcome::SignedIn(set-cookie headers)`,
  `Refused`, `Throttled`, `Unavailable`) and to sign out. Rate limits,
  sessions and cookies stay yours. Unauthenticated requests are redirected
  to `/admin/login/?next=...`.
- `.sign_in_url(url, param)`: unauthenticated requests are redirected to
  your page with the return path in `param`; `.sign_out_url(url)` makes "Log
  out" a link.
- `.sign_in_page(url)`: the same redirect without a return path, for a
  sign-in page that cannot come back to the console.
- Neither: unauthenticated requests get a 403 page.

### `History` (optional)

`record(db, Change)` runs inside the console's transaction for every add
(`Event::Added`), change (`Event::Changed(field labels)`), delete and action
(`Event::Action(label)`); `recent(db, staff_id, limit)` feeds "Recent
actions" on the index; `object(db, model, key, limit)` feeds the history
page. Without it there is no "Recent actions" block, no history page and no
History button. A Django host can write `django_admin_log` from it.

### `Passwords` (optional)

For `Kind::Password` fields: `hash(raw)` stores the add form's password
pair (Django's `UserCreationForm` names `password1` and `password2`),
`summary(stored)` describes a stored hash (Django's summary by default),
`change_url(model, key)` links to your own set-password page.

### Host pages

Pages of your own inside the console, staff only, with its chrome:

```rust
use axum_admin_console::{AdminResult, Form, Pages, Staff, html::esc};

async fn report(staff: Staff) -> AdminResult<axum::response::Response> {
    let content = format!("<p>Hello {}</p>", esc(&staff.name));
    Ok(staff.page("Report", &[("Report".into(), String::new())], "report", &content))
}

async fn rerun(staff: Staff, headers: axum::http::HeaderMap, form: Form) -> AdminResult<axum::response::Response> {
    staff.check_csrf(&headers, &form)?;
    Ok(staff.redirect("/admin/report/", "success", "Report queued."))
}

let router = console.router_with(Pages::new().route("/admin/report", get(report).post(rerun)));
```

`Staff` is the extractor: it refuses whoever is not staff the way the
console's pages do, and carries `id`, `name`, `perms`, the pending flash
messages and the CSRF token (`staff.token()` for a form's
`csrfmiddlewaretoken`). `staff.page` renders the chrome with breadcrumbs and
messages, `staff.redirect` redirects with a flash message,
`staff.console()` gives the registered models, `load`, `crumbs` and the
time zone. Link to the page from the index with
`.index_link(label, url, |perms| ...)`. Host handlers keep their own state
(`State<MyState>`).

### Unknown console paths

A `/admin/...` path that no console route matches, or a method its route
does not take, reaches the host's fallback. To answer it with the console's
404 page (with its security headers) instead of the host's own:

```rust
let console = console.clone();
let app = Router::new()
    .merge(console.router())
    .fallback(move |req: axum::extract::Request| {
        let console = console.clone();
        async move {
            if req.uri().path() == "/admin" || req.uri().path().starts_with("/admin/") {
                return console.error_page(AdminError::NotFound);
            }
            host_fallback(req).await
        }
    });
```

Set the same handler as `method_not_allowed_fallback`. `error_page` renders
any `AdminError` the way the console's own pages do.

## Settings

| Builder method | Default | |
| --- | --- | --- |
| `site_title`, `site_header` | `Administration` | `<title>` suffix and the header |
| `time_zone(Tz)` | UTC | datetimes are shown and edited in it |
| `secure_cookies(bool)` | `false` | `Secure` on the CSRF and message cookies |
| `csrf_cookie(name)` | `csrftoken` | Django's format, so a Django site can share it |
| `report_errors(Fn(anyhow::Error, &mut Response))` | puts `InternalError` in the response's extensions | for your request logging |

Every console response also carries a `Content-Security-Policy` (only the
console's own stylesheet and script, forms post to this host, no framing),
`X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff` and
`Referrer-Policy: same-origin`. A host page that sets one of these headers
itself keeps its own value.

## The registry

Field kinds: `char`, `text`, `int`, `decimal`, `float`, `bool`, `datetime`,
`utc_timestamp`, `date`, `duration`, `json`,
`choice(name, label, &[(value, label)])`, `fk(name, column, label, table)`
and `Kind::Password`, with `.null()`,
`.blank()`, `.default(..)`, `.default_sql(..)`, `.default_gen(..)`,
`.help(..)`, `.auto_now_add()`, `.auto_now()`. Also on `Model`:
`.fieldsets`, `.add_fieldsets`, `.readonly`, `.m2m`, `.computed`, `.inlines`,
`.per_page`, `.date_hierarchy`, `.after_save` (code Django runs in `save()`
or a signal), `.create_with` (Django's `save_model` taking over an add),
`.no_add()`, `.no_change()`, `.no_delete()`, `.no_delete_selected()`,
`.count_limit(n)`.

- `datetime` is a `timestamptz`; `utc_timestamp` is a
  `timestamp without time zone` holding UTC (what Ecto's `:utc_datetime` and
  many ORMs store). Both are shown, filtered and edited in the console's time
  zone, whatever the database session's time zone is.
- A `char` field can show any column through a text cast: a uuid that is not
  a key, an array, a Postgres enum. The forms cannot write such columns, so
  declare them `.readonly`, or give the model no add and no change. A
  `choice` filter compares as text, so it works over an enum column too
  (shown read-only). `bytea` has no kind; leave such columns out.
- `.count_limit(n)` stops the change list counting at `n` rows: it shows
  "More than n" and pages only through the first `n`. Use it on tables too
  big to count on every page view.
- A model without change permission, or declared `.no_change()`, shows the
  change page read-only ("View …"), with no Save or Delete.

### Columns

```rust
Column::expr("Total", "{t}.price * {t}.copies", Kind::Decimal(2))
    .order_by(Path::col("price")),             // optional: makes it sortable
Column::path("Author", Path::via(&[("author_id", "library_author")], "name")),
```

`Column::field("x").label("Other label")` renames a field column. For a
computed field on the change page use `.computed(&[Computed { .. }])` with
`ComputedRender::Sql` or an async `ComputedRender::Html` function, which
gets the row's `Key`.

### Filters

```rust
.list_filter(&[
    Filter::Field("status"),     // choices, booleans, dates, foreign keys
    Filter::Related {            // book__author
        param: "book__author",
        label: "Author",
        path: Path::via(&[("book_id", "library_book")], "author_id"),
        target: "library_author",
    },
    Filter::Values {             // distinct values through foreign keys
        param: "book__language",
        label: "Language",
        path: Path::via(&[("book_id", "library_book")], "language"),
    },
    Filter::Options {            // same, choices from a small table: for
        param: "book__series",   // large tables, where scanning every row
        label: "Series",         // for its values is too dear
        path: Path::via(&[("book_id", "library_book")], "series"),
        options_sql: "SELECT DISTINCT name, name FROM library_series ORDER BY 1",
    },
])
```

### Actions

```rust
use axum_admin_console::{Key, registry::{Action, BoxFuture, Refusal}};

fn mark_out_of_print<'a>(db: &'a mut sqlx::PgConnection, keys: &'a [Key]) -> BoxFuture<'a, anyhow::Result<String>> {
    Box::pin(async move {
        let done = sqlx::query("UPDATE library_book SET in_print = false WHERE id = ANY($1)")
            .bind(Key::uuids(keys))
            .execute(&mut *db)
            .await?;
        if done.rows_affected() == 0 {
            return Err(Refusal("Nothing to update".into()).into());
        }
        Ok(format!("{} books marked out of print.", done.rows_affected()))
    })
}

.actions(&[Action { name: "mark_out_of_print", label: "Mark selected books out of print", run: mark_out_of_print }])
```

An action runs in one transaction with the keys of the selected rows that
exist (at most 1000), needs the model's change permission, and records a
history entry for each row. A `Refusal` rolls it back and shows its message
as an error. "Delete selected" is built in.

### Exports

A Django action that returns a file is an `Export`: it gets the selected
keys, returns a `Download { filename, content_type, body }`, needs only the
view permission and records nothing.

### Deleting

Deletes follow Django's collector over the rules given to
`Schema::delete_rules(relations, also_delete)`: every foreign key with
`OnDelete::Cascade`, `SetNull` or `Protect`, plus extra deletions
(`(deleted table, table also deleted from, condition)`, where `{parent}` in
the condition is a query selecting the deleted keys) for what a Django
`post_delete` receiver deleted. Without rules a delete removes just the
row. The confirmation page lists what goes, and refuses when a protected
row or a model the user may not delete is in the way.

## What is not there

- Users, groups, permissions tables, sessions, password checks and login
  rate limits: these are the host's, behind the hooks.
- Editable inlines and formsets (inlines are read-only), autocomplete
  widgets, raw-id popups, list editing and saving as new.
- Translations: the pages are in English, worded like Django's.
- A configurable mount path: the console lives at `/admin`.
- Templates or bundled assets beyond `admin.css` and `admin.js` (a dozen
  lines: the list's "select all" box).

## Development

The tests run against Postgres: point `DATABASE_URL` at a server whose role
may create databases (each test gets its own).

```sh
DATABASE_URL=postgres://user:password@localhost/postgres cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```
