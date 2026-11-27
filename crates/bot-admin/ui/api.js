// Same-origin Admin API client, CSRF token refresh, and fixed vocabulary error copy.
(function (root) {
  "use strict";
  const D = root.BotDom;
  const state = { me: null, req: null };

  class ApiError extends Error {
    constructor(status, word) { super(word); this.name = "ApiError"; this.status = status; this.word = word; }
  }

  const SENTENCE = {
    unauthorized: "Authentication required. Enter your credentials in the login prompt.",
    rate_limited: "Temporarily blocked after too many failures. Try again shortly.",
    host: "Opened on a disallowed host. Reopen using the advertised address.",
    csrf: "Security check failed. Reload the page and try again.",
    denied: "You do not have permission to perform this action.",
    not_found: "Not found. It may already have been processed or is not visible to you.",
    invalid: "Invalid request payload.",
    conflict: "This item has already been processed.",
    stale: "Already processed or conditions changed; a new approval is required.",
    changed: "Content changed. Please review again.",
    unavailable: "Could not reach the store. Try again shortly.",
    leak: "Response suppressed because it contained a secret value.",
    too_large: "Response is too large to display.",
    network: "Could not reach the server.",
  };
  const DECIDE = { denied: "Only the designated approver can act on this request.", no_body: "Cannot approve because the full content matching the approval fingerprint cannot be verified on the card. You can still deny it.", not_found: "Approval request not found. It may already have been processed." };
  const RESOLVE = { conflict: "This hold was already resolved. Reloading the list.", denied: "Only operators can resolve holds.", not_found: "Hold not found. It may already have been resolved.", invalid: "Check your input. It must be a single line (1 to 512 characters)." };

  // Browsers reject fetch() when user:pass@ is embedded in the URL; location strips credentials, but document.baseURI keeps them.
  function networkMessage() {
    try {
      const here = new URL(document.baseURI);
      if (here.username || here.password) return "Requests are blocked when the URL contains embedded credentials. Remove them from the address bar and reopen.";
    } catch (e) { /* fall back to generic network copy */ }
    return SENTENCE.network;
  }
  const byStatus = (s) => (s === 401 ? "unauthorized" : s === 403 ? "denied" : s === 404 ? "not_found" : s === 429 ? "rate_limited" : s >= 500 ? "unavailable" : "invalid");

  function explain(e, over) {
    if (!(e instanceof ApiError)) return "An unknown error occurred.";
    if (over && over[e.word]) return over[e.word];
    if (e.word === "network") return networkMessage();
    return SENTENCE[e.word] || "Request failed (" + e.status + ").";
  }

  function read(res) {
    const date = Date.parse(res.headers.get("Date") || "");
    if (!isNaN(date)) D.setSkew(date - Date.now());
    return res.text().then((body) => {
      let data = null;
      try { data = body ? JSON.parse(body) : null; } catch (e) { data = null; }
      if (res.ok) return data;
      throw new ApiError(res.status, (data && typeof data.error === "string" && data.error) || byStatus(res.status));
    });
  }
  function request(method, path, o) {
    o = o || {};
    const headers = { Accept: "application/json" };
    const init = { method, cache: "no-store", credentials: "same-origin", headers, signal: o.signal };
    if (method !== "GET") {
      headers["Content-Type"] = "application/json";
      headers["X-Bot-Csrf"] = state.me ? state.me.csrf : "";
      init.body = JSON.stringify(o.body || {});
    }
    return fetch(path, init).then(read, (e) => { throw e && e.name === "AbortError" ? e : new ApiError(0, "network"); });
  }

  function me(force) {
    if (state.me && !force) return Promise.resolve(state.me);
    if (!state.req) state.req = request("GET", "/v1/admin/me").then((d) => { state.me = d; state.req = null; return d; }, (e) => { state.req = null; throw e; });
    return state.req;
  }
  const get = (path, o) => request("GET", path, o);
  // Retry once on 403 csrf after refreshing /me; the server rejects invalid CSRF before executing the action.
  function post(path, body, o) {
    const signal = o && o.signal;
    return request("POST", path, { body, signal }).catch((e) => {
      if (!(e instanceof ApiError) || e.status !== 403 || e.word !== "csrf") throw e;
      return me(true).then(() => request("POST", path, { body, signal }));
    });
  }
  const q = (params) => {
    const out = [];
    Object.keys(params).forEach((k) => { const v = params[k]; if (v !== null && v !== undefined && v !== "") out.push(k + "=" + encodeURIComponent(v)); });
    return out.length ? "?" + out.join("&") : "";
  };
  const seg = encodeURIComponent;

  root.BotApi = { ApiError, me, get, post, q, seg, explain, DECIDE, RESOLVE, SENTENCE, get current() { return state.me; } };
})(window);
