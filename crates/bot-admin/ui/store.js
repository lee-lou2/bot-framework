// Shared cache for /v1/admin/map used by the Overview, Structure, and Access views.
(function (root) {
  "use strict";
  const A = root.BotApi;
  const map = { rev: null, nodes: null, access: null, at: 0, build: null, profile: null };

  function reset() { map.rev = null; map.nodes = null; map.access = null; map.at = 0; map.build = null; map.profile = null; }

  // Server omits graph when rev matches, so keep cached nodes across unchanged polls for the same profile scope.
  function load(ctx) {
    const scope = ctx.profile || "";
    if (map.profile !== scope) reset();
    const params = { profile: scope };
    if (map.nodes && map.rev) params.rev = map.rev;
    return A.get("/v1/admin/map" + A.q(params), { signal: ctx.signal }).then((d) => {
      if (!ctx.current()) return { changed: false, stale: true };
      let changed = false;
      map.profile = scope;
      if (d.graph) { map.nodes = d.graph.nodes; map.rev = d.rev; changed = true; }
      map.access = d.access || {};
      map.at = d.at_ms || 0;
      map.build = d.build || null;
      return { changed, stale: false };
    });
  }

  root.BotStore = { map, reset, load };
})(window);
