//! 수집(OTLP) 설정의 지문. 호스트의 활성화 지문에 섞어, 수집을 거두거나 목적지·내용·식별자 정책을 바꾸는 것을 **활성화 사건**으로 만든다 —
//! 그러면 옛 설정으로 남은 프로세스는 구판이 되어 그 프로세스의 내보내기 문(`Exporter::set_gate`)이 닫힌다.
//! 내보내기에 영향이 없는 값(백엔드 이름·화면 링크 틀)과 자격 값은 넣지 않는다. 평문 헤더 값은 토큰일 수 있어 값의 해시만 섞는다.

use super::{HeaderValue, Ids, ProfileTrace};
use bot_core::id::ProfileId;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

/// 내보내는 프로필(백엔드가 있는 것)이 하나도 없으면 `None`이다 — 수집을 쓰지 않는 호스트의 활성화 판본은 달라지지 않는다.
/// 백엔드가 없는 프로필은 아무것도 내보내지 않으니 그 프로필의 식별자·내용·표본 설정은 지문에 들어가지 않는다.
pub fn fingerprint(config: &BTreeMap<ProfileId, ProfileTrace>) -> Option<[u8; 32]> {
    let exporting: Vec<_> = config
        .iter()
        .filter_map(|(profile, trace)| Some((profile, trace, trace.backend.as_ref()?)))
        .collect();
    if exporting.is_empty() {
        return None;
    }
    let mut hash = Sha256::new();
    hash.update(b"bot-trace-authority-v1\0");
    hash.update((exporting.len() as u64).to_be_bytes());
    for (profile, trace, backend) in exporting {
        field(&mut hash, profile.0.as_bytes());
        field(&mut hash, backend.endpoint.as_bytes());
        hash.update((backend.headers.len() as u64).to_be_bytes());
        for (name, value) in &backend.headers {
            field(&mut hash, name.as_bytes());
            match value {
                HeaderValue::Plain(text) => {
                    hash.update([0]);
                    field(&mut hash, &Sha256::digest(text.as_bytes()));
                }
                HeaderValue::Secret(secret) => {
                    hash.update([1]);
                    field(&mut hash, secret.as_str().as_bytes());
                }
            }
        }
        hash.update([match trace.ids {
            Ids::Plain => 0,
            Ids::Hash => 1,
        }]);
        hash.update([u8::from(trace.content)]);
        // -0.0과 0.0은 같은 표본이다
        hash.update((trace.sample + 0.0).to_bits().to_be_bytes());
    }
    Some(hash.finalize().into())
}
