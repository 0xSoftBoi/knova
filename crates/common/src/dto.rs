//! Request and response bodies.
//!
//! Deliberately behaviourless: these describe the shape of the JSON on the
//! wire and nothing else.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::{UserId, Version};

/// Body of `POST /login`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    /// The account name being claimed.
    pub username: String,
    /// The password offered for that account.
    pub password: String,
}

/// Successful response to `POST /login`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginResponse {
    /// Signed bearer token to present on subsequent requests.
    pub access_token: String,
    /// Always `"Bearer"`; present so the response matches OAuth 2.0 habits.
    ///
    /// A [`Cow`] because it is invariably a literal on the way out, and only
    /// ever owned when a client deserializes one.
    pub token_type: Cow<'static, str>,
    /// Seconds remaining until [`Self::access_token`] expires.
    pub expires_in: u64,
}

/// Why a profile payload was rejected.
///
/// Field-level, because a client cannot fix "invalid input" but can fix "phone
/// number has too few digits". None of these distinctions are sensitive: the
/// caller supplied the value being complained about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidProfile {
    /// Address was empty or only whitespace.
    #[error("address must not be empty")]
    EmptyAddress,
    /// Address exceeded [`ProfileInput::MAX_ADDRESS_BYTES`].
    #[error("address must be at most {} bytes", ProfileInput::MAX_ADDRESS_BYTES)]
    AddressTooLong,
    /// Phone number was empty or only whitespace.
    #[error("phone number must not be empty")]
    EmptyPhone,
    /// Phone number exceeded [`ProfileInput::MAX_PHONE_BYTES`].
    #[error("phone number must be at most {} bytes", ProfileInput::MAX_PHONE_BYTES)]
    PhoneTooLong,
    /// Phone number contained something other than digits and `+-() `.
    #[error("phone number may contain only digits and the characters + - ( ) and space")]
    PhoneCharacters,
    /// Phone number had too few or too many digits to be dialable.
    #[error(
        "phone number must contain between {} and {} digits",
        ProfileInput::MIN_PHONE_DIGITS,
        ProfileInput::MAX_PHONE_DIGITS
    )]
    PhoneDigitCount,
}

/// Body of `POST /profile` and `PUT /profile`.
///
/// There is deliberately no user id field: the caller never states whose
/// profile is being written. That comes from the verified token, which is why
/// an authenticated user cannot overwrite someone else's profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileInput {
    /// Postal address.
    pub address: String,
    /// Telephone number.
    pub phone_number: String,
}

impl ProfileInput {
    /// Longest accepted address. Generous for a postal address, and bounded so
    /// a client cannot pin megabytes of resident memory per stored profile.
    pub const MAX_ADDRESS_BYTES: usize = 512;
    /// Longest accepted phone number, in bytes.
    pub const MAX_PHONE_BYTES: usize = 32;
    /// Fewest digits in a dialable number (shortest national numbers).
    pub const MIN_PHONE_DIGITS: usize = 7;
    /// Most digits in a dialable number, per E.164.
    pub const MAX_PHONE_DIGITS: usize = 15;

    /// Checks the payload against the stored-field invariants.
    ///
    /// Lives here rather than in the profile service so the gateway could
    /// reject a bad payload before spending an upstream round-trip on it, and
    /// so both sides can never disagree about what is storable.
    ///
    /// # Errors
    ///
    /// Returns the first [`InvalidProfile`] the payload violates.
    pub fn validate(&self) -> Result<(), InvalidProfile> {
        if self.address.len() > Self::MAX_ADDRESS_BYTES {
            return Err(InvalidProfile::AddressTooLong);
        }
        if self.address.trim().is_empty() {
            return Err(InvalidProfile::EmptyAddress);
        }
        if self.phone_number.len() > Self::MAX_PHONE_BYTES {
            return Err(InvalidProfile::PhoneTooLong);
        }

        let phone = self.phone_number.trim();
        if phone.is_empty() {
            return Err(InvalidProfile::EmptyPhone);
        }
        if !phone
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '(' | ')' | ' '))
        {
            return Err(InvalidProfile::PhoneCharacters);
        }

        let digits = phone.chars().filter(char::is_ascii_digit).count();
        if !(Self::MIN_PHONE_DIGITS..=Self::MAX_PHONE_DIGITS).contains(&digits) {
            return Err(InvalidProfile::PhoneDigitCount);
        }

        Ok(())
    }
}

/// A stored profile as returned to the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    /// Owner of this profile.
    pub user_id: UserId,
    /// Free-form postal address.
    pub address: String,
    /// Free-form telephone number.
    pub phone_number: String,
    /// Revision counter, echoed as an `ETag` and required back in `If-Match`
    /// on update. It is how a lost update is detected.
    pub version: Version,
}

/// Uniform error envelope.
///
/// Every non-2xx response from either service has this shape. It matters most
/// for login, where the body must be byte-identical whether the username was
/// unknown or the password was wrong.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Stable machine-readable code, for example `invalid_credentials`.
    pub error: Cow<'static, str>,
    /// Human-readable text, chosen so it reveals nothing the code does not.
    pub message: Cow<'static, str>,
}

impl ErrorResponse {
    /// Builds a response from the static strings every call site already has,
    /// so rendering an error allocates nothing.
    #[must_use]
    pub const fn new(error: &'static str, message: &'static str) -> Self {
        Self {
            error: Cow::Borrowed(error),
            message: Cow::Borrowed(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(address: &str, phone: &str) -> ProfileInput {
        ProfileInput {
            address: address.to_owned(),
            phone_number: phone.to_owned(),
        }
    }

    #[test]
    fn accepts_ordinary_values() {
        assert_eq!(input("1 Main St", "+1 (555) 010-0123").validate(), Ok(()));
    }

    #[test]
    fn rejects_blank_fields() {
        assert_eq!(
            input("   ", "5550100").validate(),
            Err(InvalidProfile::EmptyAddress)
        );
        assert_eq!(
            input("1 Main St", "  ").validate(),
            Err(InvalidProfile::EmptyPhone)
        );
    }

    #[test]
    fn rejects_oversized_fields() {
        let long = "x".repeat(ProfileInput::MAX_ADDRESS_BYTES + 1);
        assert_eq!(
            input(&long, "5550100").validate(),
            Err(InvalidProfile::AddressTooLong)
        );
    }

    #[test]
    fn rejects_undialable_numbers() {
        assert_eq!(
            input("1 Main St", "555-01").validate(),
            Err(InvalidProfile::PhoneDigitCount)
        );
        assert_eq!(
            input("1 Main St", "555-0100 ext. 12").validate(),
            Err(InvalidProfile::PhoneCharacters)
        );
    }
}
