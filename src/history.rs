//! The optional [`History`] hook: what the console records after each add,
//! change, delete and action, and reads back for "Recent actions" on the
//! index and each object's history page (Django's `LogEntry`).

use chrono::{DateTime, Utc};
use sqlx::PgConnection;

use crate::{
    key::Key,
    registry::{BoxFuture, Model},
};

/// What happened to an object.
#[derive(Clone, Copy, Debug)]
pub enum Event<'a> {
    Added,
    /// Saved from its change form; the labels of the fields that changed
    /// (empty when none did).
    Changed(&'a [String]),
    Deleted,
    /// A changelist action ran on it; its label.
    Action(&'a str),
}

/// One thing to record, in the console's transaction.
#[derive(Clone, Copy)]
pub struct Change<'a> {
    /// [`crate::StaffMember::id`] of who did it.
    pub staff_id: &'a str,
    pub model: &'a Model,
    pub key: &'a Key,
    /// The object's text at the time.
    pub repr: &'a str,
    pub event: Event<'a>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Added,
    Changed,
    Deleted,
}

/// A recorded entry, read back.
#[derive(Clone, Debug)]
pub struct Entry {
    pub at: DateTime<Utc>,
    /// The name of who did it.
    pub staff_name: String,
    /// App label and model name, when the entry names a model.
    pub app: Option<String>,
    pub model: Option<String>,
    /// The object's key as text.
    pub key: Option<String>,
    pub repr: String,
    pub kind: EntryKind,
    /// What a change entry says (the history page's "Action" column):
    /// "Changed Title and Author.". Added and deleted entries show "Added."
    /// and "Deleted.".
    pub message: String,
}

/// Where the console's history lives (Django's `django_admin_log`).
pub trait History: Send + Sync + 'static {
    /// Records one change. `db` is the transaction that made it.
    fn record<'a>(
        &'a self,
        db: &'a mut PgConnection,
        change: Change<'a>,
    ) -> BoxFuture<'a, anyhow::Result<()>>;

    /// The staff member's latest entries, newest first, at most `limit`.
    fn recent<'a>(
        &'a self,
        db: &'a mut PgConnection,
        staff_id: &'a str,
        limit: i64,
    ) -> BoxFuture<'a, anyhow::Result<Vec<Entry>>>;

    /// An object's entries, oldest first, at most `limit`.
    fn object<'a>(
        &'a self,
        db: &'a mut PgConnection,
        model: &'a Model,
        key: &'a Key,
        limit: i64,
    ) -> BoxFuture<'a, anyhow::Result<Vec<Entry>>>;
}
