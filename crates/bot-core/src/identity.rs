//! Principal identity grammar shared by authenticated adapters and profile declarations.
use crate::id::{PrincipalId, is_name};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityKind {
    Local,
    Slack,
    Http,
    Service,
}

impl IdentityKind {
    pub fn is_human(self) -> bool {
        matches!(self, Self::Local | Self::Slack)
    }
}

pub fn identity_kind(identity: &str) -> Option<IdentityKind> {
    let (kind, value) = identity.split_once(':')?;
    match kind {
        "local" if is_name(value) => Some(IdentityKind::Local),
        "service" if is_name(value) => Some(IdentityKind::Service),
        "slack" => {
            let (workspace, user) = value.split_once('/')?;
            let bytes = user.as_bytes();
            (is_name(workspace)
                && (2..=32).contains(&bytes.len())
                && matches!(bytes[0], b'U' | b'W')
                && bytes[1..]
                    .iter()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()))
            .then_some(IdentityKind::Slack)
        }
        "http" => {
            let (endpoint, subject) = value.split_once('/')?;
            (is_name(endpoint)
                && (1..=128).contains(&subject.len())
                && subject.bytes().all(|b| {
                    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'@' | b'+')
                }))
            .then_some(IdentityKind::Http)
        }
        _ => None,
    }
}

impl PrincipalId {
    pub fn is_human(&self) -> bool {
        identity_kind(&self.0).is_some_and(IdentityKind::is_human)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_supported_human_identities_can_approve() {
        for id in ["local:owner", "slack:home/U1", "slack:home/W2ABC"] {
            assert!(PrincipalId(id.into()).is_human(), "{id}");
        }
        for id in ["http:inbox/alice", "service:morning"] {
            assert!(identity_kind(id).is_some());
            assert!(!PrincipalId(id.into()).is_human());
        }
        for id in [
            "slack:home/u1",
            "slack:home/U",
            "slack:home/B123",
            "slack:home/U1/extra",
            "local:a--b",
            "http:a/x:y",
            "other:owner",
        ] {
            assert!(identity_kind(id).is_none(), "{id}");
        }
    }
}
