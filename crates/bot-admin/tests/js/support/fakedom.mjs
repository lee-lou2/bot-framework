// 노드에서 화면 모듈(ui/*.js)을 «실제로» 돌리는 아주 작은 DOM 모조품. 브라우저·외부 패키지 없이 화면 코드(dom.js·api.js·탭 모듈)를 그대로 불러
// 눌러 보고(click) 글자·속성·단추 상태를 읽고, 가짜 fetch로 서버 응답을 흉내 낸다. 화면이 쓰는 DOM 기능만 구현한다(선택자 엔진 없음).
import fs from "node:fs";
import path from "node:path";
import vm from "node:vm";
import { fileURLToPath } from "node:url";

const UI = path.join(path.dirname(fileURLToPath(import.meta.url)), "../../../ui");

class Node {}
class Text extends Node {
  constructor(data) { super(); this.data = String(data); this.parentNode = null; }
  get textContent() { return this.data; }
}

class Element extends Node {
  constructor(tag) {
    super();
    this.tagName = tag.toUpperCase();
    this.localName = tag;
    this.attrs = {};
    this.childNodes = [];
    this.parentNode = null;
    this.listeners = {};
    this.style = { props: {}, setProperty(k, v) { this.props[k] = String(v); }, getPropertyValue(k) { return this.props[k] || ""; } };
    const self = this;
    this.classList = {
      add(...n) { const c = self.classes(); n.forEach((x) => c.add(x)); self.attrs.class = [...c].join(" "); },
      remove(...n) { const c = self.classes(); n.forEach((x) => c.delete(x)); self.attrs.class = [...c].join(" "); },
      toggle(n, force) { const on = force === undefined ? !self.classes().has(n) : !!force; this[on ? "add" : "remove"](n); return on; },
      contains(n) { return self.classes().has(n); },
    };
  }
  classes() { return new Set((this.attrs.class || "").split(/\s+/).filter(Boolean)); }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return k in this.attrs ? this.attrs[k] : null; }
  removeAttribute(k) { delete this.attrs[k]; }
  hasAttribute(k) { return k in this.attrs; }
  get disabled() { return this.hasAttribute("disabled"); }
  set disabled(v) { if (v) this.setAttribute("disabled", ""); else this.removeAttribute("disabled"); }
  get id() { return this.getAttribute("id") || ""; }
  get firstChild() { return this.childNodes[0] || null; }
  get nextSibling() { const p = this.parentNode; return p ? p.childNodes[p.childNodes.indexOf(this) + 1] || null : null; }
  get children() { return this.childNodes.filter((n) => n instanceof Element); }
  get textContent() { return this.childNodes.map((n) => n.textContent).join(""); }
  set textContent(v) {
    this.childNodes.forEach((n) => { n.parentNode = null; });
    this.childNodes = [];
    if (v !== "" && v !== null && v !== undefined) this.appendChild(new Text(v));
  }
  detach(n) { if (n.parentNode) n.parentNode.removeChild(n); }
  appendChild(n) { return this.insertBefore(n, null); }
  insertBefore(n, ref) {
    this.detach(n);
    const at = ref ? this.childNodes.indexOf(ref) : this.childNodes.length;
    if (at < 0) throw new Error("기준 노드가 이 부모의 자식이 아니다");
    this.childNodes.splice(at, 0, n);
    n.parentNode = this;
    return n;
  }
  removeChild(n) {
    const i = this.childNodes.indexOf(n);
    if (i < 0) throw new Error("자식이 아니다");
    this.childNodes.splice(i, 1);
    n.parentNode = null;
    return n;
  }
  replaceChild(next, old) { this.insertBefore(next, old); return this.removeChild(old); }
  contains(n) { for (let x = n; x; x = x.parentNode) if (x === this) return true; return false; }
  addEventListener(type, fn) { (this.listeners[type] = this.listeners[type] || []).push(fn); }
  removeEventListener(type, fn) { this.listeners[type] = (this.listeners[type] || []).filter((f) => f !== fn); }
  // 위로 거품이 올라가는 사건: 처리기가 stopPropagation을 부르면 거기서 멈춘다.
  dispatch(type, extra) {
    const ev = { type, target: this, key: undefined, stopped: false, stopPropagation() { this.stopped = true; }, preventDefault() {}, ...extra };
    for (let at = this; at && !ev.stopped; at = at.parentNode) (at.listeners && at.listeners[type] || []).slice().forEach((fn) => fn(ev));
    return ev;
  }
  // 사용자가 누른 것처럼: 꺼진 단추는 아무 일도 없다.
  click() { if (!this.disabled) this.dispatch("click"); }
  key(name) { return this.dispatch("keydown", { key: name }); }
  focus() { globalThis.document.activeElement = this; }
  blur() { if (globalThis.document.activeElement === this) globalThis.document.activeElement = globalThis.document.body; }
}

// ---------- 찾기 도우미(시험이 쓴다) ----------
export const walk = (el, fn) => { fn(el); el.childNodes.forEach((c) => c instanceof Element && walk(c, fn)); };
export function all(el, pred) { const out = []; walk(el, (x) => pred(x) && out.push(x)); return out; }
export const textOf = (el) => el.textContent.replace(/\s+/g, " ").trim();
export const buttons = (el) => all(el, (x) => x.localName === "button");
export const button = (el, label) => buttons(el).find((b) => textOf(b).includes(label)) || null;
export const cardsOf = (el) => all(el, (x) => x.localName === "article");
export const cardFor = (el, name) => cardsOf(el).find((c) => textOf(c.childNodes[0]).includes(name)) || null;

// ---------- 설치 ----------
// 화면 모듈이 기대하는 전역(window·document·Node·fetch 등)을 만든다. 시험 하나가 한 프로세스이므로 전역을 더럽혀도 된다.
export function install() {
  const body = new Element("body");
  const doc = {
    body, activeElement: body, visibilityState: "visible", baseURI: ["http:", "", "127.0.0.1", "ui"].join("/"),
    createElement: (t) => new Element(t),
    createElementNS: (_ns, t) => new Element(t),
    createTextNode: (t) => new Text(t),
    getElementById: (id) => all(body, (x) => x.id === id)[0] || null,
    querySelectorAll: () => [],
  };
  Object.assign(globalThis, { document: doc, Node, window: globalThis });
  return { document: doc, body };
}

// 화면 파일을 순서대로 같은 전역에서 실행한다(브라우저의 <script> 순서와 같다).
export function load(...files) {
  files.forEach((f) => vm.runInThisContext(fs.readFileSync(path.join(UI, f), "utf8"), { filename: f }));
}

// ---------- 가짜 서버(fetch) ----------
// handler({method, path, headers, body}) → {status, body, headers?} 또는 그것의 Promise. 응답이 오기 전에 signal이 끊기면 AbortError로 거절한다.
export function fakeFetch(handler) {
  const calls = [];
  async function fetchImpl(url, init) {
    init = init || {};
    const call = { method: init.method || "GET", path: url, headers: init.headers || {}, body: init.body === undefined ? null : JSON.parse(init.body) };
    calls.push(call);
    const abort = new Promise((_, reject) => {
      const stop = () => reject(Object.assign(new Error("aborted"), { name: "AbortError" }));
      if (init.signal && init.signal.aborted) stop();
      else if (init.signal) init.signal.addEventListener("abort", stop);
    });
    const res = await Promise.race([Promise.resolve(handler(call)), abort]);
    const text = res.body === undefined ? "" : typeof res.body === "string" ? res.body : JSON.stringify(res.body);
    const headers = res.headers || {};
    return { ok: res.status >= 200 && res.status < 300, status: res.status, headers: { get: (n) => headers[n.toLowerCase()] || null }, text: async () => text };
  }
  fetchImpl.calls = calls;
  return fetchImpl;
}

// 눈에 보일 일이 모두 끝나도록 마이크로태스크와 한 번의 이벤트 루프를 비운다.
export async function settle() { for (let i = 0; i < 6; i++) await new Promise((r) => setImmediate(r)); }
export const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
