use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A profile's revision counter, and the entity tag that carries it.
///
/// One type rather than a `u64` plus a formatting helper plus a parsing helper,
/// so the stored version and the `ETag` on the wire cannot drift apart. The
/// [`Display`] and [`FromStr`] impls are inverses by construction.
///
/// [`Display`]: fmt::Display
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Version(u64);

impl Version {
    /// The version a freshly created profile is stored at.
    pub const INITIAL: Self = Self(1);

    /// The counter as a plain integer, for storage backends that must persist
    /// one. Not a leak of an implementation detail: the same number is
    /// published to every client as an `ETag`.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Rebuilds a version read back from storage.
    #[must_use]
    pub const fn from_counter(counter: u64) -> Self {
        Self(counter)
    }

    /// The version this one becomes after an accepted write, or [`None`] if the
    /// counter cannot advance.
    ///
    /// Fallible rather than saturating or wrapping. Overflow is unreachable —
    /// at one write per nanosecond it is 584 years away — but at saturation
    /// every stale version would silently become valid again, and at wrap the
    /// sequence would repeat. Both turn a capacity limit into a correctness
    /// failure, so the caller is made to handle it.
    #[must_use]
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

/// Renders as an HTTP entity tag, quotes included: `"3"`.
impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"{}\"", self.0)
    }
}

/// An `If-Match` value that is not a version this service issued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidVersion;

impl fmt::Display for InvalidVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected an entity tag such as \"3\"")
    }
}

impl std::error::Error for InvalidVersion {}

impl FromStr for Version {
    type Err = InvalidVersion;

    /// Accepts the quoted form an `ETag` round-trips as (`"3"`) and the bare
    /// form typed by hand (`3`).
    ///
    /// `*` is rejected rather than treated as "any version": it means exactly
    /// the unconditional overwrite the precondition exists to prevent.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.trim()
            .trim_matches('"')
            .parse()
            .map(Self)
            .map_err(|_| InvalidVersion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Convenience for tests, where overflow is not in question.
    fn bump(version: Version, times: usize) -> Version {
        (0..times).fold(version, |current, _| {
            current
                .checked_next()
                .expect("test versions do not overflow")
        })
    }

    #[test]
    fn display_and_parse_are_inverses() {
        let version = bump(Version::INITIAL, 2);
        assert_eq!(version.to_string(), r#""3""#);
        assert_eq!(version.to_string().parse(), Ok(version));
    }

    #[test]
    fn bare_digits_are_accepted() {
        assert_eq!("3".parse(), Ok(bump(Version::INITIAL, 2)));
    }

    #[test]
    fn the_counter_refuses_to_wrap() {
        let last: Version = u64::MAX.to_string().parse().expect("parses");
        assert_eq!(last.checked_next(), None);
    }

    #[test]
    fn wildcard_is_rejected() {
        assert_eq!("*".parse::<Version>(), Err(InvalidVersion));
    }
}
