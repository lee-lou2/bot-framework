// 구조 지도 자료: `/v1/admin/map`을 한 곳에서 받아 구조 화면과 권한 화면이 함께 쓴다.
// 구조가 그대로면(`rev`가 같으면) 서버가 카드를 생략하므로, 마지막으로 받은 카드를 그대로 들고 있다.
(function (root) {
  "use strict";
  const A = root.BotApi;
  const map = { rev: null, nodes: null, access: null, at: 0, build: null };

  function reset() { map.rev = null; map.nodes = null; map.access = null; map.at = 0; map.build = null; }

  // ctx: { profile, signal, current() }. 반환: { changed(새 카드를 받았는가), stale(낡은 응답이라 버렸는가) }
  function load(ctx) {
    const params = { profile: ctx.profile };
    if (map.nodes && map.rev) params.rev = map.rev;
    return A.get("/v1/admin/map" + A.q(params), { signal: ctx.signal }).then((d) => {
      if (!ctx.current()) return { changed: false, stale: true };
      let changed = false;
      if (d.graph) { map.nodes = d.graph.nodes; map.rev = d.rev; changed = true; }
      map.access = d.access || {};
      map.at = d.at_ms || 0;
      map.build = d.build || null;
      return { changed, stale: false };
    });
  }

  root.BotStore = { map, reset, load };
})(window);
