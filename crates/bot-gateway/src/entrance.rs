//! 입구 키와 유일성. 같은 입구를 두 프로필(또는 두 바인딩)이 쓰면 기동 전에 거절한다.
//! 오류에는 프로필·바인딩 ID와 호스트 별칭만 싣는다(실제 워크스페이스·채널 ID, 토큰은 싣지 않는다).
use bot_core::{
    id::{BindingId, ProfileId},
    profile::Adapter,
};
use std::collections::BTreeMap;
use std::fmt;

/// 프로필이 선언한 입구(별칭 수준). `profile-spec.md`의 바인딩 종류와 변종이 1:1이다.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Entrance {
    /// 호스트가 OS 사용자에 연결한 별칭
    Cli { account: String },
    /// 호스트에 등록한 워크스페이스·채널 별칭
    SlackChannel { workspace: String, channel: String },
    /// 워크스페이스의 봇과 1:1 DM 전체
    SlackDm { workspace: String },
    /// 호스트에 등록한 인증된 HTTP 입구
    Http { endpoint: String },
    /// 호스트에 등록한 OpenAI 호환 입구 별칭과 그 위의 공개 모델 이름
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
            Entrance::Cli { account } => write!(f, "cli 계정 {account}"),
            Entrance::SlackChannel { workspace, channel } => {
                write!(f, "slack {workspace}의 채널 {channel}")
            }
            Entrance::SlackDm { workspace } => write!(f, "slack {workspace}의 DM"),
            Entrance::Http { endpoint } => write!(f, "http 입구 {endpoint}"),
            Entrance::HttpModel { endpoint, model } => {
                write!(f, "http 입구 {endpoint}의 모델 {model}")
            }
        }
    }
}

/// 호스트 등록부로 별칭을 풀어 얻은 실제 입구. 어댑터가 이벤트에서 인증한 사실과 정확히 일치해야 한다.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Real {
    /// 연결한 로컬 사용자(uid)와 그가 고른 프로필
    Cli {
        uid: u32,
        profile: ProfileId,
    },
    SlackChannel {
        team: String,
        channel: String,
    },
    SlackDm {
        team: String,
    },
    Http {
        endpoint: String,
    },
    /// OpenAI 호환 입구 별칭과 그 위의 공개 모델 이름. 모델을 고르면 그 모델의 바인딩이 정해진다.
    HttpModel {
        endpoint: String,
        model: String,
    },
}

/// 입구를 쓰는 바인딩
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

/// 같은 입구가 두 번 올라왔다.
#[derive(Debug, Eq, PartialEq)]
pub struct Conflict {
    /// 나중에 온 주인이 쓰는 입구(별칭 수준)
    pub entrance: Entrance,
    /// 먼저 온 주인이 쓰는 입구. 같으면 같은 별칭을 두 번 쓴 것이고, 다르면 서로 다른 별칭이 호스트 설정에서 같은 실제 입구로 풀린 것이다.
    pub first_entrance: Entrance,
    pub first: Owner,
    pub second: Owner,
}

impl Conflict {
    /// 고치는 방법. 입구 종류는 `Entrance` 변종으로 가른다(새 입구를 더하면 이 `match`가 컴파일 오류로 알려 준다). 실제 팀·채널 ID는 싣지 않는다(별칭과 설정 위치만).
    fn fix(&self) -> &'static str {
        let same = self.first_entrance == self.entrance;
        match (&self.entrance, same) {
            (Entrance::Cli { .. }, true) => {
                "한 입구는 바인딩 하나에만 연결할 수 있어요. 한쪽 바인딩의 `account`를 다른 계정 별칭으로 바꾸거나 바인딩을 지워 주세요"
            }
            (Entrance::Cli { .. }, false) => {
                "같은 프로필의 CLI 입구는 OS 사용자마다 하나예요. 두 계정 별칭이 호스트 설정 `gateway.cli.accounts`에서 같은 OS 사용자를 가리켜요 — 계정마다 다른 OS 사용자를 연결하거나 한쪽 바인딩을 지워 주세요"
            }
            (Entrance::SlackChannel { .. } | Entrance::SlackDm { .. }, true) => {
                "한 입구는 바인딩 하나에만 연결할 수 있어요. 한쪽 바인딩의 채널(또는 워크스페이스) 별칭을 바꾸거나 바인딩을 지워 주세요"
            }
            (Entrance::SlackChannel { .. } | Entrance::SlackDm { .. }, false) => {
                "두 별칭이 호스트 설정 `gateway.slack.workspaces`에서 같은 Slack 채널(또는 팀)을 가리켜요 — 한쪽 별칭을 다른 채널로 바꾸거나 바인딩을 지워 주세요"
            }
            (Entrance::Http { .. } | Entrance::HttpModel { .. }, _) => {
                "한 입구는 바인딩 하나에만 연결할 수 있어요. 한쪽 바인딩의 `endpoint`를 바꾸거나 바인딩을 지워 주세요"
            }
        }
    }
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.first == self.second {
            write!(f, "{}: 같은 바인딩이 두 번 올라왔어요", self.first)
        } else if self.first_entrance == self.entrance {
            write!(
                f,
                "입구가 겹쳐요: {} 와 {} 가 모두 {}를 써요. {}",
                self.first,
                self.second,
                self.entrance,
                self.fix()
            )
        } else {
            write!(
                f,
                "입구가 겹쳐요: {}({})와 {}({})가 같은 실제 입구로 이어져요. {}",
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

/// 입구 → 주인. 같은 키가 두 번 들어오면 `Conflict`. 조회는 키 정확 일치뿐이다(부분 일치·와일드카드 없음).
/// 항목마다 오류 문구용으로 그 입구의 별칭 수준 선언(`Entrance`)을 받는다.
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
    /// 키가 곧 별칭 수준 입구인 표
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
        // 별칭이 다르면 다른 입구다. 부분 일치는 없다.
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
            text.starts_with("입구가 겹쳐요: p/a 와 q/b 가 모두 cli 계정 owner를 써요."),
            "{text}"
        );
        assert!(text.contains("`account`를 다른 계정 별칭으로"), "{text}");
        // 별칭·ID와 설정 위치 말고는 싣지 않는다
        assert!(!text.contains("uid"), "{text}");
    }

    #[test]
    fn two_cli_accounts_that_resolve_to_one_os_user_say_so_and_where_to_fix_it() {
        // 서로 다른 계정 별칭이 호스트 설정에서 같은 OS 사용자(uid)로 풀렸다: 별칭만 보이고 실제 uid는 싣지 않는다
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
                "notes/desk(cli 계정 owner)와 notes/lab(cli 계정 researcher)가 같은 실제 입구로"
            ),
            "{text}"
        );
        assert!(text.contains("OS 사용자마다 하나"), "{text}");
        assert!(text.contains("`gateway.cli.accounts`"), "{text}");
        assert!(
            !text.contains("501") && !text.contains("모두 cli 계정 researcher를 써요"),
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
            same_alias.contains("채널(또는 워크스페이스) 별칭을 바꾸거나"),
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
        assert!(http.contains("`endpoint`를 바꾸거나"), "{http}");
    }

    #[test]
    fn resolved_entrances_collide_even_when_aliases_differ() {
        // 두 별칭이 같은 실제 채널을 가리키면 라벨은 별칭 수준으로 보이고 실제 ID는 나오지 않는다
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
