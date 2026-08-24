//! Types shared by [`auth-service`] and [`profile-service`].
//!
//! Both services compile against these definitions of the wire format, so
//! renaming a field is a compile error in the gateway rather than a `422` at
//! runtime. That is the only reason this crate exists; it holds no behaviour
//! beyond the conversions and invariants of [`UserId`].
//!
//! [`auth-service`]: https://github.com/0xSoftBoi/knova
//! [`profile-service`]: https://github.com/0xSoftBoi/knova

pub mod dto;
pub mod headers;
mod user_id;
mod version;

pub use user_id::UserId;
pub use version::{InvalidVersion, Version};

pub use dto::InvalidProfile;
