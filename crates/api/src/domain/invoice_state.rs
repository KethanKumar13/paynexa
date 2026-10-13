use crate::error::AppError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvoiceStatus {
    Open,
    Processing,
    Paid,
    Void,
    Uncollectible,
}

use InvoiceStatus::*;

/// The only legal invoice transitions; everything else is rejected.
const ALLOWED: &[(InvoiceStatus, InvoiceStatus)] = &[
    (Open, Processing),
    (Processing, Paid),
    (Processing, Open),
    (Open, Void),
    (Open, Uncollectible),
];

impl InvoiceStatus {
    pub const ALL: [Self; 5] = [Open, Processing, Paid, Void, Uncollectible];

    pub fn as_str(self) -> &'static str {
        match self {
            Open => "open",
            Processing => "processing",
            Paid => "paid",
            Void => "void",
            Uncollectible => "uncollectible",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|status| status.as_str() == s)
    }
}

pub fn can_transition(from: InvoiceStatus, to: InvoiceStatus) -> bool {
    ALLOWED.contains(&(from, to))
}

pub fn transition(from: InvoiceStatus, to: InvoiceStatus) -> Result<InvoiceStatus, AppError> {
    if can_transition(from, to) {
        Ok(to)
    } else {
        Err(AppError::invalid_transition(from.as_str(), to.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_the_documented_transitions_are_allowed() {
        let allowed: Vec<_> = InvoiceStatus::ALL
            .into_iter()
            .flat_map(|from| InvoiceStatus::ALL.into_iter().map(move |to| (from, to)))
            .filter(|&(from, to)| can_transition(from, to))
            .map(|(from, to)| format!("{}->{}", from.as_str(), to.as_str()))
            .collect();
        assert_eq!(
            allowed,
            [
                "open->processing",
                "open->void",
                "open->uncollectible",
                "processing->open",
                "processing->paid",
            ]
        );
    }

    #[test]
    fn terminal_states_have_no_exits() {
        for terminal in [Paid, Void, Uncollectible] {
            for to in InvoiceStatus::ALL {
                assert!(!can_transition(terminal, to), "{terminal:?} -> {to:?}");
            }
        }
    }

    #[test]
    fn invalid_transition_is_a_conflict_error() {
        assert_eq!(
            transition(Processing, Void).unwrap_err().code(),
            "invalid_state_transition"
        );
        assert_eq!(transition(Open, Void).unwrap(), Void);
    }

    #[test]
    fn parses_only_known_states() {
        assert_eq!(InvoiceStatus::parse("uncollectible"), Some(Uncollectible));
        assert_eq!(InvoiceStatus::parse("draft"), None);
    }
}
