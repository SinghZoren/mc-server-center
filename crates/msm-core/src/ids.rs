//! Strongly typed identifiers.
//!
//! Every persisted entity gets its own ULID newtype so IDs of different kinds cannot be mixed up
//! at compile time. ULIDs sort by creation time, which keeps database indexes and logs readable.
//! The string form is the canonical 26-character Crockford base32 ULID.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Error returned when parsing an ID from a string fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind} id: {reason}")]
pub struct IdParseError {
    /// Which ID type was being parsed (e.g. `"instance"`).
    pub kind: &'static str,
    /// Why parsing failed.
    pub reason: String,
}

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Ulid);

        impl $name {
            /// Human-readable kind name used in errors and logs.
            pub const KIND: &'static str = $kind;

            /// Creates a new, unique ID.
            #[must_use]
            pub fn new() -> Self {
                Self(Ulid::new())
            }

            /// Wraps an existing ULID.
            #[must_use]
            pub const fn from_ulid(ulid: Ulid) -> Self {
                Self(ulid)
            }

            /// Returns the underlying ULID.
            #[must_use]
            pub const fn as_ulid(&self) -> Ulid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ulid::from_string(s).map(Self).map_err(|e| IdParseError {
                    kind: $kind,
                    reason: e.to_string(),
                })
            }
        }
    };
}

define_id!(
    /// A host running `msmd` (the local machine or a paired remote).
    HostId,
    "host"
);
define_id!(
    /// A Minecraft server instance.
    InstanceId,
    "instance"
);
define_id!(
    /// A long-running operation tracked by the task manager.
    TaskId,
    "task"
);
define_id!(
    /// A client request, used for idempotency and response correlation.
    RequestId,
    "request"
);
define_id!(
    /// A backup or safety snapshot.
    BackupId,
    "backup"
);
define_id!(
    /// An operation-journal entry for crash-safe multi-step mutations.
    JournalId,
    "journal"
);
define_id!(
    /// A paired remote controller device.
    DeviceId,
    "device"
);
define_id!(
    /// A custom-domain binding.
    DomainBindingId,
    "domain_binding"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_ids_are_unique() {
        let a = InstanceId::new();
        let b = InstanceId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn display_and_parse_round_trip() {
        let id = InstanceId::new();
        let text = id.to_string();
        assert_eq!(text.len(), 26);
        assert_eq!(text.parse::<InstanceId>().unwrap(), id);
    }

    #[test]
    fn parse_rejects_garbage_and_names_the_kind() {
        let err = "not-a-ulid".parse::<TaskId>().unwrap_err();
        assert_eq!(err.kind, "task");
    }

    #[test]
    fn serde_is_a_plain_string() {
        let id = BackupId::new();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        let back: BackupId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn ids_sort_by_creation_time() {
        let earlier = JournalId::from_ulid(Ulid::from_parts(1_000, 0));
        let later = JournalId::from_ulid(Ulid::from_parts(2_000, 0));
        assert!(earlier < later);
    }
}
