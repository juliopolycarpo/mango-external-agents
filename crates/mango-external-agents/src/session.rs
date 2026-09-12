//! A live conversation with one vendor CLI, and the typed reasons it ends by.
//!
//! The library hands a host [`Session`](crate::Session) handles and reason enums; it keeps no
//! registry of live sessions, polls no consent and fans nothing out to a hub. Those are host
//! policy, and a host builds whatever registry it needs on top of these handles.

use std::fmt;

/// Why a turn was stopped.
///
/// A reason enum rather than a message: the library never returns copy, and a host maps these to
/// its own words. The distinction between the four is load-bearing — "you stopped this turn" is a
/// lie for a shutdown, and a timeout is not a user's decision.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum CancelReason {
    /// Somebody asked for it: a stop button, an API call.
    Requested,
    /// The machine's owner withdrew permission to run external agents.
    ConsentRevoked,
    /// The turn passed a deadline the host set.
    Timeout,
    /// The host is going away.
    Shutdown,
}

impl fmt::Display for CancelReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Requested => "requested",
            Self::ConsentRevoked => "consent revoked",
            Self::Timeout => "timeout",
            Self::Shutdown => "shutdown",
        })
    }
}

/// Why a session was closed.
///
/// One reason shorter than [`CancelReason`]: a timeout ends a turn, never a session. A session
/// with a stalled turn is still a session, and closing it would throw away the vendor
/// conversation the host may still resume.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum CloseReason {
    /// Somebody asked for it.
    Requested,
    /// The machine's owner withdrew permission to run external agents.
    ConsentRevoked,
    /// The host is going away.
    Shutdown,
}

impl fmt::Display for CloseReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Requested => "requested",
            Self::ConsentRevoked => "consent revoked",
            Self::Shutdown => "shutdown",
        })
    }
}

impl From<CloseReason> for CancelReason {
    /// Closing a session cancels whatever turn was running, for the same reason.
    fn from(reason: CloseReason) -> Self {
        match reason {
            CloseReason::Requested => Self::Requested,
            CloseReason::ConsentRevoked => Self::ConsentRevoked,
            CloseReason::Shutdown => Self::Shutdown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CancelReason, CloseReason};

    #[test]
    fn closing_carries_its_reason_into_the_turn_it_cancels() {
        let cases = [
            (CloseReason::Requested, CancelReason::Requested),
            (CloseReason::ConsentRevoked, CancelReason::ConsentRevoked),
            (CloseReason::Shutdown, CancelReason::Shutdown),
        ];
        for (close, expected) in cases {
            assert_eq!(
                CancelReason::from(close),
                expected,
                "expected {expected:?}, received a different cancel reason for {close:?}"
            );
        }
    }

    #[test]
    fn reasons_print_as_words_a_host_can_map() {
        assert_eq!(CancelReason::ConsentRevoked.to_string(), "consent revoked");
        assert_eq!(CloseReason::Shutdown.to_string(), "shutdown");
    }
}
