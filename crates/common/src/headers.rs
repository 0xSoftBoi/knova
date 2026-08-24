//! Header names used on the internal gateway-to-profile-service hop.
//!
//! Constants rather than literals at each call site: a typo in a literal is a
//! silent authorization bypass, a typo in a constant is a compile error.

/// Shared secret proving a request originated from the gateway rather than
/// from a client that discovered the profile service's port.
pub const INTERNAL_TOKEN: &str = "x-internal-token";

/// The authenticated [`UserId`], established by the gateway from a verified
/// token. The profile service treats it as ground truth only because it is read
/// after [`INTERNAL_TOKEN`] has been checked.
///
/// [`UserId`]: crate::UserId
pub const USER_ID: &str = "x-user-id";

/// Correlates the log lines both services emit for one client request.
///
/// Accepted from the caller when present so a trace can span the client too,
/// and minted by the gateway otherwise. Two services is the point at which
/// "grep both logs for the timestamp" stops working.
pub const REQUEST_ID: &str = "x-request-id";
