-- 보내지 않고 끝낸 전송 초안(미발송 종결). 수신자가 현재 운영자 등록부에서 철회됐거나(unrouted) 소유자가 전송을 거절·철회했다(rejected).
-- 종결한 초안의 본문(최종 결과·답)은 지우고 지문·집합 판본만 남긴다. 영수증은 종결한 커밋에서 한 번 만들고 같은 바이트로 다시 돌려준다.
alter table bot_deliveries
    add column unsent text check (unsent in ('rejected', 'unrouted')),
    add column unsent_receipt text,
    -- 종결한 때의 호스트 활성 집합 판본(영수증을 읽는 쪽이 지금 집합과 대조한다)
    add column unsent_activation text,
    -- 카드 앞에서 종결했다면 그 카드의 nonce(같은 종결의 재전달을 알아본다)
    add column unsent_nonce text,
    add column unsent_ms bigint,
    add constraint bot_deliveries_unsent_shape check (
        (unsent is null) = (unsent_receipt is null)
        and (unsent is null) = (unsent_activation is null)
        and (unsent is null or state = 'dropped')
    );
