//! 활성 프로필 집합 + 호스트 등록부 → 라우팅 표. 기동 전(그리고 프로필 집합이 바뀔 때마다) 한 번에 만들고 검사한다.
//! 입구 유일성(별칭·실제 ID 두 수준), 트리거 선택이 결정적인지, 민감(private) 트리거가 일반 처리기에 앞서 잡히는지를 본다.
//! 오류에는 프로필·바인딩·트리거 ID와 호스트 별칭만 싣는다.
use crate::{
    entrance::{Conflict, Entrance, Owner, Real, Table},
    host::{CliAccount, HostConfig, HostError},
};
use bot_core::{
    id::{AgentId, BindingId, ProfileId},
    profile::{Adapter, Binding, Snapshot, Trigger, TriggerSource, TriggerTarget},
};
use std::{collections::BTreeMap, fmt, sync::Arc};

pub type Active = Arc<BTreeMap<ProfileId, Arc<Snapshot>>>;

/// HTTP 요청 상태 조회가 쓰는 예약 경로. 트리거가 이 아래를 쓸 수 없다.
pub const STATUS_SEGMENT: &str = "_requests";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutesError(pub String);

impl fmt::Display for RoutesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RoutesError {}

impl From<Box<Conflict>> for RoutesError {
    fn from(conflict: Box<Conflict>) -> Self {
        RoutesError(conflict.to_string())
    }
}

impl From<HostError> for RoutesError {
    fn from(error: HostError) -> Self {
        RoutesError(error.0)
    }
}

/// 한 바인딩에 걸린 사건·HTTP 트리거와 그것을 선언한 에이전트
#[derive(Clone, Debug)]
pub struct BoundTrigger {
    pub agent: AgentId,
    pub trigger: Trigger,
}

/// 실제 입구가 가리키는 바인딩과 그 프로필
pub struct Route<'a> {
    pub snapshot: &'a Arc<Snapshot>,
    pub binding_id: &'a BindingId,
    pub binding: &'a Binding,
    /// order 오름차순(같으면 에이전트·트리거 ID 순)
    pub triggers: &'a [BoundTrigger],
}

pub struct Routes {
    source: Active,
    table: Table<Real>,
    triggers: BTreeMap<(ProfileId, BindingId), Vec<BoundTrigger>>,
}

impl Routes {
    pub fn build(
        source: Active,
        host: &HostConfig,
        uid_of: &dyn Fn(&CliAccount) -> Result<u32, HostError>,
    ) -> Result<Self, RoutesError> {
        let owners = || {
            source.iter().flat_map(|(profile, snapshot)| {
                snapshot.bindings.iter().map(move |(binding, entry)| {
                    (
                        Entrance::from(&entry.source),
                        Owner {
                            profile: profile.clone(),
                            binding: binding.clone(),
                        },
                    )
                })
            })
        };
        // 1) 프로필이 선언한 별칭 수준 입구는 활성 프로필 전체에서 한 번씩만
        Table::build_described(owners())?;
        // 2) 호스트 등록부로 풀어 실제 입구 수준에서도 겹치지 않는지
        let mut resolved = Vec::new();
        for (entrance, owner) in owners() {
            let real = resolve(&entrance, &owner, host, uid_of)?;
            resolved.push((real, owner, entrance.to_string()));
        }
        let table = Table::build(resolved)?;
        let triggers = collect_triggers(&source)?;
        Ok(Self {
            source,
            table,
            triggers,
        })
    }

    pub fn find(&self, real: &Real) -> Option<Route<'_>> {
        let owner = self.table.get(real)?;
        let snapshot = self.source.get(&owner.profile)?;
        let binding = snapshot.bindings.get(&owner.binding)?;
        let triggers = self
            .triggers
            .get(&(owner.profile.clone(), owner.binding.clone()))
            .map_or(&[][..], Vec::as_slice);
        Some(Route {
            snapshot,
            binding_id: &owner.binding,
            binding,
            triggers,
        })
    }

    /// 이 uid가 CLI로 닿는 프로필들(`bot chat`이 프로필을 생략했을 때 고르는 데 쓴다).
    pub fn cli_profiles(&self, uid: u32) -> Vec<ProfileId> {
        self.table
            .iter()
            .filter_map(|(real, _)| match real {
                Real::Cli { uid: u, profile } if *u == uid => Some(profile.clone()),
                _ => None,
            })
            .collect()
    }

    /// HTTP 입구 별칭을 쓰는 바인딩
    pub fn http_owner(&self, endpoint: &str) -> Option<&Owner> {
        self.table.get(&Real::Http {
            endpoint: endpoint.to_owned(),
        })
    }

    /// 활성 프로필 전체
    pub fn profiles(&self) -> impl Iterator<Item = &Arc<Snapshot>> {
        self.source.values()
    }

    pub fn snapshot(&self, profile: &ProfileId) -> Option<&Arc<Snapshot>> {
        self.source.get(profile)
    }

    /// 만든 뒤 활성 집합이 바뀌지 않았는가
    pub fn is_current(&self, active: &Active) -> bool {
        Arc::ptr_eq(&self.source, active)
    }

    pub fn entrances(&self) -> impl Iterator<Item = (&Real, &Owner)> {
        self.table.iter()
    }
}

fn unknown(owner: &Owner, what: &str, alias: &str) -> RoutesError {
    RoutesError(format!(
        "{owner}: 호스트에 등록되지 않은 {what} 별칭 {alias}"
    ))
}

fn resolve(
    entrance: &Entrance,
    owner: &Owner,
    host: &HostConfig,
    uid_of: &dyn Fn(&CliAccount) -> Result<u32, HostError>,
) -> Result<Real, RoutesError> {
    match entrance {
        Entrance::Cli { account } => {
            let entry = host
                .cli
                .accounts
                .get(account)
                .ok_or_else(|| unknown(owner, "cli 계정", account))?;
            Ok(Real::Cli {
                uid: uid_of(entry)?,
                profile: owner.profile.clone(),
            })
        }
        Entrance::SlackChannel { workspace, channel } => {
            let ws = host
                .slack
                .workspaces
                .get(workspace)
                .ok_or_else(|| unknown(owner, "slack 워크스페이스", workspace))?;
            let id = ws
                .channels
                .get(channel)
                .ok_or_else(|| unknown(owner, "slack 채널", channel))?;
            Ok(Real::SlackChannel {
                team: ws.team_id.clone(),
                channel: id.clone(),
            })
        }
        Entrance::SlackDm { workspace } => {
            let ws = host
                .slack
                .workspaces
                .get(workspace)
                .ok_or_else(|| unknown(owner, "slack 워크스페이스", workspace))?;
            if !ws.dm {
                return Err(RoutesError(format!(
                    "{owner}: 호스트가 slack 워크스페이스 {workspace}의 DM 입구를 열지 않았어요(dm: true)"
                )));
            }
            Ok(Real::SlackDm {
                team: ws.team_id.clone(),
            })
        }
        Entrance::Http { endpoint } => {
            if !host.http.endpoints.contains_key(endpoint) {
                return Err(unknown(owner, "http 입구", endpoint));
            }
            Ok(Real::Http {
                endpoint: endpoint.clone(),
            })
        }
    }
}

/// 같은 후보군(바인딩 + 사건 또는 method·path)을 가리키는 키
fn candidate_key(trigger: &Trigger) -> String {
    match &trigger.on {
        TriggerSource::Event { event, .. } => format!("event {event}"),
        TriggerSource::Http { method, path, .. } => format!("http {method} {path}"),
        TriggerSource::Cron { .. } => "cron".into(),
    }
}

fn reserved(path: &str) -> bool {
    let first = path.trim_start_matches('/').split('/').next();
    first == Some(STATUS_SEGMENT)
}

fn collect_triggers(
    source: &Active,
) -> Result<BTreeMap<(ProfileId, BindingId), Vec<BoundTrigger>>, RoutesError> {
    let mut out: BTreeMap<(ProfileId, BindingId), Vec<BoundTrigger>> = BTreeMap::new();
    for (profile, snapshot) in source.iter() {
        for (agent_id, agent) in &snapshot.agents {
            for trigger in &agent.triggers {
                let (binding_id, http) = match &trigger.on {
                    TriggerSource::Cron { .. } => continue,
                    TriggerSource::Event { binding, event } => {
                        if event != "message" {
                            return Err(RoutesError(format!(
                                "{}/{}: 지원하지 않는 이벤트 {event}",
                                profile.0, trigger.id
                            )));
                        }
                        (binding, false)
                    }
                    TriggerSource::Http { binding, path, .. } => {
                        if reserved(path) {
                            return Err(RoutesError(format!(
                                "{}/{}: 경로가 예약된 /{STATUS_SEGMENT} 아래예요",
                                profile.0, trigger.id
                            )));
                        }
                        (binding, true)
                    }
                };
                let binding = snapshot.bindings.get(binding_id).ok_or_else(|| {
                    RoutesError(format!(
                        "{}/{}: 없는 바인딩 {}",
                        profile.0, trigger.id, binding_id.0
                    ))
                })?;
                if matches!(binding.source, Adapter::Http { .. }) != http {
                    return Err(RoutesError(format!(
                        "{}/{}: 트리거 종류가 바인딩 {}의 입구와 맞지 않아요",
                        profile.0, trigger.id, binding_id.0
                    )));
                }
                out.entry((profile.clone(), binding_id.clone()))
                    .or_default()
                    .push(BoundTrigger {
                        agent: agent_id.clone(),
                        trigger: trigger.clone(),
                    });
            }
        }
    }
    for ((profile, binding), list) in &mut out {
        list.sort_by(|a, b| {
            (a.trigger.order, &a.agent, &a.trigger.id).cmp(&(
                b.trigger.order,
                &b.agent,
                &b.trigger.id,
            ))
        });
        check_candidates(profile, binding, list, &source[profile].bindings[binding])?;
    }
    Ok(out)
}

fn check_candidates(
    profile: &ProfileId,
    binding: &BindingId,
    list: &[BoundTrigger],
    entry: &Binding,
) -> Result<(), RoutesError> {
    // 같은 후보군(바인딩 + 사건 또는 method·path)에서 같은 순위가 둘 이상이면 먼저 잡을 쪽이 정해지지 않는다.
    // 정렬 순서에서 이웃하지 않아도(A·B·A) 놓치지 않도록 (후보 키, order) 쌍 전체를 본다.
    let mut seen: BTreeMap<(String, i64), &str> = BTreeMap::new();
    for bound in list {
        let trigger = &bound.trigger;
        if let Some(first) = seen.insert((candidate_key(trigger), trigger.order), &trigger.id) {
            return Err(RoutesError(format!(
                "{}/{}: 트리거 {} 와 {} 의 순위가 같아 먼저 잡을 쪽이 정해지지 않아요",
                profile.0, binding.0, first, trigger.id
            )));
        }
    }
    for private in list.iter().filter(|t| t.trigger.private) {
        let who = format!("{}/{}", profile.0, private.trigger.id);
        if !matches!(entry.source, Adapter::SlackDm { .. }) {
            return Err(RoutesError(format!(
                "{who}: private은 slack DM 바인딩에서만 쓸 수 있어요"
            )));
        }
        if !matches!(private.trigger.target, TriggerTarget::Tool { .. }) {
            return Err(RoutesError(format!(
                "{who}: private은 도구 직접 호출만 쓸 수 있어요"
            )));
        }
        let key = candidate_key(&private.trigger);
        if let Some(earlier) = list.iter().find(|t| {
            !t.trigger.private
                && candidate_key(&t.trigger) == key
                && t.trigger.order <= private.trigger.order
        }) {
            return Err(RoutesError(format!(
                "{who}: 트리거 {} 가 먼저 잡을 수 있어 민감 입력이 일반 처리기로 갈 수 있어요",
                earlier.trigger.id
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
