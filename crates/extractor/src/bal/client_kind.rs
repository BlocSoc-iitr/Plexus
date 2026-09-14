use std::fmt;
use std::str::FromStr;

use crate::bal::error::BalError;

/// The execution client Plexus is fetching block access lists from.
///
/// This selects the fetch path only, not the parsing: Reth serves JSON from
/// `eth_getBlockAccessList` and Nethermind serves raw RLP from
/// `debug_getRawBlockAccessList`, but both decode into the same
/// `alloy_eip7928` types.
///
/// Geth and Erigon are deliberately absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientKind {
    Reth,
    Nethermind,
}

impl ClientKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ClientKind::Reth => "reth",
            ClientKind::Nethermind => "nethermind",
        }
    }
}

impl fmt::Display for ClientKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ClientKind {
    type Err = BalError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "reth" => Ok(ClientKind::Reth),
            "nethermind" => Ok(ClientKind::Nethermind),
            other => Err(BalError::UnsupportedClient {
                name: other.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_clients_case_insensitively() {
        assert_eq!("reth".parse::<ClientKind>().unwrap(), ClientKind::Reth);
        assert_eq!("  Reth ".parse::<ClientKind>().unwrap(), ClientKind::Reth);
        assert_eq!(
            "NETHERMIND".parse::<ClientKind>().unwrap(),
            ClientKind::Nethermind
        );
    }

    #[test]
    fn as_str_round_trips_through_from_str() {
        for kind in [ClientKind::Reth, ClientKind::Nethermind] {
            assert_eq!(kind.as_str().parse::<ClientKind>().unwrap(), kind);
        }
    }

    // geth and erigon are known clients that simply can't serve a BAL yet
    #[test]
    fn blocked_and_unknown_clients_are_unsupported() {
        for name in ["geth", "erigon", "besu", "nonsense"] {
            let err = name.parse::<ClientKind>().unwrap_err();
            match err {
                BalError::UnsupportedClient { name: got } => assert_eq!(got, name),
                other => panic!("expected UnsupportedClient, got {other:?}"),
            }
        }
    }
}
