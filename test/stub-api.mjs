// A local stand-in for the Anthropic API, for the tests. It listens on
// 127.0.0.1 only and answers POST /v1/messages and GET /api/oauth/usage from a
// routes file, keyed by the bearer token the request carried:
//
//   { "<bearer token>": { "name": "work", "status": 200, "headers": {...}, "delayMs": 0 } }
//
// A token with no route gets 401. The log file gets one line per request,
// "<METHOD> <PATH> <route name or ?> model=<model>", never the token itself.
//
//   node test/stub-api.mjs --routes FILE --log FILE --port-file FILE
//
// The chosen port is written to --port-file once the server is listening. The
// routes file is re-read on every request, so a test can change it between runs.
import { createServer } from "node:http";
import { appendFileSync, readFileSync, writeFileSync } from "node:fs";

const args = Object.fromEntries(
  process.argv.slice(2).reduce((acc, v, i, all) => (i % 2 === 0 ? [...acc, [v.replace(/^--/, ""), all[i + 1]]] : acc), []),
);
if (!args.routes || !args.log || !args["port-file"]) {
  console.error("usage: stub-api.mjs --routes FILE --log FILE --port-file FILE");
  process.exit(2);
}

const server = createServer((req, res) => {
  let body = "";
  req.on("data", (c) => { body += c; });
  req.on("end", () => {
    const auth = String(req.headers.authorization ?? "");
    const token = auth.startsWith("Bearer ") ? auth.slice(7) : "";
    let routes = {};
    try { routes = JSON.parse(readFileSync(args.routes, "utf8")); } catch { /* no routes: everything is 401 */ }
    const route = routes[token];
    let model = "-";
    try { model = JSON.parse(body).model ?? "-"; } catch { /* a GET has no body */ }
    appendFileSync(args.log, `${req.method} ${req.url} ${route?.name ?? "?"} model=${model} beta=${req.headers["anthropic-beta"] ?? "-"}\n`);
    const reply = () => {
      if (!route) { res.writeHead(401, { "content-type": "application/json" }); res.end("{}"); return; }
      const status = req.url.startsWith("/api/oauth/usage") ? (route.usageStatus ?? 403) : route.status ?? 200;
      res.writeHead(status, { "content-type": "application/json", ...(route.headers ?? {}) });
      res.end("{}");
    };
    if (route?.delayMs) setTimeout(reply, route.delayMs); else reply();
  });
});

server.listen(0, "127.0.0.1", () => {
  writeFileSync(args["port-file"], String(server.address().port));
});
