use std::convert::Infallible;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Opaque identifier for a user account.
///
/// A newtype rather than a bare [`String`] so an id cannot be silently
/// interchanged with a username, an address, or any other string in scope. The
/// distinction matters most on the internal hop, where the gateway writes an id
/// into a header and the profile service reads it back out.
///
/// Serializes transparently, so the JSON representation is a plain string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserId(String);

impl UserId {
    /// Generates a fresh random identifier.
    #[must_use]
    pub fn random() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Borrows the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for UserId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for UserId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl AsRef<str> for UserId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for UserId {
    type Err = Infallible;

    /// Never fails; any string is a syntactically valid identifier.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::from(s))
    }
}
