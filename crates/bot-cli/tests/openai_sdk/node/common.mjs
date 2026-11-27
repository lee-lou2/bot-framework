// Shared helpers for official openai Node SDK tests; fails fast if required env vars are missing.
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import OpenAI from 'openai';

const need = (name) => {
  const value = process.env[name];
  assert.ok(value, `${name} is required`);
  return value;
};
export const BASE = need('OPENAI_BASE_URL').replace(/\/$/, '');
const WIRE = need('OPENAI_WIRE_URL').replace(/\/$/, '');
const CONTROL = need('OPENAI_E2E_CONTROL').replace(/\/$/, '');
export const [KEY, ROTATED, GUEST, OTHER] = ['OPENAI_API_KEY', 'OPENAI_ROTATED_KEY', 'OPENAI_GUEST_KEY', 'OPENAI_OTHER_KEY'].map(need);
export const BAD = process.env.OPENAI_BAD_KEY ?? 'wrong-api-key-0000000000';
export const MODEL = process.env.OPENAI_MODEL ?? 'assistant';
export const HIDDEN = process.env.OPENAI_HIDDEN_MODEL ?? 'secret-agent';
export const WAIT_MS = Number(need('OPENAI_WAIT_MS'));
export const USER = [{ role: 'user', content: 'hello e2e' }];
export const ANSWER = 'e2e-answer-ok';
export const HOST_CAP = 12000;
export const NOTICE_PREFIX = 'Request is still in progress.';
export const NOTICE_RETRY = 'Query again with the same Idempotency-Key and request.';

/** SDK client with retries disabled by default; retry tests use defaultClient(). */
export const client = (apiKey = KEY, options = {}) => new OpenAI({ baseURL: BASE, apiKey, maxRetries: 0, timeout: 60_000, ...options });
export const defaultClient = (apiKey = KEY) => new OpenAI({ baseURL: BASE, apiKey, timeout: 60_000 });
export const freshKey = () => `e2e-${randomUUID().replaceAll('-', '')}`;
export const keyed = (key) => ({ headers: { 'Idempotency-Key': key } });

const getJson = async (url) => (await fetch(url)).json();
export const script = async (plan) => {
  const reply = await fetch(`${CONTROL}/script`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(plan) });
  assert.equal(reply.status, 200);
};
export const stats = () => getJson(`${CONTROL}/stats`);
export const settle = async (seconds = 20) => {
  const deadline = Date.now() + seconds * 1000;
  while ((await stats()).active > 0) {
    assert.ok(Date.now() < deadline, 'active runs did not settle');
    await new Promise((done) => setTimeout(done, 100));
  }
};
export const counted = async (work) => {
  const before = await stats();
  const value = await work();
  const after = await stats();
  return { value, runs: after.runs - before.runs, modelCalls: after.model_calls - before.model_calls, toolRuns: after.tool_runs - before.tool_runs };
};
export const chatAttempts = async () => (await getJson(`${WIRE}/log`)).filter((entry) => entry.path.split('?')[0] === '/v1/chat/completions');

/** Resets wire logs around a test, drains unconsumed scripts, and asserts zero new schema/SSE violations. */
export const wired = (work) => async () => {
  await settle();
  const before = (await getJson(`${WIRE}/summary`)).violations.length;
  await fetch(`${WIRE}/reset`, { method: 'POST' });
  let failure;
  try {
    await work();
  } catch (error) {
    failure = error;
  }
  const leftover = (await stats()).queued;
  for (let i = 0; i < leftover; i += 1) await client().chat.completions.create({ model: MODEL, messages: USER }, keyed(freshKey()));
  await settle();
  if (failure) throw failure;
  assert.deepEqual((await getJson(`${WIRE}/summary`)).violations.slice(before), [], 'OpenAPI schema/SSE framing violations');
  assert.equal(leftover, 0, 'queued model script was left unconsumed');
};

export const assertError = (error, status, type, code) => {
  assert.equal(error.status, status);
  assert.deepEqual([error.type, error.code], [type, code], JSON.stringify(error.error));
  assert.deepEqual(Object.keys(error.error).sort(), ['code', 'message', 'param', 'type'], 'error object must contain all four fields');
};
