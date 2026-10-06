//! What is known about whether a server answers `textDocument/diagnostic`.
//!
//! `diagnosticProvider` is the advertisement, but not the truth: pyright
//! advertises none and answers pulls anyway, and a push-only server answers
//! `-32601`. A server that advertises nothing is therefore probed once; what
//! the probe showed is kept per process in its slot.

/// What probing a server that advertises no pull provider has shown so far.
///
/// Held in the server's slot and fresh for every replacement process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum PullProbe {
    /// No pull request has been sent to the process yet.
    #[default]
    Untried,
    /// The process answered a pull request.
    Answered,
    /// The process answered a pull request with `-32601`.
    Refused,
}

/// Whether `get_diagnostics` can expect a pull answer from a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PullSupport {
    /// The server advertises `diagnosticProvider`.
    Advertised,
    /// Nothing advertised and not tried yet: the next pull is the probe.
    Probing,
    /// Nothing advertised, but the server answered a pull.
    Answers,
    /// Nothing advertised, and the server refused a pull.
    Unsupported,
}

impl PullSupport {
    /// The support of a server that advertises a pull provider or not, with
    /// what its probe showed.
    pub(super) const fn of(advertised: bool, probe: PullProbe) -> Self {
        match (advertised, probe) {
            (true, _) => Self::Advertised,
            (false, PullProbe::Untried) => Self::Probing,
            (false, PullProbe::Answered) => Self::Answers,
            (false, PullProbe::Refused) => Self::Unsupported,
        }
    }

    /// Whether `get_diagnostics` sends a pull request.
    pub(super) const fn sends_pull(self) -> bool {
        !matches!(self, Self::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_advertising_wins_over_any_probe_result() {
        for probe in [PullProbe::Untried, PullProbe::Answered, PullProbe::Refused] {
            assert_eq!(PullSupport::of(true, probe), PullSupport::Advertised);
        }
    }

    #[test]
    fn test_unadvertised_support_follows_the_probe() {
        assert_eq!(
            PullSupport::of(false, PullProbe::Untried),
            PullSupport::Probing
        );
        assert_eq!(
            PullSupport::of(false, PullProbe::Answered),
            PullSupport::Answers
        );
        assert_eq!(
            PullSupport::of(false, PullProbe::Refused),
            PullSupport::Unsupported
        );
    }

    #[test]
    fn test_only_a_refusing_server_is_not_pulled() {
        for support in [
            PullSupport::Advertised,
            PullSupport::Probing,
            PullSupport::Answers,
        ] {
            assert!(support.sends_pull());
        }
        assert!(!PullSupport::Unsupported.sends_pull());
    }
}
