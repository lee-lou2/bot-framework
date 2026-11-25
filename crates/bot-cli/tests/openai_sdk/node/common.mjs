// 공식 openai Node SDK 시험의 공통 도구: 환경, SDK 클라이언트, 모델 제어·관찰 창구, 프록시 기록. 환경이 빠지면 실패한다(건너뛰지 않는다).
// 환경은 파이썬 시험과 같다: OPENAI_BASE_URL(프록시 …/v1) · OPENAI_WIRE_URL · OPENAI_E2E_CONTROL · OPENAI_API_KEY · OPENAI_ROTATED_KEY · OPENAI_GUEST_KEY · OPENAI_OTHER_KEY · OPENAI_WAIT_MS
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import OpenAI from 'openai';

const need = (name) => {
  const value = process.env[name];
  assert.ok(value, `${name}이(가) 필요하다`);
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
export const HOST_CAP = 12000; // 최종 글의 호스트 상한(글자 수): 넘으면 잘리고 finish_reason은 length
export const NOTICE_PREFIX = '작업을 처리 중입니다.';
export const NOTICE_RETRY = '같은 Idempotency-Key와 같은 요청으로 다시 조회하세요.';

/** SDK 클라이언트. 기본은 재시도를 끈다(`maxRetries: 0`) — 재시도를 보는 시험은 `defaultClient()`를 쓴다. */
export const client = (apiKey = KEY, options = {}) => new OpenAI({ baseURL: BASE, apiKey, maxRetries: 0, timeout: 60_000, ...options });
export const defaultClient = (apiKey = KEY) => new OpenAI({ baseURL: BASE, apiKey, timeout: 60_000 }); // SDK 기본: maxRetries 2
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
    assert.ok(Date.now() < deadline, '작업이 끝나지 않았다');
    await new Promise((done) => setTimeout(done, 100));
  }
};
/** 함수를 돌리는 동안 서버 쪽에서 늘어난 작업 수(`runs`)와 모델 호출 수(`modelCalls`) */
export const counted = async (work) => {
  const before = await stats();
  const value = await work();
  const after = await stats();
  return { value, runs: after.runs - before.runs, modelCalls: after.model_calls - before.model_calls, toolRuns: after.tool_runs - before.tool_runs };
};
export const chatAttempts = async () => (await getJson(`${WIRE}/log`)).filter((entry) => entry.path.split('?')[0] === '/v1/chat/completions');

/** 시험 하나: 프록시 기록을 비우고, 끝나면 소비되지 않은 모델 응답을 비운 뒤(다음 시험으로 새지 않게) 그 사이 공식 스키마·SSE 틀 위반이 새로 생기지 않았는지, 모델 응답이 남지 않았는지 본다. */
export const wired = (work) => async () => {
  await settle(); // 앞 시험이 남긴 진행 중 작업이 이 시험의 한도·수를 흔들지 않게 한다
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
  assert.deepEqual((await getJson(`${WIRE}/summary`)).violations.slice(before), [], '공식 OpenAPI 스키마·SSE 틀 위반');
  assert.equal(leftover, 0, '시험이 쌓은 모델 응답이 소비되지 않고 남았다(요청이 모델까지 가지 않았다)');
};

export const assertError = (error, status, type, code) => {
  assert.equal(error.status, status);
  assert.deepEqual([error.type, error.code], [type, code], JSON.stringify(error.error));
  assert.deepEqual(Object.keys(error.error).sort(), ['code', 'message', 'param', 'type'], '오류 객체의 네 필드(null 포함)');
};
