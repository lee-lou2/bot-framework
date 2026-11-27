//! Entrance keys and exact-match uniqueness table across profiles and bindings.
//!
//! Conflict messages report only profile/binding IDs and host aliases — never real workspace/channel IDs, UIDs, or tokens.
use bot_core::{
    id::{BindingId, ProfileId},
    profile::Adapter,
};
use std::collections::BTreeMap;
use std::fmt;

/// Alias-level entrance declared by a profile binding.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Entrance {
    Cli { account: String },
    SlackChannel { workspace: String, channel: String },
    SlackDm { workspace: String },
    Http { endpoint: String },
    HttpModel { endpoint: String, model: String },
}

impl From<&Adapter> for Entrance {
    fn from(adapter: &Adapter) -> Self {
        match adapter {
            Adapter::Cli { account } => Entrance::Cli {
                account: account.clone(),
            },
            Adapter::SlackChannel {
                workspace, channel, ..
            } => Entrance::SlackChannel {
                workspace: workspace.clone(),
                channel: channel.clone(),
            },
            Adapter::SlackDm { workspace } => Entrance::SlackDm {
                workspace: workspace.clone(),
            },
            Adapter::Http { endpoint } => Entrance::Http {
                endpoint: endpoint.clone(),
            },
            Adapter::OpenAi {
                endpoint, model, ..
            } => Entrance::HttpModel {
                endpoint: endpoint.clone(),
                model: model.clone(),
            },
        }
    }
}

impl fmt::Display for Entrance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Entrance::Cli { account } => write!(f, "cli account {account}"),
            Entrance::SlackChannel { workspace, channel } => {
                write!(f, "slack {workspace} channel {channel}")
            }
            Entrance::SlackDm { workspace } => write!(f, "slack {workspace} DM"),
            Entrance::Http { endpoint } => write!(f, "http endpoint {endpoint}"),
            Entrance::HttpModel { endpoint, model } => {
                write!(f, "http endpoint {endpoint} model {model}")
            }
        }
    }
}

/// Host-resolved entrance key matched strictly against adapter-authenticated facts.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Real {
    Cli { uid: u32, profile: ProfileId },
    SlackChannel { team: String, channel: String },
    SlackDm { team: String },
    Http { endpoint: String },
    HttpModel { endpoint: String, model: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Owner {
    pub profile: ProfileId,
    pub binding: BindingId,
}

impl fmt::Display for Owner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.profile.0, self.binding.0)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct Conflict {
    pub entrance: Entrance,
    /// When equal to `entrance`, the same alias was declared twice; otherwise distinct aliases resolved to the same real entrance.
    pub first_entrance: Entrance,
    pub first: Owner,
    pub second: Owner,
}

impl Conflict {
    fn fix(&self) -> &'static str {
        let same = self.first_entrance == self.entrance;
        match (&self.entrance, same) {
            (Entrance::Cli { .. }, true) => {
                "Each entrance can only belong to one binding. Change `account` on one binding to a different account alias or remove one binding."
            }
            (Entrance::Cli { .. }, false) => {
                "A profile allows only one CLI entrance per OS user. Two account aliases resolve to the same OS user in `gateway.cli.accounts` — map each alias to a different OS user or remove one binding."
            }
            (Entrance::SlackChannel { .. } | Entrance::SlackDm { .. }, true) => {
                "Each entrance can only belong to one binding. Change the channel (or workspace) alias on one binding or remove one binding."
            }
            (Entrance::SlackChannel { .. } | Entrance::SlackDm { .. }, false) => {
                "Two aliases resolve to the same Slack channel (or team) in `gateway.slack.workspaces` — point one alias to a different channel or remove one binding."
            }
            (Entrance::Http { .. } | Entrance::HttpModel { .. }, _) => {
                "Each entrance can only belong to one binding. Change `endpoint` on one binding or remove one binding."
            }
        }
    }
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.first == self.second {
            write!(f, "{}: duplicate binding", self.first)
        } else if self.first_entrance == self.entrance {
            write!(
                f,
                "Entrance conflict: {} and {} both use {}. {}",
                self.first,
                self.second,
                self.entrance,
                self.fix()
            )
        } else {
            write!(
                f,
                "Entrance conflict: {} ({}) and {} ({}) resolve to the same entrance. {}",
                self.first,
                self.first_entrance,
                self.second,
                self.entrance,
                self.fix()
            )
        }
    }
}

impl std::error::Error for Conflict {}

/// Exact-match entrance-to-owner map rejecting any duplicate key with `Conflict`.
#[derive(Debug)]
pub struct Table<K: Ord> {
    map: BTreeMap<K, (Owner, Entrance)>,
}

impl<K: Ord + Clone> Table<K> {
    pub fn build(
        entries: impl IntoIterator<Item = (K, Owner, Entrance)>,
    ) -> Result<Self, Box<Conflict>> {
        let mut map: BTreeMap<K, (Owner, Entrance)> = BTreeMap::new();
        for (key, owner, label) in entries {
            if let Some((first, first_label)) = map.get(&key) {
                return Err(Box::new(Conflict {
                    entrance: label,
                    first_entrance: first_label.clone(),
                    first: first.clone(),
                    second: owner,
                }));
            }
            map.insert(key, (owner, label));
        }
        Ok(Self { map })
    }

    pub fn get(&self, key: &K) -> Option<&Owner> {
        self.map.get(key).map(|(owner, _)| owner)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&K, &Owner)> {
        self.map.iter().map(|(key, (owner, _))| (key, owner))
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl Table<Entrance> {
    pub fn build_described(
        entries: impl IntoIterator<Item = (Entrance, Owner)>,
    ) -> Result<Self, Box<Conflict>> {
        Self::build(entries.into_iter().map(|(key, owner)| {
            let label = key.clone();
            (key, owner, label)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(profile: &str, binding: &str) -> Owner {
        Owner {
            profile: ProfileId(profile.into()),
            binding: BindingId(binding.into()),
        }
    }

    fn channel(workspace: &str, channel: &str) -> Entrance {
        Entrance::SlackChannel {
            workspace: workspace.into(),
            channel: channel.into(),
        }
    }

    #[test]
    fn rejects_the_same_entrance_in_two_profiles() {
        let err = Table::build_described([
            (channel("home", "planning"), owner("personal", "team")),
            (channel("home", "planning"), owner("work", "plan")),
        ])
        .unwrap_err();
        assert_eq!(err.first, owner("personal", "team"));
        assert_eq!(err.second, owner("work", "plan"));
        let text = err.to_string();
        assert!(text.contains("personal/team") && text.contains("work/plan"));
        assert!(text.contains("planning"));
    }

    #[test]
    fn rejects_two_bindings_of_one_profile_on_the_same_entrance() {
        let dm = || Entrance::SlackDm {
            workspace: "home".into(),
        };
        let err = Table::build_described([
            (dm(), owner("personal", "a")),
            (dm(), owner("personal", "b")),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("DM"));
    }

    #[test]
    fn a_channel_and_a_dm_in_one_workspace_do_not_overlap() {
        let table = Table::build_described([
            (channel("home", "planning"), owner("personal", "team")),
            (
                Entrance::SlackDm {
                    workspace: "home".into(),
                },
                owner("personal", "direct"),
            ),
            (
                Entrance::Cli {
                    account: "owner".into(),
                },
                owner("personal", "desk"),
            ),
            (
                Entrance::Http {
                    endpoint: "reminders".into(),
                },
                owner("personal", "inbox"),
            ),
        ])
        .unwrap();
        assert_eq!(table.len(), 4);
        assert_eq!(
            table.get(&channel("home", "planning")),
            Some(&owner("personal", "team"))
        );
        assert_eq!(table.get(&channel("home", "plan")), None);
        assert_eq!(table.get(&channel("other", "planning")), None);
    }

    #[test]
    fn conflict_text_never_contains_more_than_aliases_and_ids() {
        let err = Table::build_described([
            (
                Entrance::Cli {
                    account: "owner".into(),
                },
                owner("p", "a"),
            ),
            (
                Entrance::Cli {
                    account: "owner".into(),
                },
                owner("q", "b"),
            ),
        ])
        .unwrap_err();
        let text = err.to_string();
        assert!(
            text.starts_with("Entrance conflict: p/a and q/b both use cli account owner."),
            "{text}"
        );
        assert!(
            text.contains("`account` on one binding to a different account alias"),
            "{text}"
        );
        assert!(!text.contains("uid"), "{text}");
    }

    #[test]
    fn two_cli_accounts_that_resolve_to_one_os_user_say_so_and_where_to_fix_it() {
        let real = Real::Cli {
            uid: 501,
            profile: ProfileId("notes".into()),
        };
        let err = Table::build([
            (
                real.clone(),
                owner("notes", "desk"),
                Entrance::Cli {
                    account: "owner".into(),
                },
            ),
            (
                real,
                owner("notes", "lab"),
                Entrance::Cli {
                    account: "researcher".into(),
                },
            ),
        ])
        .unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains(
                "notes/desk (cli account owner) and notes/lab (cli account researcher) resolve to the same entrance"
            ),
            "{text}"
        );
        assert!(text.contains("one CLI entrance per OS user"), "{text}");
        assert!(text.contains("`gateway.cli.accounts`"), "{text}");
        assert!(
            !text.contains("501") && !text.contains("both use cli account researcher"),
            "{text}"
        );
    }

    #[test]
    fn slack_and_http_conflicts_name_their_own_fix() {
        let same_alias = Table::build_described([
            (channel("home", "planning"), owner("a", "x")),
            (channel("home", "planning"), owner("b", "y")),
        ])
        .unwrap_err()
        .to_string();
        assert!(
            same_alias.contains("channel (or workspace) alias"),
            "{same_alias}"
        );
        let http = Table::build_described([
            (
                Entrance::Http {
                    endpoint: "inbox".into(),
                },
                owner("a", "x"),
            ),
            (
                Entrance::Http {
                    endpoint: "inbox".into(),
                },
                owner("b", "y"),
            ),
        ])
        .unwrap_err()
        .to_string();
        assert!(http.contains("Change `endpoint`"), "{http}");
    }

    #[test]
    fn resolved_entrances_collide_even_when_aliases_differ() {
        let real = Real::SlackChannel {
            team: "T1".into(),
            channel: "C1".into(),
        };
        let err = Table::build([
            (real.clone(), owner("a", "x"), channel("home", "planning")),
            (real, owner("b", "y"), channel("home", "plan-copy")),
        ])
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("planning") || text.contains("plan-copy"));
        assert!(!text.contains("T1") && !text.contains("C1"));
    }
}
