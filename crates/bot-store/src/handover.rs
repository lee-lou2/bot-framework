//! 리스 만료 인계의 시도 횟수 판정. 호스트의 활성 집합이 교체되는 동안 옛 워커가 반납하지 못해 리스가 끝난 run은 run의 잘못이 아니니
//! 그 인계는 시도를 쓰지 않는다(docs/reliability.md «워커 종료와 리스 인계»).

/// 인계(리스가 끝난 leased를 다시 집음)가 시도 횟수를 쓰는가.
///
/// 직전 claim이 기록한 집합과 지금 집는 핸들이 고정한 집합이 **둘 다 있고 다를 때만** 쓰지 않는다. 같거나 한쪽이라도 없으면(이 열이 생기기 전의 행,
/// 활성 집합을 고정하지 않은 핸들) 한 번 쓴다. 집는 핸들의 집합이 지금도 현재인지는 `begin_authorized`가 같은 트랜잭션에서 `bot_active`를
/// `FOR SHARE`로 읽어 확인하고, 읽지 못하면 아무것도 쓰지 않고 실패한다.
pub(crate) fn spends_attempt(previous: Option<&str>, current: Option<&str>) -> bool {
    !matches!((previous, current), (Some(previous), Some(current)) if previous != current)
}

#[cfg(test)]
mod tests {
    use super::spends_attempt;

    #[test]
    fn only_two_known_and_different_sets_forgive_the_attempt() {
        assert!(!spends_attempt(Some("set-1"), Some("set-2")));
        assert!(spends_attempt(Some("set-1"), Some("set-1")));
        assert!(spends_attempt(None, Some("set-2")), "구판 행");
        assert!(spends_attempt(Some("set-1"), None), "고정하지 않은 핸들");
        assert!(spends_attempt(None, None));
    }
}
