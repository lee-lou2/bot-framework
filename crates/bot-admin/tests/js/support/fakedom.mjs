// Minimal zero-dependency DOM and fetch harness for running ui/*.js modules directly under Node.
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
    if (at < 0) throw new Error("reference node is not a child of this parent");
    this.childNodes.splice(at, 0, n);
    n.parentNode = this;
    return n;
  }
  removeChild(n) {
    const i = this.childNodes.indexOf(n);
    if (i < 0) throw new Error("node is not a child of this parent");
    this.childNodes.splice(i, 1);
    n.parentNode = null;
    return n;
  }
  replaceChild(next, old) { this.insertBefore(next, old); return this.removeChild(old); }
  contains(n) { for (let x = n; x; x = x.parentNode) if (x === this) return true; return false; }
  addEventListener(type, fn) { (this.listeners[type] = this.listeners[type] || []).push(fn); }
  removeEventListener(type, fn) { this.listeners[type] = (this.listeners[type] || []).filter((f) => f !== fn); }
  dispatch(type, extra) {
    const ev = { type, target: this, key: undefined, stopped: false, stopPropagation() { this.stopped = true; }, preventDefault() {}, ...extra };
    for (let at = this; at && !ev.stopped; at = at.parentNode) (at.listeners && at.listeners[type] || []).slice().forEach((fn) => fn(ev));
    return ev;
  }
  click() { if (!this.disabled) this.dispatch("click"); }
  key(name) { return this.dispatch("keydown", { key: name }); }
  focus() { globalThis.document.activeElement = this; }
  blur() { if (globalThis.document.activeElement === this) globalThis.document.activeElement = globalThis.document.body; }
}

export const walk = (el, fn) => { fn(el); el.childNodes.forEach((c) => c instanceof Element && walk(c, fn)); };
export function all(el, pred) { const out = []; walk(el, (x) => pred(x) && out.push(x)); return out; }
export const textOf = (el) => el.textContent.replace(/\s+/g, " ").trim();
export const buttons = (el) => all(el, (x) => x.localName === "button");
export const button = (el, label) => buttons(el).find((b) => textOf(b).includes(label)) || null;
export const cardsOf = (el) => all(el, (x) => x.localName === "article");
export const cardFor = (el, name) => cardsOf(el).find((c) => textOf(c.childNodes[0]).includes(name)) || null;

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

export function load(...files) {
  files.forEach((f) => vm.runInThisContext(fs.readFileSync(path.join(UI, f), "utf8"), { filename: f }));
}

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

export async function settle() { for (let i = 0; i < 6; i++) await new Promise((r) => setImmediate(r)); }
export const deferred = () => { let resolve, reject; const promise = new Promise((a, b) => { resolve = a; reject = b; }); return { promise, resolve, reject }; };
