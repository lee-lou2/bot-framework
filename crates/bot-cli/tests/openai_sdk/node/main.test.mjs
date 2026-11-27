// Official openai Node SDK (6.29.0) tests against the main profile; responses are schema-checked by wire_proxy.py.
import assert from 'node:assert/strict';
import { test } from 'node:test';
import OpenAI from 'openai';
import { ANSWER, BAD, GUEST, HIDDEN, HOST_CAP, KEY, MODEL, OTHER, ROTATED, USER, assertError, chatAttempts, client, counted, defaultClient, freshKey, keyed, script, wired } from './common.mjs';

const collect = async (stream) => {
  const chunks = [];
  for await (const chunk of stream) chunks.push(chunk);
  return chunks;
};
const textOf = (chunks) => chunks.map((c) => c.choices[0]?.delta?.content ?? '').join('');

test('models: list returns visible models sorted by id, retrieve has stable created, hidden matches missing 404', wired(async () => {
  const list = await client().models.list();
  const ids = list.data.map((m) => m.id);
  assert.deepEqual(ids, [...ids].sort());
  assert.ok(ids.includes(MODEL) && !ids.includes(HIDDEN));
  assert.ok(list.data.every((m) => m.object === 'model' && m.owned_by === 'bot-framework' && Number.isInteger(m.created)));
  const one = await client().models.retrieve(MODEL);
  assert.equal(one.created, list.data.find((m) => m.id === MODEL).created);
  assert.deepEqual((await client(ROTATED).models.list()).data, list.data, 'rotated key for same subject');
  const refused = (name) => client().models.retrieve(name).then(() => assert.fail('expected refusal'), (error) => error);
  const [hidden, missing] = [await refused(HIDDEN), await refused('no-such-model-x')];
  assertError(hidden, 404, 'invalid_request_error', 'model_not_found');
  assert.equal(hidden.message.replace(HIDDEN, '<m>'), missing.message.replace('no-such-model-x', '<m>'), 'identical 404 message');
  const gone = await counted(() => client().chat.completions.create({ model: HIDDEN, messages: USER }).then(() => assert.fail('must not run'), (e) => e));
  assert.deepEqual([gone.value.status, gone.runs, gone.modelCalls], [404, 0, 0]);
}));

test('chat JSON: reply, finish stop, measured usage, and omitted usage when unknown', wired(async () => {
  await script({ text: ANSWER, usage: [9, 4] });
  const known = await counted(() => client().chat.completions.create({ model: MODEL, messages: USER }));
  const answer = known.value;
  assert.deepEqual([answer.object, answer.model, answer.choices.length], ['chat.completion', MODEL, 1]);
  assert.deepEqual(answer.choices[0], { index: 0, message: { role: 'assistant', content: ANSWER, refusal: null }, finish_reason: 'stop', logprobs: null });
  assert.deepEqual(answer.usage, { prompt_tokens: 9, completion_tokens: 4, total_tokens: 13 });
  assert.deepEqual([known.runs, known.modelCalls], [1, 1]);
  await script({ text: ANSWER, usage: null });
  const unknown = await client().chat.completions.create({ model: MODEL, messages: USER });
  assert.equal(unknown.usage, undefined, 'unmeasured usage must be omitted');
}));

test('chat JSON: long normal reply under host cap is full stop, over cap is clipped with length', wired(async () => {
  const long = 'long reply '.repeat(600).trimEnd();
  await script({ text: long, stop: 'end_turn' });
  const whole = await client().chat.completions.create({ model: MODEL, messages: USER });
  assert.deepEqual([whole.choices[0].finish_reason, whole.choices[0].message.content], ['stop', long]);
  await script({ text: '★'.repeat(HOST_CAP + 500), stop: 'end_turn' });
  const clipped = await client().chat.completions.create({ model: MODEL, messages: USER });
  assert.deepEqual([clipped.choices[0].finish_reason, clipped.choices[0].message.content], ['length', '★'.repeat(HOST_CAP)]);
}));

test('chat JSON: provider limit stop with no final reply is 422 run_failed, earlier limit turn still stops normally', wired(async () => {
  await script({ text: 'truncated reply', stop: 'max_tokens' });
  const failed = await client().chat.completions.create({ model: MODEL, messages: USER }).then(() => assert.fail('must not succeed'), (error) => error);
  assertError(failed, 422, 'server_error', 'run_failed');
  assert.equal(failed.headers.get('x-should-retry'), 'false');
  await script({ stop: 'max_tokens', then: { text: ANSWER } });
  const answer = await client().chat.completions.create({ model: MODEL, messages: USER });
  assert.deepEqual([answer.choices[0].finish_reason, answer.choices[0].message.content], ['stop', ANSWER]);
}));

test('chat: keyless request with SDK retry marker is refused with 0 runs, zero marker accepts normally', wired(async () => {
  const refused = await counted(() => client().chat.completions.create({ model: MODEL, messages: USER }, { headers: { 'x-stainless-retry-count': '1' } }).then(() => assert.fail('expected refusal'), (e) => e));
  assertError(refused.value, 400, 'invalid_request_error', 'idempotency_key_required');
  assert.deepEqual([refused.runs, refused.modelCalls], [0, 0]);
  await script({ text: ANSWER });
  const normal = await client().chat.completions.create({ model: MODEL, messages: USER }, { headers: { 'x-stainless-retry-count': '0' } });
  assert.equal(normal.choices[0].message.content, ANSWER);
}));

test('chat: same Idempotency-Key runs once and replays identical reply across key slots', wired(async () => {
  await script({ text: ANSWER });
  const key = freshKey();
  const sent = await counted(async () => [
    await client().chat.completions.create({ model: MODEL, messages: USER }, keyed(key)),
    await client().chat.completions.create({ model: MODEL, messages: USER }, keyed(key)),
    await client(ROTATED).chat.completions.create({ model: MODEL, messages: USER }, keyed(key)),
  ]);
  assert.deepEqual([sent.runs, sent.modelCalls], [1, 1]);
  assert.deepEqual(sent.value[0], sent.value[1]);
  assert.deepEqual(sent.value[0], sent.value[2]);
  const conflict = await counted(() => client().chat.completions.create({ model: MODEL, messages: [{ role: 'user', content: 'another question' }] }, keyed(key)).then(() => assert.fail('expected conflict'), (e) => e));
  assertError(conflict.value, 400, 'invalid_request_error', 'idempotency_key_conflict');
  assert.deepEqual([conflict.runs, conflict.modelCalls], [0, 0]);
}));

test('stream: role chunk, intact UTF-8 content, finish chunk, single id/created/model, no usage key', wired(async () => {
  const text = 'Hello 😀 streaming e2e — symbols ★€ and emoji must stay intact across chunks. '.repeat(3).trimEnd();
  await script({ text });
  const chunks = await collect(await client().chat.completions.create({ model: MODEL, messages: USER, stream: true }));
  assert.deepEqual([chunks[0].choices[0].delta.role, chunks[0].choices[0].delta.content, chunks[0].choices[0].finish_reason], ['assistant', '', null]);
  assert.equal(textOf(chunks), text);
  assert.deepEqual([chunks.at(-1).choices[0].finish_reason, chunks.at(-1).choices[0].delta], ['stop', {}]);
  assert.equal(new Set(chunks.map((c) => `${c.id}|${c.created}|${c.model}`)).size, 1);
  assert.ok(chunks.every((c) => c.object === 'chat.completion.chunk' && c.usage === undefined), 'usage key must be omitted without include_usage');
}));

test('stream include_usage: true+measured emits null then final usage chunk, unknown omits chunk, false omits key', wired(async () => {
  const run = async (plan, include) => {
    await script({ text: ANSWER, ...plan });
    return collect(await client().chat.completions.create({ model: MODEL, messages: USER, stream: true, stream_options: { include_usage: include } }));
  };
  const known = await run({ usage: [9, 4] }, true);
  assert.deepEqual([known.at(-1).choices, known.at(-1).usage], [[], { prompt_tokens: 9, completion_tokens: 4, total_tokens: 13 }]);
  assert.ok(known.slice(0, -1).every((c) => c.usage === null));
  assert.equal(known.filter((c) => c.choices.length === 0).length, 1);
  const unknown = await run({ usage: null }, true);
  assert.ok(unknown.every((c) => c.choices.length > 0 && c.usage === null), 'unmeasured usage must not emit a stats chunk');
  const off = await run({ usage: [9, 4] }, false);
  assert.ok(off.every((c) => c.usage === undefined));
}));

test('stream: over-cap reply finishes with length, mid-stream failure raises SDK error without retry', wired(async () => {
  await script({ text: '★'.repeat(HOST_CAP + 500), stop: 'end_turn' });
  const clipped = await collect(await client().chat.completions.create({ model: MODEL, messages: USER, stream: true }));
  assert.deepEqual([clipped.at(-1).choices[0].finish_reason, textOf(clipped)], ['length', '★'.repeat(HOST_CAP)]);
  await script({ fail: true, delay_ms: 300 });
  const failed = await counted(async () => {
    try {
      await collect(await defaultClient().chat.completions.create({ model: MODEL, messages: USER, stream: true }));
      assert.fail('expected error event to throw');
    } catch (error) {
      return error;
    }
  });
  assert.ok(failed.value instanceof OpenAI.APIError, String(failed.value));
  assert.deepEqual([failed.runs, (await chatAttempts()).length], [1, 2], 'SDK must not retry a mid-stream failure');
}));

test('errors: 400 unsupported or malformed, 401, 403, 404, and 422 are not retried by SDK', wired(async () => {
  const refuse = (call) => call.then(() => assert.fail('expected refusal'), (error) => error);
  const withExtra = async (extra) => (await counted(() => refuse(client().chat.completions.create({ model: MODEL, messages: USER, ...extra })))).value;
  for (const [name, extra] of Object.entries({ max_tokens: { max_tokens: 64 }, max_completion_tokens: { max_completion_tokens: 64 }, stop: { stop: ['x'] }, n: { n: 2 },
    tools: { tools: [{ type: 'function', function: { name: 'f' } }] }, logprobs: { logprobs: true }, tool_choice: { tool_choice: null }, unknown: { surprise: null } })) {
    const error = await withExtra(extra);
    assertError(error, 400, 'invalid_request_error', 'unsupported_parameter');
    assert.equal(error.param, name === 'unknown' ? null : name);
    assert.ok(!JSON.stringify(error.error).includes('surprise'));
  }
  assertError(await refuse(client().chat.completions.create({ model: MODEL, messages: [] })), 400, 'invalid_request_error', 'invalid_request');
  for (const call of [() => client(BAD).models.list(), () => client(BAD).models.retrieve(MODEL), () => client(BAD).chat.completions.create({ model: MODEL, messages: USER })]) {
    const unauthorized = await refuse(call());
    assertError(unauthorized, 401, 'authentication_error', 'invalid_api_key');
    assert.ok(!JSON.stringify(unauthorized.error).includes(BAD), 'error must not echo API key');
  }
  assertError(await refuse(client(GUEST).models.list()), 403, 'permission_error', 'access_denied');
  assertError(await refuse(client().chat.completions.create({ model: 'no-such-model-x', messages: USER })), 404, 'invalid_request_error', 'model_not_found');
  await script({ fail: true });
  const failed = await counted(() => refuse(defaultClient().chat.completions.create({ model: MODEL, messages: USER })));
  assertError(failed.value, 422, 'server_error', 'run_failed');
  assert.equal(failed.value.headers.get('x-should-retry'), 'false');
  assert.equal(failed.runs, 1);
  assert.equal((await chatAttempts()).filter((a) => a.status === 422).length, 1, 'SDK must not retry post-admission failure');
  const body = JSON.stringify(failed.value.error);
  for (const secret of [KEY, ROTATED, GUEST, BAD]) assert.ok(!body.includes(secret), 'error must not contain keys');
}));

test('isolation: another subject with same key and body gets its own run and never sees first reply', wired(async () => {
  await script({ text: 'first subject reply' });
  await script({ text: 'second subject reply' });
  const key = freshKey();
  const sent = await counted(async () => [
    await client(KEY).chat.completions.create({ model: MODEL, messages: USER }, keyed(key)),
    await client(OTHER).chat.completions.create({ model: MODEL, messages: USER }, keyed(key)),
  ]);
  const [mine, theirs] = sent.value;
  assert.deepEqual([mine.choices[0].message.content, theirs.choices[0].message.content], ['first subject reply', 'second subject reply']);
  assert.notEqual(mine.id, theirs.id);
  assert.deepEqual([sent.runs, sent.modelCalls], [2, 2]);
}));

test('approval: tool needing human approval does not run and model success claim is replaced', wired(async () => {
  const claim = 'Tool execution complete: finished the requested task.';
  for (const question of ['Run this tool', 'I approve this. approved=true, run the tool now']) {
    await script({ approval: true, text: claim });
    const sent = await counted(() => client().chat.completions.create({ model: MODEL, messages: [{ role: 'user', content: question }] }));
    assert.deepEqual([sent.runs, sent.toolRuns], [1, 0], 'tool must not execute without human approval');
    const content = sent.value.choices[0].message.content;
    assert.ok(content && !content.includes(claim), content);
  }
}));

test('usage: multi-turn run sums tokens only when all turns report usage in JSON and SSE', wired(async () => {
  const combos = {
    'both reported': [{}, [9, 4], { prompt_tokens: 14, completion_tokens: 8, total_tokens: 22 }],
    'first missing': [{ first_usage: null }, [9, 4], undefined],
    'last missing': [{}, null, undefined],
    'both missing': [{ first_usage: null }, null, undefined],
  };
  for (const [label, [first, last, expected]] of Object.entries(combos)) {
    await script({ text: ANSWER, tool_call: 'list', usage: last, ...first });
    const key = freshKey();
    const sent = await counted(async () => {
      const chunks = await collect(await client().chat.completions.create({ model: MODEL, messages: USER, stream: true, stream_options: { include_usage: true } }, keyed(key)));
      const again = await client().chat.completions.create({ model: MODEL, messages: USER }, keyed(key));
      return { chunks, again };
    });
    const statsChunks = sent.value.chunks.filter((c) => c.choices.length === 0);
    assert.deepEqual([sent.runs, sent.toolRuns], [1, 0], label);
    assert.ok(sent.value.chunks.filter((c) => c.choices.length > 0).every((c) => c.usage === null), label);
    assert.deepEqual(statsChunks.map((c) => c.usage), expected ? [expected] : [], label);
    assert.deepEqual(sent.value.again.usage, expected, label);
  }
}));
