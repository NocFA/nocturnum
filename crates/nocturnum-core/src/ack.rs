use serde::{Deserialize, Serialize};

/// How far a write has to get before the server answers, see `docs/decisions/0001-v1-scope.md`.
///
/// The levels are ordered, so `Durable` implies everything `Applied` does.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Ack {
    /// Validated and written to the WAL, may be lost on a crash and may not be searchable yet.
    Accepted,
    /// Written to the WAL and applied, so search sees it, but may be lost on power loss before
    /// the next background fsync.
    Applied,
    /// The WAL record is fsynced and the write is applied.
    #[default]
    Durable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_durable_and_orders_by_strength() {
        assert_eq!(Ack::default(), Ack::Durable);
        assert!(Ack::Accepted < Ack::Applied && Ack::Applied < Ack::Durable);
        assert_eq!(serde_json::to_string(&Ack::Applied).unwrap(), "\"applied\"");
    }
}
