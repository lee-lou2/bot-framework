//! 관리 API(`/v1/admin/*`)의 입력 검증과 핸들러. 인증은 `auth::guard`가 앞서 끝냈고 `Viewer`가 요청에 실려 있다.
//! 운영자 전용 조회·판정은 여기서 먼저 막고, 저장소도 `Viewer`로 한 번 더 한정한다. 판정의 감사는 저장소가 같은 커밋에 쓴다.

use crate::{
    auth::Guard,
    json,
    port::*,
    resp::{Fail, LeakCheck, ok},
};
use axum::{
    Extension,
    body::Bytes,
    extract::{Path, RawQuery, State},
    http::StatusCode,
    response::Response,
};
use bot_core::{
    id::*,
    port::{Broker, Error},
    profile::ProfileCatalog,
};
use serde_json::{Map, Value};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Instant, SystemTime},
};

pub(crate) struct St {
    pub read: Arc<dyn AdminRead>,
    pub act: Arc<dyn AdminAct>,
    pub broker: Arc<dyn Broker>,
    pub profiles: Arc<dyn ProfileCatalog>,
    pub guard: Arc<Guard>,
    pub leak: Option<LeakCheck>,
    pub started: Instant,
    pub extra: crate::Extra,
}

pub(crate) type S = State<Arc<St>>;
pub(crate) type V = Extension<Viewer>;
pub(crate) type R = Result<Response, Fail>;

pub(crate) const INVALID: Fail = Fail(StatusCode::BAD_REQUEST, "invalid");
const DENIED: Fail = Fail(StatusCode::FORBIDDEN, "denied");
const BODY_MAX: usize = 4096;
const TEXT_MAX: usize = 512;

type Params = HashMap<String, String>;

/// 쿼리 문자열을 엄격하게 읽는다: 허용한 키만, 중복 키 없음, 잘못된 퍼센트 부호 거절.
fn params(raw: Option<&str>, allowed: &[&str]) -> Result<Params, Fail> {
    let mut out = HashMap::new();
    for pair in raw.unwrap_or_default().split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let (k, v) = (pct_decode(k).ok_or(INVALID)?, pct_decode(v).ok_or(INVALID)?);
        if !allowed.contains(&k.as_str()) || out.insert(k, v).is_some() {
            return Err(INVALID);
        }
    }
    Ok(out)
}

fn pct_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::with_capacity(b.len()), 0);
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = s.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// 불투명 식별자: 1..=128자의 ASCII 영숫자와 `._:-`.
fn token(s: &str) -> Option<&str> {
    let good = (1..=128).contains(&s.len())
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b':' | b'-'));
    good.then_some(s)
}

pub(crate) fn profile_id(s: &str) -> Option<ProfileId> {
    bot_core::id::is_name(s).then(|| ProfileId(s.to_owned()))
}

fn cursor(s: &str) -> Option<String> {
    let good = (1..=200).contains(&s.len())
        && s.bytes().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b':' | b'~' | b'=' | b'-')
        });
    good.then(|| s.to_owned())
}

fn limit(p: &Params) -> Result<u32, Fail> {
    match p.get("limit") {
        None => Ok(50),
        Some(s) => s
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=200).contains(n))
            .ok_or(INVALID),
    }
}

fn profile_param(p: &Params) -> Result<Option<ProfileId>, Fail> {
    p.get("profile")
        .map(|s| profile_id(s).ok_or(INVALID))
        .transpose()
}

fn page(p: &Params) -> Result<Page, Fail> {
    let after = p
        .get("after")
        .map(|s| cursor(s).ok_or(INVALID))
        .transpose()?;
    Ok(Page {
        limit: limit(p)?,
        after,
    })
}

pub(crate) fn operator(v: &Viewer) -> Result<(), Fail> {
    if v.operator { Ok(()) } else { Err(DENIED) }
}

/// 요청 본문: 4 KiB 이하의 JSON 객체이고 허용한 키만 가진다.
pub(crate) fn body(bytes: &Bytes, allowed: &[&str]) -> Result<Map<String, Value>, Fail> {
    if bytes.len() > BODY_MAX {
        return Err(INVALID);
    }
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(m)) if m.keys().all(|k| allowed.contains(&k.as_str())) => Ok(m),
        _ => Err(INVALID),
    }
}

/// 한 줄 글: 앞뒤 공백을 걷고 1..=512자, 제어 문자 없음.
fn text(v: Option<&Value>) -> Result<String, Fail> {
    let s = v.and_then(Value::as_str).map(str::trim).ok_or(INVALID)?;
    let good = !s.is_empty() && s.chars().count() <= TEXT_MAX && !s.chars().any(char::is_control);
    good.then(|| s.to_owned()).ok_or(INVALID)
}

pub(crate) fn identifier(s: &str) -> Result<String, Fail> {
    token(s).map(str::to_owned).ok_or(INVALID)
}

pub(crate) async fn me(State(st): S, Extension(v): V) -> R {
    let body = serde_json::json!({
        "principal": v.principal.0, "operator": v.operator, "csrf": st.guard.csrf(&v),
    });
    Ok(ok(&body, None))
}

pub(crate) async fn runs(State(st): S, Extension(v): V, RawQuery(raw): RawQuery) -> R {
    let p = params(
        raw.as_deref(),
        &["profile", "state", "root", "limit", "after"],
    )?;
    let query = RunQuery {
        profile: profile_param(&p)?,
        state: p
            .get("state")
            .map(|s| json::parse_run_state(s).ok_or(INVALID))
            .transpose()?,
        root: p
            .get("root")
            .map(|s| identifier(s).map(RunId))
            .transpose()?,
        page: page(&p)?,
    };
    let list = st.read.runs(&v, query).await?;
    let mut body = json::listing(&list, json::run);
    st.extra.decorate(&mut body);
    Ok(ok(&body, st.leak.as_ref()))
}

pub(crate) async fn run_detail(
    State(st): S,
    Extension(v): V,
    Path((profile, run)): Path<(String, String)>,
) -> R {
    let profile = profile_id(&profile).ok_or(INVALID)?;
    let d = st
        .read
        .detail(&v, profile, RunId(identifier(&run)?))
        .await?;
    let mut body = json::detail(&d);
    st.extra.decorate(&mut body);
    Ok(ok(&body, st.leak.as_ref()))
}

pub(crate) async fn approvals(State(st): S, Extension(v): V, RawQuery(raw): RawQuery) -> R {
    let p = params(raw.as_deref(), &["profile", "state", "limit", "after"])?;
    let query = ApprovalQuery {
        profile: profile_param(&p)?,
        filter: match p.get("state") {
            None => ApprovalFilter::Pending,
            Some(s) => json::parse_approval_filter(s).ok_or(INVALID)?,
        },
        page: page(&p)?,
    };
    let list = st.read.approvals(&v, query).await?;
    Ok(ok(
        &json::listing(&list, |a| json::approval(a, &v)),
        st.leak.as_ref(),
    ))
}

/// 이미 내려진 같은 판정인가: 같은 승인자가 같은 해시로 같은 판정을 다시 보냈다.
/// 결정자 기록이 없거나(`None`) 다른 사람이면 같다고 보지 않는다.
fn same_decision(view: &ApprovalView, who: &PrincipalId, allow: bool, seen: &str) -> bool {
    let was = match view.state {
        ApprovalState::Approved | ApprovalState::Consumed => true,
        ApprovalState::Denied => false,
        _ => return false,
    };
    was == allow
        && json::hex(&view.args_sha256) == seen
        && view.approver == *who
        && view.decided_by.as_ref() == Some(who)
}

fn repeated(st: &St, view: &ApprovalView, viewer: &Viewer) -> Response {
    let body =
        serde_json::json!({"ok": true, "repeated": true, "approval": json::approval(view, viewer)});
    ok(&body, st.leak.as_ref())
}

/// 승인·거절. 화면이 본 인자 해시(`seen`)가 지금 값과 같아야 하고, 클릭한 사람이 지정된 승인자여야 한다.
/// 실제 소비·세대·만료·정책 재판정은 브로커가 같은 커밋에서 한다.
pub(crate) async fn decide(
    State(st): S,
    Extension(v): V,
    Path(nonce): Path<String>,
    bytes: Bytes,
) -> R {
    let nonce = ApprovalId(identifier(&nonce)?);
    let b = body(&bytes, &["allow", "seen"])?;
    let (Some(allow), Some(seen)) = (
        b.get("allow").and_then(Value::as_bool),
        b.get("seen").and_then(Value::as_str),
    ) else {
        return Err(INVALID);
    };
    let view = st.read.approval(&v, nonce.clone()).await?;
    // 같은 승인자가 같은 내용을 같은 판정으로 다시 보내면 기존 결과를 돌려준다(브로커를 다시 부르지 않는다).
    if same_decision(&view, &v.principal, allow, seen) {
        return Ok(repeated(&st, &view, &v));
    }
    // 반대 판정이나 무효화된 승인은 계속 거절한다.
    if view.state != ApprovalState::Pending {
        return Err(Fail(StatusCode::CONFLICT, "stale"));
    }
    if json::hex(&view.args_sha256) != seen {
        return Err(Fail(StatusCode::CONFLICT, "changed"));
    }
    if view.approver != v.principal {
        return Err(DENIED);
    }
    // 볼 수 없는 본문은 승인할 수 없다(거절은 허용). 브로커까지 가지 않는다.
    if allow && let Some(word) = json::approve_blocked(&view, &v) {
        return Err(Fail(StatusCode::FORBIDDEN, word));
    }
    match st
        .broker
        .decide(nonce.clone(), v.principal.clone(), allow)
        .await
    {
        Ok(()) => {}
        // 같은 판정이 동시에 먼저 커밋됐을 수 있다: 원장을 다시 읽어 같은 결정자·해시·판정이면 기존 결과를 돌려준다.
        Err(e @ (Error::Stale | Error::Conflict)) => {
            return match st.read.approval(&v, nonce).await {
                Ok(now) if same_decision(&now, &v.principal, allow, seen) => {
                    Ok(repeated(&st, &now, &v))
                }
                _ => Err(e.into()),
            };
        }
        Err(e) => return Err(e.into()),
    }
    let at = Attempt {
        profile: view.profile.clone(),
        run: view.run.clone(),
        claim: None,
        generation: view.generation,
    };
    let verdict = if allow { "allow" } else { "deny" };
    st.extra.decision(
        &at,
        &view.call.0,
        "approval.decide",
        verdict,
        Some(json::risk(view.risk)),
    );
    let after = st.read.approval(&v, nonce).await.ok();
    let body = serde_json::json!({
        "ok": true,
        "approval": after.as_ref().map(|a| json::approval(a, &v)),
    });
    Ok(ok(&body, st.leak.as_ref()))
}

pub(crate) async fn holds(State(st): S, Extension(v): V, RawQuery(raw): RawQuery) -> R {
    operator(&v)?;
    let profile = profile_param(&params(raw.as_deref(), &["profile"])?)?;
    let items = st.read.holds(&v, profile).await?;
    let body = serde_json::json!({"items": items.iter().map(json::hold).collect::<Vec<_>>()});
    Ok(ok(&body, st.leak.as_ref()))
}

/// 보류 판정(운영자 전용). 수신처 영수증을 확인했거나(`confirmed`) 옛 도구 종료와 미실행 증거를 확인했다(`not_executed`).
pub(crate) async fn resolve(
    State(st): S,
    Extension(v): V,
    Path((profile, run, call)): Path<(String, String, String)>,
    bytes: Bytes,
) -> R {
    operator(&v)?;
    let profile = profile_id(&profile).ok_or(INVALID)?;
    let (run, call) = (RunId(identifier(&run)?), CallId(identifier(&call)?));
    let b = body(
        &bytes,
        &["verdict", "receipt", "evidence", "effect", "hold_id"],
    )?;
    // 보류 식별자는 필수다: 화면이 첫 확인에서 본 바로 그 보류에만 판정이 적용된다(같은 대상이 다시 보류돼도 새 보류에는 닿지 않는다).
    let hold_id = HoldId(identifier(
        b.get("hold_id").and_then(Value::as_str).ok_or(INVALID)?,
    )?);
    let verdict = match b.get("verdict").and_then(Value::as_str) {
        Some("confirmed") if !b.contains_key("evidence") => HoldVerdict::Confirmed {
            receipt: text(b.get("receipt"))?,
        },
        Some("not_executed") if !b.contains_key("receipt") => HoldVerdict::NotExecuted {
            evidence: text(b.get("evidence"))?,
        },
        _ => return Err(INVALID),
    };
    let effect = match b.get("effect") {
        None => None,
        Some(e) => Some(EffectId(identifier(e.as_str().ok_or(INVALID)?)?)),
    };
    let hold = HoldRef {
        hold_id,
        profile,
        run,
        call,
        effect,
    };
    let label = match &verdict {
        HoldVerdict::Confirmed { .. } => "confirmed",
        HoldVerdict::NotExecuted { .. } => "not_executed",
    };
    let (profile, run, call) = (hold.profile.clone(), hold.run.clone(), hold.call.clone());
    let decision = st
        .act
        .resolve_hold(v.principal.clone(), hold, verdict)
        .await?;
    // 같은 판정의 반복은 새로 적용되지 않았으므로 판정 스팬도 남기지 않는다(저장소도 감사를 더하지 않는다).
    let repeated = decision == HoldDecision::Repeated;
    if !repeated && let Ok(d) = st.read.detail(&v, profile.clone(), run.clone()).await {
        let at = Attempt {
            profile,
            run,
            claim: None,
            generation: d.run.generation,
        };
        st.extra.decision(&at, &call.0, "hold.resolve", label, None);
    }
    Ok(ok(
        &serde_json::json!({"ok": true, "repeated": repeated}),
        None,
    ))
}

pub(crate) async fn audit(State(st): S, Extension(v): V, RawQuery(raw): RawQuery) -> R {
    operator(&v)?;
    let p = params(raw.as_deref(), &["profile", "before", "limit"])?;
    let query = AuditQuery {
        profile: profile_param(&p)?,
        before: p
            .get("before")
            .map(|s| s.parse::<u64>().map_err(|_| INVALID))
            .transpose()?,
        limit: limit(&p)?,
    };
    // 한 건 더 달라고 해서 다음 쪽이 있는지 정확히 안다(저장소는 최대 201건까지 받는다: 마지막 쪽이 정확히 200건이어도 «더 보기»가 거짓으로 나오지 않는다).
    let wanted = query.limit as usize;
    let query = AuditQuery {
        limit: query.limit.saturating_add(1).min(201),
        ..query
    };
    let mut items = st.read.audit(&v, query).await?;
    let more = items.len() > wanted;
    items.truncate(wanted);
    let next = if more {
        items.last().map(|e| e.seq)
    } else {
        None
    };
    let body = serde_json::json!({"items": items.iter().map(json::audit).collect::<Vec<_>>(), "next": next});
    Ok(ok(&body, st.leak.as_ref()))
}

pub(crate) async fn audit_verify(State(st): S, Extension(v): V) -> R {
    operator(&v)?;
    let check = st.read.verify_audit(&v).await?;
    Ok(ok(&json::check(&check), None))
}

pub(crate) async fn status(State(st): S, Extension(v): V) -> R {
    operator(&v)?;
    let rows = st.read.status(&v).await?;
    let mut profiles = Map::new();
    for s in &rows {
        let mut one = json::status(s);
        if let Some(o) = one.as_object_mut() {
            o.insert("obs".into(), st.extra.obs(&s.profile));
        }
        profiles.insert(s.profile.0.clone(), one);
    }
    let body = serde_json::json!({
        "at_ms": json::ms(SystemTime::now()),
        "build": {"name": "bot", "version": env!("CARGO_PKG_VERSION")},
        "process": {
            "uptime_secs": st.started.elapsed().as_secs(),
            "profiles": rows.iter().map(|s| &s.profile.0).collect::<Vec<_>>(),
            "logs": st.extra.logs_obs(),
            "issues": st.extra.issues_obs(),
        },
        "profiles": profiles,
    });
    Ok(ok(&body, st.leak.as_ref()))
}

/// 구조 지도(운영자 전용): `?profile=`로 하나만, `?rev=`가 지금 구조와 같으면 카드는 생략한다.
pub(crate) async fn map(State(st): S, Extension(v): V, RawQuery(raw): RawQuery) -> R {
    operator(&v)?;
    let p = params(raw.as_deref(), &["profile", "rev"])?;
    let active = st.profiles.active();
    let chosen: std::collections::BTreeMap<ProfileId, Arc<_>> = match profile_param(&p)? {
        Some(id) => {
            let one = active
                .get(&id)
                .ok_or(Fail(StatusCode::NOT_FOUND, "not_found"))?;
            std::collections::BTreeMap::from([(id, one.clone())])
        }
        None => (*active).clone(),
    };
    let m = crate::map::build(&chosen);
    let same = p.get("rev").is_some_and(|r| *r == m.rev);
    let mut body = crate::map::to_json(&m, !same);
    if let Some(o) = body.as_object_mut() {
        let access: Map<String, Value> = chosen
            .iter()
            .map(|(k, s)| (k.0.clone(), crate::map::access(s)))
            .collect();
        o.insert("access".into(), Value::Object(access));
        o.insert("at_ms".into(), json::ms(SystemTime::now()).into());
        o.insert(
            "build".into(),
            serde_json::json!({"name": "bot", "version": env!("CARGO_PKG_VERSION")}),
        );
    }
    Ok(ok(&body, st.leak.as_ref()))
}

pub(crate) async fn not_found() -> Fail {
    Fail(StatusCode::NOT_FOUND, "not_found")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_strings_are_read_strictly() {
        let allowed = ["profile", "limit"];
        let p = params(Some("profile=alpha&limit=20"), &allowed).expect("정상");
        assert_eq!(
            (p["profile"].as_str(), p["limit"].as_str()),
            ("alpha", "20")
        );
        assert!(params(None, &allowed).expect("빈 값").is_empty());
        for bad in [
            "other=1",
            "profile=a&profile=b",
            "profile=%zz",
            "profile=%e3%81",
            "limit=1&=2",
        ] {
            assert!(params(Some(bad), &allowed).is_err(), "{bad}");
        }
        assert_eq!(pct_decode("a%20b+c%E2%9C%93").as_deref(), Some("a b c✓"));
        assert_eq!(pct_decode("%4"), None);
    }

    #[test]
    fn identifiers_and_cursors_are_closed_sets() {
        assert!(token("r-17.a:b_c").is_some() && token("").is_none() && token("a b").is_none());
        assert!(
            token(&"x".repeat(129)).is_none() && token("../x").is_none() && token("a/b").is_none()
        );
        assert!(profile_id("personal").is_some() && profile_id("a-b2").is_some());
        for bad in ["", "Personal", "9a", "a_b", "a.b", "a/b", &"a".repeat(64)] {
            assert!(profile_id(bad).is_none(), "{bad}");
        }
        assert!(
            cursor("abc.DEF_1:2~3=-").is_some() && cursor("a b").is_none() && cursor("").is_none()
        );
    }

    #[test]
    fn limits_default_to_fifty_and_stay_within_bounds() {
        let with = |v: &str| HashMap::from([("limit".to_owned(), v.to_owned())]);
        assert_eq!(limit(&HashMap::new()).ok(), Some(50));
        assert_eq!(limit(&with("1")).ok(), Some(1));
        assert_eq!(limit(&with("200")).ok(), Some(200));
        for bad in ["0", "201", "-1", "x", "", "1.5"] {
            assert!(limit(&with(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn bodies_are_small_objects_with_known_keys_and_one_line_text() {
        let b = |s: &str| Bytes::from(s.to_owned());
        assert!(body(&b(r#"{"allow":true}"#), &["allow", "seen"]).is_ok());
        for bad in [
            r#"{"x":1}"#,
            "[]",
            "true",
            "",
            "{",
            r#"{"allow":true,"x":1}"#,
        ] {
            assert!(body(&b(bad), &["allow", "seen"]).is_err(), "{bad}");
        }
        assert!(body(&Bytes::from(vec![b' '; BODY_MAX + 1]), &["a"]).is_err());
        let one = serde_json::json!("  file-17 ");
        assert_eq!(text(Some(&one)).ok().as_deref(), Some("file-17"));
        let long = serde_json::json!("x".repeat(513));
        for bad in [
            serde_json::json!(""),
            serde_json::json!("  "),
            serde_json::json!("a\nb"),
            serde_json::json!(7),
            long,
        ] {
            assert!(text(Some(&bad)).is_err(), "{bad}");
        }
        assert!(text(None).is_err());
    }
}
