//! 수집(OTLP) 헤더가 참조하는 자격 이름. `needed_secrets`가 프로필이 선언한 자격과 같이 모은다.
use crate::host::HostFile;
use bot_core::profile::Snapshot;
use std::collections::BTreeSet;

/// 이 프로필의 수집(OTLP) 헤더가 내보낼 때마다 보관소에서 받는 자격 이름(호스트 `admin.trace`). 없으면 내보내기가 매번 실패하고 값은 가림
/// 목록에 미리 들어가지 못하므로, 프로필이 선언한 자격과 같이 시작 전에 점검하고 시작 때 읽어 둔다(`bot validate`·`bot doctor`·`serve`가 같은 목록을 쓴다).
pub fn trace_secrets(snapshot: &Snapshot, host: &HostFile) -> BTreeSet<String> {
    use bot_admin::trace::HeaderValue;
    host.admin
        .trace
        .get(&snapshot.id)
        .and_then(|trace| trace.backend.as_ref())
        .into_iter()
        .flat_map(|backend| &backend.headers)
        .filter_map(|(_, value)| match value {
            HeaderValue::Secret(name) => Some(name.as_str().to_owned()),
            HeaderValue::Plain(_) => None,
        })
        .collect()
}
