//! Primary keys. A model's key is an integer, a UUID or text; in URLs,
//! forms and hooks it travels as a [`Key`], and in SQL it is always bound as
//! text with a cast to its column type (`$1::bigint`, `$1::uuid`).

use std::fmt;

use uuid::Uuid;

/// The type of a key column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyKind {
    /// `bigint`, `integer` or `smallint` (Django's `AutoField`).
    Int,
    Uuid,
    /// `text` or `varchar`.
    Text,
}

impl KeyKind {
    /// The key written as `raw`, or `None` when it is not one of this kind.
    pub fn parse(self, raw: &str) -> Option<Key> {
        match self {
            Self::Int => raw.parse::<i64>().ok().map(Key::Int),
            Self::Uuid => Uuid::parse_str(raw).ok().map(Key::Uuid),
            Self::Text if raw.is_empty() => None,
            Self::Text => Some(Key::Text(raw.to_string())),
        }
    }

    /// The SQL type a bound key is cast to.
    pub fn sql_type(self) -> &'static str {
        match self {
            Self::Int => "bigint",
            Self::Uuid => "uuid",
            Self::Text => "text",
        }
    }

    /// `placeholder` cast to this key type (`$1::bigint`).
    pub fn cast(self, placeholder: &str) -> String {
        format!("{placeholder}::{}", self.sql_type())
    }

    /// `placeholder`, bound as a `text[]`, cast to an array of this key type.
    pub fn cast_array(self, placeholder: &str) -> String {
        format!("{placeholder}::{}[]", self.sql_type())
    }
}

/// One primary key value.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Key {
    Int(i64),
    Uuid(Uuid),
    Text(String),
}

impl Key {
    pub fn kind(&self) -> KeyKind {
        match self {
            Self::Int(_) => KeyKind::Int,
            Self::Uuid(_) => KeyKind::Uuid,
            Self::Text(_) => KeyKind::Text,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_uuid(&self) -> Option<Uuid> {
        match self {
            Self::Uuid(value) => Some(*value),
            _ => None,
        }
    }

    /// The integer key of an integer-keyed model, or an error for another kind.
    pub fn int(&self) -> anyhow::Result<i64> {
        self.as_i64()
            .ok_or_else(|| anyhow::anyhow!("key {self} is not an integer"))
    }

    /// The integers among `keys`: the keys of an integer-keyed model.
    pub fn ints(keys: &[Key]) -> Vec<i64> {
        keys.iter().filter_map(Key::as_i64).collect()
    }

    /// The UUIDs among `keys`: the keys of a UUID-keyed model.
    pub fn uuids(keys: &[Key]) -> Vec<Uuid> {
        keys.iter().filter_map(Key::as_uuid).collect()
    }

    /// Each key as text, for binding as a `text[]` cast with
    /// [`KeyKind::cast_array`].
    pub fn texts(keys: &[Key]) -> Vec<String> {
        keys.iter().map(Key::to_string).collect()
    }

    /// The key as one URL path segment, percent-encoded.
    pub fn url_segment(&self) -> String {
        let text = self.to_string();
        let mut out = String::with_capacity(text.len());
        for byte in text.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                out.push(byte as char);
            } else {
                out.push_str(&format!("%{byte:02X}"));
            }
        }
        out
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int(value) => write!(f, "{value}"),
            Self::Uuid(value) => write!(f, "{value}"),
            Self::Text(value) => f.write_str(value),
        }
    }
}

impl From<i64> for Key {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<Uuid> for Key {
    fn from(value: Uuid) -> Self {
        Self::Uuid(value)
    }
}

impl From<String> for Key {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&Key> for Key {
    fn from(value: &Key) -> Self {
        value.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_kind() {
        assert_eq!(KeyKind::Int.parse("42"), Some(Key::Int(42)));
        assert_eq!(KeyKind::Int.parse("4x"), None);
        let id = "67e55044-10b1-426f-9247-bb680e5fe0c8";
        assert_eq!(KeyKind::Uuid.parse(id).unwrap().to_string(), id);
        assert_eq!(KeyKind::Uuid.parse("42"), None);
        assert_eq!(KeyKind::Text.parse("a b"), Some(Key::Text("a b".into())));
        assert_eq!(KeyKind::Text.parse(""), None);
    }

    #[test]
    fn text_keys_are_one_path_segment() {
        assert_eq!(Key::Text("a/b c".into()).url_segment(), "a%2Fb%20c");
        assert_eq!(Key::Int(-3).url_segment(), "-3");
    }
}
