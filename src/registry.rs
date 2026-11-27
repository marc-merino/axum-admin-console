//! The declarative description of each admin page, mirroring a Django
//! `ModelAdmin`. Everything here is `'static` data: table names, column
//! names and SQL fragments come only from these declarations, never from a
//! request. SQL fragments refer to the row's table through the `{t}`
//! placeholder (`"{t}.name"`), which the query builder replaces with an
//! alias of its own.

use std::{future::Future, pin::Pin};

use sqlx::PgConnection;

pub use crate::key::{Key, KeyKind};
use crate::query::Values;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// An app: a heading on the index with its models (a Django app).
pub struct App {
    /// `app_label`, as in the URL and in permissions.
    pub label: &'static str,
    /// The heading Django shows (`verbose_name` of the app config).
    pub name: &'static str,
    pub models: &'static [&'static Model],
}

/// How a value is stored, edited and shown.
#[derive(Clone, Copy, Debug)]
pub enum Kind {
    /// `CharField`: one line of text.
    Char,
    /// `TextField`: a text area.
    Text,
    /// Any integer column.
    Int,
    /// `DecimalField` with this many decimal places.
    Decimal(u32),
    /// `FloatField` (`double precision`).
    Float,
    Bool,
    /// `timestamptz`, shown and edited in the console's time zone.
    DateTime,
    /// `timestamp without time zone` holding UTC (what Ecto's
    /// `:utc_datetime` and many ORMs store), shown and edited in the
    /// console's time zone like [`Kind::DateTime`], whatever the database
    /// session's time zone.
    UtcTimestamp,
    Date,
    /// `DurationField` (`interval`), written like Django's `duration_string`.
    Duration,
    Json,
    /// A `CharField` with `choices`: `(stored value, label)`.
    Choice(&'static [(&'static str, &'static str)]),
    /// A foreign key to the table named here (to its key column, see
    /// [`crate::Schema::table_key`]).
    ForeignKey(&'static str),
    /// A password hash: never shown, summarised by the console's
    /// [`crate::Passwords`] hook (Django's summary by default).
    Password,
}

/// What an add form puts in a column the form does not set.
#[derive(Clone, Copy, Debug)]
pub enum Initial {
    None,
    /// A literal value, bound like a submitted one.
    Value(&'static str),
    /// An SQL expression (`now()`, `'{}'::jsonb`).
    Sql(&'static str),
    /// A value computed in Rust at insert time (random keys).
    Gen(fn() -> String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Auto {
    None,
    /// `auto_now_add`: `now()` on insert, never edited.
    OnAdd,
    /// `auto_now`: `now()` on every save, never edited.
    OnSave,
}

/// One model field (one column).
#[derive(Clone, Copy, Debug)]
pub struct Field {
    /// Django's field name: form key, filter parameter, fieldset entry.
    pub name: &'static str,
    pub column: &'static str,
    /// `verbose_name`, capitalised as Django shows it.
    pub label: &'static str,
    pub kind: Kind,
    pub null: bool,
    pub blank: bool,
    pub default: Initial,
    pub auto: Auto,
    pub help: &'static str,
    pub max_length: Option<u32>,
}

impl Field {
    pub const fn new(name: &'static str, label: &'static str, kind: Kind) -> Self {
        Self {
            name,
            column: name,
            label,
            kind,
            null: false,
            blank: false,
            default: Initial::None,
            auto: Auto::None,
            help: "",
            max_length: None,
        }
    }
    pub const fn char(name: &'static str, label: &'static str, max_length: u32) -> Self {
        let mut field = Self::new(name, label, Kind::Char);
        field.max_length = Some(max_length);
        field
    }
    pub const fn text(name: &'static str, label: &'static str) -> Self {
        Self::new(name, label, Kind::Text)
    }
    pub const fn int(name: &'static str, label: &'static str) -> Self {
        Self::new(name, label, Kind::Int)
    }
    pub const fn decimal(name: &'static str, label: &'static str, places: u32) -> Self {
        Self::new(name, label, Kind::Decimal(places))
    }
    pub const fn float(name: &'static str, label: &'static str) -> Self {
        Self::new(name, label, Kind::Float)
    }
    pub const fn bool(name: &'static str, label: &'static str) -> Self {
        Self::new(name, label, Kind::Bool).default("false")
    }
    pub const fn datetime(name: &'static str, label: &'static str) -> Self {
        Self::new(name, label, Kind::DateTime)
    }
    /// A `timestamp without time zone` column holding UTC.
    pub const fn utc_timestamp(name: &'static str, label: &'static str) -> Self {
        Self::new(name, label, Kind::UtcTimestamp)
    }
    pub const fn date(name: &'static str, label: &'static str) -> Self {
        Self::new(name, label, Kind::Date)
    }
    pub const fn duration(name: &'static str, label: &'static str) -> Self {
        Self::new(name, label, Kind::Duration)
    }
    pub const fn json(name: &'static str, label: &'static str) -> Self {
        Self::new(name, label, Kind::Json)
    }
    pub const fn choice(
        name: &'static str,
        label: &'static str,
        choices: &'static [(&'static str, &'static str)],
    ) -> Self {
        Self::new(name, label, Kind::Choice(choices))
    }
    /// A foreign key `name` stored in `column` (`author`, `author_id`).
    pub const fn fk(
        name: &'static str,
        column: &'static str,
        label: &'static str,
        table: &'static str,
    ) -> Self {
        let mut field = Self::new(name, label, Kind::ForeignKey(table));
        field.column = column;
        field
    }
    pub const fn column(mut self, column: &'static str) -> Self {
        self.column = column;
        self
    }
    /// `null=True, blank=True`.
    pub const fn null(mut self) -> Self {
        self.null = true;
        self.blank = true;
        self
    }
    /// `blank=True`: may be left empty (stored as `''` unless nullable).
    pub const fn blank(mut self) -> Self {
        self.blank = true;
        self
    }
    pub const fn default(mut self, value: &'static str) -> Self {
        self.default = Initial::Value(value);
        self
    }
    pub const fn default_sql(mut self, sql: &'static str) -> Self {
        self.default = Initial::Sql(sql);
        self
    }
    pub const fn default_gen(mut self, generate: fn() -> String) -> Self {
        self.default = Initial::Gen(generate);
        self
    }
    pub const fn auto_now_add(mut self) -> Self {
        self.auto = Auto::OnAdd;
        self
    }
    pub const fn auto_now(mut self) -> Self {
        self.auto = Auto::OnSave;
        self
    }
    pub const fn help(mut self, help: &'static str) -> Self {
        self.help = help;
        self
    }
    pub const fn max_length(mut self, max_length: u32) -> Self {
        self.max_length = Some(max_length);
        self
    }
    pub(crate) fn editable(&self) -> bool {
        self.auto == Auto::None
    }
}

/// A value reached from the row, possibly through foreign keys: Django's
/// `author__name`. Each hop is `(foreign key column, table it points to)`;
/// the foreign key refers to the target table's key column.
#[derive(Clone, Copy, Debug)]
pub struct Path {
    pub hops: &'static [(&'static str, &'static str)],
    pub column: &'static str,
}

impl Path {
    /// A column of the row itself.
    pub const fn col(column: &'static str) -> Self {
        Self { hops: &[], column }
    }
    pub const fn via(hops: &'static [(&'static str, &'static str)], column: &'static str) -> Self {
        Self { hops, column }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Sort {
    pub path: Path,
    pub desc: bool,
}

impl Sort {
    pub const fn asc(path: Path) -> Self {
        Self { path, desc: false }
    }
    pub const fn desc(path: Path) -> Self {
        Self { path, desc: true }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ColumnSource {
    /// A model field (`list_display = ('title', ...)`); a foreign key shows
    /// the target's text.
    Field(&'static str),
    /// A computed column: an SQL expression shown as `kind`.
    Expr(&'static str, Kind),
    /// A value through foreign keys, shown as text.
    Path(Path),
}

/// One `list_display` entry.
#[derive(Clone, Copy, Debug)]
pub struct Column {
    pub label: &'static str,
    pub label_fn: Option<fn() -> String>,
    pub source: ColumnSource,
    /// `admin_order_field`; field columns sort by their column.
    pub order: Option<Path>,
    /// `admin_order_field` as SQL on `{t}` (an annotation).
    pub order_sql: Option<&'static str>,
}

impl Column {
    /// The field's own label and type.
    pub const fn field(name: &'static str) -> Self {
        Self {
            label: "",
            label_fn: None,
            source: ColumnSource::Field(name),
            order: None,
            order_sql: None,
        }
    }
    pub const fn expr(label: &'static str, sql: &'static str, kind: Kind) -> Self {
        Self {
            label,
            label_fn: None,
            source: ColumnSource::Expr(sql, kind),
            order: None,
            order_sql: None,
        }
    }
    /// A value through foreign keys, sortable by it.
    pub const fn path(label: &'static str, path: Path) -> Self {
        Self {
            label,
            label_fn: None,
            source: ColumnSource::Path(path),
            order: Some(path),
            order_sql: None,
        }
    }
    pub const fn label(mut self, label: &'static str) -> Self {
        self.label = label;
        self
    }
    /// A label computed when the page renders (`"Sales in 2026"`).
    pub const fn label_fn(mut self, label: fn() -> String) -> Self {
        self.label_fn = Some(label);
        self
    }
    pub const fn order_by(mut self, path: Path) -> Self {
        self.order = Some(path);
        self
    }
    pub const fn order_sql(mut self, sql: &'static str) -> Self {
        self.order_sql = Some(sql);
        self
    }
}

/// One `list_filter` entry.
#[derive(Clone, Copy, Debug)]
pub enum Filter {
    /// A field: booleans and choices list their values, dates offer Django's
    /// ranges, foreign keys list the target rows, other fields their
    /// distinct values.
    Field(&'static str),
    /// A foreign key reached through others (`book__author`): `path`
    /// ends at the foreign key column, `target` is the table it points to.
    Related {
        param: &'static str,
        label: &'static str,
        path: Path,
        target: &'static str,
    },
    /// Distinct values of a column reached through foreign keys
    /// (`author__country`).
    Values {
        param: &'static str,
        label: &'static str,
        path: Path,
    },
    /// Like `Values`, but the choices come from `options_sql` (`SELECT
    /// value, label ...`, capped) instead of the model's own rows: for large
    /// tables, where scanning every row for its distinct values is too dear.
    Options {
        param: &'static str,
        label: &'static str,
        path: Path,
        options_sql: &'static str,
    },
}

/// A `fieldsets` entry.
#[derive(Clone, Copy, Debug)]
pub struct Fieldset {
    pub name: Option<&'static str>,
    /// Field names: model fields, many-to-many fields or computed fields.
    pub fields: &'static [&'static str],
    pub collapse: bool,
    pub description: &'static str,
}

impl Fieldset {
    pub const fn new(name: Option<&'static str>, fields: &'static [&'static str]) -> Self {
        Self {
            name,
            fields,
            collapse: false,
            description: "",
        }
    }
    pub const fn collapse(mut self) -> Self {
        self.collapse = true;
        self
    }
    pub const fn description(mut self, description: &'static str) -> Self {
        self.description = description;
        self
    }
}

/// A many-to-many field edited as a multiple select (`filter_horizontal`).
#[derive(Clone, Copy, Debug)]
pub struct ManyToMany {
    pub name: &'static str,
    pub label: &'static str,
    /// The through table and its two columns.
    pub through: &'static str,
    pub own: &'static str,
    pub other: &'static str,
    /// The table `other` points to.
    pub target: &'static str,
    /// `SELECT key, label ... ORDER BY ...` listing the choices; it is capped.
    pub options_sql: &'static str,
    pub help: &'static str,
}

/// A read-only value on the change page that is not a column.
#[derive(Clone, Copy)]
pub struct Computed {
    pub name: &'static str,
    pub label: &'static str,
    pub render: ComputedRender,
}

/// HTML for a computed field of the row with this key. The returned string
/// is inserted as it is: escape every value with [`crate::html::esc`].
pub type ComputedFn =
    for<'a> fn(&'a mut PgConnection, Key) -> BoxFuture<'a, anyhow::Result<String>>;

#[derive(Clone, Copy)]
pub enum ComputedRender {
    /// An SQL expression on the row, shown as `kind`.
    Sql(&'static str, Kind),
    Html(ComputedFn),
}

/// A related table shown read-only under the form (Django's inlines).
#[derive(Clone, Copy, Debug)]
pub struct Inline {
    pub label: &'static str,
    pub table: &'static str,
    /// Its column pointing at this row.
    pub fk: &'static str,
    /// `(label, SQL on {t}, kind)`.
    pub columns: &'static [(&'static str, &'static str, Kind)],
    /// SQL ordering on `{t}`.
    pub order: &'static str,
    /// `(column, table)`: the first cell links to the row this column of the
    /// inline row points to (when the inline table itself is not registered).
    /// `None`: the inline row itself, by its table's key column.
    pub link: Option<(&'static str, &'static str)>,
}

/// The keys of the selected rows given to an action.
pub type ActionFn =
    for<'a> fn(&'a mut PgConnection, &'a [Key]) -> BoxFuture<'a, anyhow::Result<String>>;

/// A changelist action (`actions = [...]`). It runs in one transaction with
/// the selected keys and returns the message shown afterwards; an error that
/// is a [`Refusal`] rolls it back and shows that message. Each selected row
/// gets a history entry (when the console keeps history).
#[derive(Clone, Copy)]
pub struct Action {
    pub name: &'static str,
    pub label: &'static str,
    pub run: ActionFn,
}

/// A refusal the staff user sees instead of a server error, like a Django
/// `ValidationError` or a message from an action: returned (through
/// `anyhow`) by a [`SaveHook`] or [`CreateHook`] it becomes a form error and
/// the save is rolled back; returned by an [`ActionFn`] it becomes an error
/// message and the action's transaction is rolled back.
#[derive(Debug)]
pub struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refusal {}

/// Django's `save_model` taking over an add: writes the new row itself from
/// the submitted values (instead of the plain INSERT) and returns its key.
/// `after_save` and the many-to-many writes still run afterwards.
pub type CreateHook =
    for<'a> fn(&'a mut PgConnection, &'a Values) -> BoxFuture<'a, anyhow::Result<Key>>;

/// A file an [`Export`] answers with.
pub struct Download {
    /// The attachment's file name (a plain name from the registry).
    pub filename: &'static str,
    pub content_type: &'static str,
    pub body: String,
}

/// The keys of the selected rows given to an export.
pub type ExportFn =
    for<'a> fn(&'a mut PgConnection, &'a [Key]) -> BoxFuture<'a, anyhow::Result<Download>>;

/// A changelist action that answers with a file instead of changing rows
/// (a Django action returning an `HttpResponse`, like a CSV export). It is
/// offered with the view permission, gets the selected keys (at most 1000)
/// and records no history.
#[derive(Clone, Copy)]
pub struct Export {
    pub name: &'static str,
    pub label: &'static str,
    pub run: ExportFn,
}

/// What a save changed, given to [`SaveHook`].
pub struct Saved<'a> {
    pub key: Key,
    pub created: bool,
    /// Field names whose value changed (all submitted fields on an add).
    pub changed: &'a [&'static str],
    /// The values before the save (`None` on an add), as text.
    pub old: Option<&'a Values>,
    pub new: &'a Values,
}

impl Saved<'_> {
    pub fn changed(&self, name: &str) -> bool {
        self.changed.contains(&name)
    }
}

/// Runs in the save's transaction after the row is written: what Django's
/// `save_model`, `save()` override and `post_save` receivers did.
pub type SaveHook =
    for<'a> fn(&'a mut PgConnection, &'a Saved<'a>) -> BoxFuture<'a, anyhow::Result<()>>;

/// One registered model (Django's `ModelAdmin`).
pub struct Model {
    pub app: &'static str,
    /// Lower-case model name: URL segment and permission codename suffix.
    pub name: &'static str,
    pub verbose: &'static str,
    pub plural: &'static str,
    pub table: &'static str,
    /// Primary key column used in URLs.
    pub pk: &'static str,
    /// The type of `pk`.
    pub key: KeyKind,
    /// `__str__` as SQL on `{t}`.
    pub repr: &'static str,
    pub fields: &'static [Field],
    pub m2m: &'static [ManyToMany],
    pub computed: &'static [Computed],
    /// Empty: one unnamed fieldset with every editable field.
    pub fieldsets: &'static [Fieldset],
    /// Empty: the change form's fieldsets.
    pub add_fieldsets: &'static [Fieldset],
    pub readonly: &'static [&'static str],
    /// Empty: the object's text, like Django's `__str__` column.
    pub list_display: &'static [Column],
    pub list_filter: &'static [Filter],
    pub search: &'static [Path],
    /// Empty: newest primary key first.
    pub ordering: &'static [Sort],
    pub per_page: i64,
    /// Count at most this many rows for the change list (see
    /// [`Model::count_limit`]); `None` counts them all.
    pub count_limit: Option<i64>,
    pub date_hierarchy: Option<&'static str>,
    pub actions: &'static [Action],
    pub exports: &'static [Export],
    pub inlines: &'static [Inline],
    pub after_save: Option<SaveHook>,
    /// Replaces the INSERT of an add (see [`CreateHook`]).
    pub create: Option<CreateHook>,
    pub can_add: bool,
    pub can_change: bool,
    pub can_delete: bool,
    /// Offer "Delete selected" (Django's `actions = None` removes it).
    pub delete_selected: bool,
}

impl Model {
    pub const fn new(
        app: &'static str,
        name: &'static str,
        table: &'static str,
        verbose: &'static str,
        plural: &'static str,
    ) -> Self {
        Self {
            app,
            name,
            verbose,
            plural,
            table,
            pk: "id",
            key: KeyKind::Int,
            repr: "",
            fields: &[],
            m2m: &[],
            computed: &[],
            fieldsets: &[],
            add_fieldsets: &[],
            readonly: &[],
            list_display: &[],
            list_filter: &[],
            search: &[],
            ordering: &[],
            per_page: 100,
            count_limit: None,
            date_hierarchy: None,
            actions: &[],
            exports: &[],
            inlines: &[],
            after_save: None,
            create: None,
            can_add: true,
            can_change: true,
            can_delete: true,
            delete_selected: true,
        }
    }
    pub const fn pk(mut self, pk: &'static str) -> Self {
        self.pk = pk;
        self
    }
    /// The type of the primary key (default: integer).
    pub const fn key(mut self, kind: KeyKind) -> Self {
        self.key = kind;
        self
    }
    pub const fn repr(mut self, sql: &'static str) -> Self {
        self.repr = sql;
        self
    }
    pub const fn fields(mut self, fields: &'static [Field]) -> Self {
        self.fields = fields;
        self
    }
    pub const fn m2m(mut self, m2m: &'static [ManyToMany]) -> Self {
        self.m2m = m2m;
        self
    }
    pub const fn computed(mut self, computed: &'static [Computed]) -> Self {
        self.computed = computed;
        self
    }
    pub const fn fieldsets(mut self, fieldsets: &'static [Fieldset]) -> Self {
        self.fieldsets = fieldsets;
        self
    }
    pub const fn add_fieldsets(mut self, fieldsets: &'static [Fieldset]) -> Self {
        self.add_fieldsets = fieldsets;
        self
    }
    pub const fn readonly(mut self, names: &'static [&'static str]) -> Self {
        self.readonly = names;
        self
    }
    pub const fn list_display(mut self, columns: &'static [Column]) -> Self {
        self.list_display = columns;
        self
    }
    pub const fn list_filter(mut self, filters: &'static [Filter]) -> Self {
        self.list_filter = filters;
        self
    }
    pub const fn search(mut self, paths: &'static [Path]) -> Self {
        self.search = paths;
        self
    }
    pub const fn ordering(mut self, ordering: &'static [Sort]) -> Self {
        self.ordering = ordering;
        self
    }
    pub const fn per_page(mut self, per_page: i64) -> Self {
        self.per_page = per_page;
        self
    }
    /// For large tables: the change list counts at most `limit` matching
    /// rows, says "More than `limit`" beyond that and pages through the
    /// first `limit` rows only (search and filters reach the others), so a
    /// page never counts the whole table.
    pub const fn count_limit(mut self, limit: i64) -> Self {
        self.count_limit = Some(limit);
        self
    }
    pub const fn date_hierarchy(mut self, field: &'static str) -> Self {
        self.date_hierarchy = Some(field);
        self
    }
    pub const fn actions(mut self, actions: &'static [Action]) -> Self {
        self.actions = actions;
        self
    }
    pub const fn exports(mut self, exports: &'static [Export]) -> Self {
        self.exports = exports;
        self
    }
    pub const fn inlines(mut self, inlines: &'static [Inline]) -> Self {
        self.inlines = inlines;
        self
    }
    pub const fn after_save(mut self, hook: SaveHook) -> Self {
        self.after_save = Some(hook);
        self
    }
    pub const fn create_with(mut self, hook: CreateHook) -> Self {
        self.create = Some(hook);
        self
    }
    pub const fn no_add(mut self) -> Self {
        self.can_add = false;
        self
    }
    pub const fn no_change(mut self) -> Self {
        self.can_change = false;
        self
    }
    pub const fn no_delete(mut self) -> Self {
        self.can_delete = false;
        self.delete_selected = false;
        self
    }
    pub const fn no_delete_selected(mut self) -> Self {
        self.delete_selected = false;
        self
    }

    pub fn field(&self, name: &str) -> Option<&'static Field> {
        self.fields.iter().find(|field| field.name == name)
    }
    pub fn m2m_field(&self, name: &str) -> Option<&'static ManyToMany> {
        self.m2m.iter().find(|m| m.name == name)
    }
    pub fn computed_field(&self, name: &str) -> Option<&'static Computed> {
        self.computed.iter().find(|c| c.name == name)
    }
    pub fn url(&self) -> String {
        format!("/admin/{}/{}/", self.app, self.name)
    }
    /// The change page of the row with this key.
    pub fn object_url(&self, key: impl Into<Key>) -> String {
        format!(
            "/admin/{}/{}/{}/change/",
            self.app,
            self.name,
            key.into().url_segment()
        )
    }
    /// The history page of the row with this key.
    pub fn history_url(&self, key: impl Into<Key>) -> String {
        format!(
            "/admin/{}/{}/{}/history/",
            self.app,
            self.name,
            key.into().url_segment()
        )
    }
    /// Django's `capfirst(verbose_name_plural)`.
    pub fn title_plural(&self) -> String {
        capfirst(self.plural)
    }
    pub fn title(&self) -> String {
        capfirst(self.verbose)
    }
}

pub fn capfirst(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}
