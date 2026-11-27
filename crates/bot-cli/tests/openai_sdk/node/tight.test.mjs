// Official openai Node SDK (6.29.0) tests against the tight profile (short wait, max_active_runs=1); attempt counts come from wire_proxy.py.
import assert from 'node:assert/strict';
import { test } from 'node:test';
import OpenAI from 'openai';
import { ANSWER, KEY, MODEL, NOTICE_PREFIX, NOTICE_RETRY, USER, WAIT_MS, assertError, chatAttempts, client, counted, defaultClient, freshKey, keyed, script, settle, stats, wired } from './common.mjs';

const refuse = (call) => call.then(() => assert.fail('expected refusal'), (error) => error);
const occupied = (work) => wired(async () => {
  await settle();
  await script({ text: ANSWER, delay_ms: WAIT_MS + 8000 });
  const holder = freshKey();
  const first = await client().chat.completions.create({ model: MODEL, messages: USER }, keyed(holder));
  assert.ok(first.choices[0].message.content.startsWith(NOTICE_PREFIX), 'holder run must still be active');
  assert.equal((await stats()).active, 1);
  await fetch(`${process.env.OPENAI_WIRE_URL}/reset`, { method: 'POST' });
  try {
    await work(holder);
  } finally {
    await settle();
  }
});

test('wait: deadline returns 200 notice with stop and no usage, same key collects final reply without new run', wired(async () => {
  await settle();
  await script({ text: ANSWER, delay_ms: WAIT_MS + 2500 });
  const key = freshKey();
  const sent = await counted(async () => {
    const began = Date.now();
    const notice = await client().chat.completions.create({ model: MODEL, messages: USER }, keyed(key));
    assert.ok(Date.now() - began < WAIT_MS + 1500, 'must return within wait deadline');
    const text = notice.choices[0].message.content;
    assert.ok(text.startsWith(NOTICE_PREFIX) && text.includes(NOTICE_RETRY), text);
    assert.deepEqual([notice.choices[0].finish_reason, notice.usage], ['stop', undefined]);
    assert.equal((await stats()).active, 1, 'run must keep executing after deadline');
    await settle();
    return [notice, await client().chat.completions.create({ model: MODEL, messages: USER }, keyed(key)), await client().chat.completions.create({ model: MODEL, messages: USER }, keyed(key))];
  });
  const [notice, answer, again] = sent.value;
  assert.deepEqual([answer.choices[0].message.content, answer.usage.total_tokens], [ANSWER, 13]);
  assert.deepEqual(again, answer);
  assert.equal(answer.id, notice.id, 'id stays stable for the same run');
  assert.deepEqual([sent.runs, sent.modelCalls], [1, 1], 're-querying must not create a new run');
}));

test('saturation: explicit key 429 retries 3 times on wire (0, 1, 2) with 0 new runs', occupied(async () => {
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

test('saturation: keyless 429 followed by SDK resend is 400 idempotency_key_required with 0 new runs', occupied(async () => {
  const sent = await counted(() => refuse(defaultClient().chat.completions.create({ model: MODEL, messages: USER })));
  assert.ok(sent.value instanceof OpenAI.BadRequestError, String(sent.value));
  assertError(sent.value, 400, 'invalid_request_error', 'idempotency_key_required');
  const attempts = await chatAttempts();
  assert.deepEqual(attempts.map((a) => a.status), [429, 400]);
  assert.deepEqual(attempts.map((a) => a.req['x-stainless-retry-count']), ['0', '1']);
  assert.ok(attempts.every((a) => !('idempotency-key' in a.req)));
  assert.deepEqual([sent.runs, sent.modelCalls], [0, 0]);
}));

test('saturation: retries off exposes 429 with Retry-After 1 and 400 for marked keyless request', occupied(async () => {
  const sent = await counted(async () => {
    const limited = await refuse(client().chat.completions.create({ model: MODEL, messages: USER }));
    assert.equal(limited.headers.get('retry-after'), '1');
    assertError(limited, 429, 'rate_limit_error', 'rate_limit_exceeded');
    const marked = await refuse(client().chat.completions.create({ model: MODEL, messages: USER }, { headers: { 'x-stainless-retry-count': '1' } }));
    assertError(marked, 400, 'invalid_request_error', 'idempotency_key_required');
  });
  assert.deepEqual([sent.runs, sent.modelCalls], [0, 0]);
}));

test('saturation: already accepted key still queries existing run without 429 or new slot', occupied(async (holder) => {
  const sent = await counted(() => defaultClient().chat.completions.create({ model: MODEL, messages: USER }, keyed(holder)));
  assert.ok(sent.value.choices[0].message.content.startsWith(NOTICE_PREFIX));
  assert.deepEqual((await chatAttempts()).map((a) => a.status), [200]);
  assert.deepEqual([sent.runs, sent.modelCalls], [0, 0]);
}));

test('saturation: once slot frees, same explicit key runs exactly once', occupied(async () => {
  const key = freshKey();
  await refuse(client().chat.completions.create({ model: MODEL, messages: USER }, keyed(key)));
  await settle();
  await script({ text: ANSWER });
  const sent = await counted(() => defaultClient().chat.completions.create({ model: MODEL, messages: USER }, keyed(key)));
  assert.deepEqual([sent.value.choices[0].message.content, sent.runs, sent.modelCalls], [ANSWER, 1, 1]);
}));
