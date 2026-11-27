//! The delete rules a host hands to the console: every foreign key with
//! the `on_delete` it should follow, and the extra deletions its code makes
//! (Django's `post_delete` receivers). Without them, deleting a row deletes
//! just that row and the database's own constraints decide the rest.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnDelete {
    /// The referring rows are deleted too.
    Cascade,
    /// The reference is cleared.
    SetNull,
    /// The delete is refused while referring rows exist.
    Protect,
}

/// `(table, column, referenced table, rule)`: `table.column` refers to the
/// referenced table's key column.
pub type Relation = (&'static str, &'static str, &'static str, OnDelete);

/// `(deleted table, table also deleted from, condition)`. In the condition
/// `{parent}` is an SQL query selecting the keys of the deleted rows:
/// `("author", "author_note", "author_id IN ({parent})")`.
pub type AlsoDelete = (&'static str, &'static str, &'static str);
