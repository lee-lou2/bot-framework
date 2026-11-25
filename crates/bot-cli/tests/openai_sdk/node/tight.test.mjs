// 공식 openai Node SDK(6.29.0)로 짧은 대기·실행 한도 1인 입구를 부른다: 기한이 끝나면 200 안내와 같은 키로 다시 조회, 포화 429의 SDK 기본 재시도 **실제 wire 시도 수**와 run 0,
// 키 없는 재전송은 400, 이미 접수한 키는 포화 중에도 같은 작업. 시도 수는 프록시가 센다.
import assert from 'node:assert/strict';
import { test } from 'node:test';
import OpenAI from 'openai';
import { ANSWER, KEY, MODEL, NOTICE_PREFIX, NOTICE_RETRY, USER, WAIT_MS, assertError, chatAttempts, client, counted, defaultClient, freshKey, keyed, script, settle, stats, wired } from './common.mjs';

const refuse = (call) => call.then(() => assert.fail('거절돼야 한다'), (error) => error);
const occupied = (work) => wired(async () => {
  await settle();
  await script({ text: ANSWER, delay_ms: WAIT_MS + 8000 });
  const holder = freshKey();
  const first = await client().chat.completions.create({ model: MODEL, messages: USER }, keyed(holder));
  assert.ok(first.choices[0].message.content.startsWith(NOTICE_PREFIX), '자리를 차지한 작업이 아직 돈다');
  assert.equal((await stats()).active, 1);
  await fetch(`${process.env.OPENAI_WIRE_URL}/reset`, { method: 'POST' });
  try {
    await work(holder);
  } finally {
    await settle();
  }
});

test('wait: 기한이 끝나면 200 안내(stop, usage 없음), 같은 키·같은 요청으로 다시 조회하면 새 작업 없이 최종 답', wired(async () => {
  await settle();
  await script({ text: ANSWER, delay_ms: WAIT_MS + 2500 });
  const key = freshKey();
  const sent = await counted(async () => {
    const began = Date.now();
    const notice = await client().chat.completions.create({ model: MODEL, messages: USER }, keyed(key));
    assert.ok(Date.now() - began < WAIT_MS + 1500, '기한을 넘겨 붙잡지 않는다');
    const text = notice.choices[0].message.content;
    assert.ok(text.startsWith(NOTICE_PREFIX) && text.includes(NOTICE_RETRY), text);
    assert.deepEqual([notice.choices[0].finish_reason, notice.usage], ['stop', undefined]);
    assert.equal((await stats()).active, 1, '작업은 계속 돈다');
    await settle();
    return [notice, await client().chat.completions.create({ model: MODEL, messages: USER }, keyed(key)), await client().chat.completions.create({ model: MODEL, messages: USER }, keyed(key))];
  });
  const [notice, answer, again] = sent.value;
  assert.deepEqual([answer.choices[0].message.content, answer.usage.total_tokens], [ANSWER, 13]);
  assert.deepEqual(again, answer);
  assert.equal(answer.id, notice.id, 'id는 같은 실행에서 고정');
  assert.deepEqual([sent.runs, sent.modelCalls], [1, 1], '다시 조회는 새 작업이 아니다');
}));

test('포화: 명시 키의 429는 SDK 기본 재시도로 wire에서 정확히 3번(0·1·2) 가고 새 run은 0', occupied(async () => {
  const key = freshKey();
  const sent = await counted(() => refuse(defaultClient().chat.completions.create({ model: MODEL, messages: USER }, keyed(key))));
  assert.ok(sent.value instanceof OpenAI.RateLimitError, String(sent.value));
  assertError(sent.value, 429, 'rate_limit_error', 'rate_limit_exceeded');
  const attempts = await chatAttempts();
  assert.deepEqual(attempts.map((a) => a.status), [429, 429, 429]);
  assert.deepEqual(attempts.map((a) => a.req['x-stainless-retry-count']), ['0', '1', '2']);
  assert.deepEqual([...new Set(attempts.map((a) => a.req['idempotency-key']))], [key]);
  assert.deepEqual([...new Set(attempts.map((a) => a.res['retry-after']))], ['1']);
  assert.deepEqual([sent.runs, sent.modelCalls], [0, 0]);
}));

test('포화: 키 없는 429 뒤 SDK 재전송은 400 idempotency_key_required이고 새 run은 0', occupied(async () => {
  const sent = await counted(() => refuse(defaultClient().chat.completions.create({ model: MODEL, messages: USER })));
  assert.ok(sent.value instanceof OpenAI.BadRequestError, String(sent.value));
  assertError(sent.value, 400, 'invalid_request_error', 'idempotency_key_required');
  const attempts = await chatAttempts();
  assert.deepEqual(attempts.map((a) => a.status), [429, 400]);
  assert.deepEqual(attempts.map((a) => a.req['x-stainless-retry-count']), ['0', '1']);
  assert.ok(attempts.every((a) => !('idempotency-key' in a.req)));
  assert.deepEqual([sent.runs, sent.modelCalls], [0, 0]);
}));

test('포화: 재시도를 끄면 상태 코드를 바로 본다(429+Retry-After 1, 재시도 표지가 붙은 키 없는 요청은 400)', occupied(async () => {
  const sent = await counted(async () => {
    const limited = await refuse(client().chat.completions.create({ model: MODEL, messages: USER }));
    assert.equal(limited.headers.get('retry-after'), '1');
    assertError(limited, 429, 'rate_limit_error', 'rate_limit_exceeded');
    const marked = await refuse(client().chat.completions.create({ model: MODEL, messages: USER }, { headers: { 'x-stainless-retry-count': '1' } }));
    assertError(marked, 400, 'invalid_request_error', 'idempotency_key_required');
  });
  assert.deepEqual([sent.runs, sent.modelCalls], [0, 0]);
}));

test('포화: 이미 접수한 키는 포화 중에도 429 없이 같은 작업을 조회한다(새 슬롯을 쓰지 않는다)', occupied(async (holder) => {
  const sent = await counted(() => defaultClient().chat.completions.create({ model: MODEL, messages: USER }, keyed(holder)));
  assert.ok(sent.value.choices[0].message.content.startsWith(NOTICE_PREFIX));
  assert.deepEqual((await chatAttempts()).map((a) => a.status), [200]);
  assert.deepEqual([sent.runs, sent.modelCalls], [0, 0]);
}));

test('포화: 자리가 비면 같은 명시 키가 정확히 한 번 실행된다', occupied(async () => {
  const key = freshKey();
  await refuse(client().chat.completions.create({ model: MODEL, messages: USER }, keyed(key)));
  await settle();
  await script({ text: ANSWER });
  const sent = await counted(() => defaultClient().chat.completions.create({ model: MODEL, messages: USER }, keyed(key)));
  assert.deepEqual([sent.value.choices[0].message.content, sent.runs, sent.modelCalls], [ANSWER, 1, 1]);
}));
