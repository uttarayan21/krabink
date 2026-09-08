use serde::{Deserialize, Serialize};

macro_rules! ulid_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        pub struct $name(ulid::Ulid);

        impl $name {
            pub fn new() -> Self {
                Self(ulid::Ulid::new())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl core::str::FromStr for $name {
            type Err = crate::Error;

            fn from_str(s: &str) -> crate::Result<Self> {
                s.parse()
                    .map(Self)
                    .map_err(|_| crate::Error::MalformedId(s.to_owned()))
            }
        }

        impl core::fmt::Display for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

ulid_id!(
    /// Identifies a note document.
    NoteId
);
ulid_id!(
    /// Identifies a sketch within a note.
    SketchId
);
ulid_id!(
    /// Identifies a single stroke within a sketch.
    StrokeId
);
ulid_id!(
    /// Stable per-install identifier, used for presence and self-echo suppression.
    DeviceId
);
