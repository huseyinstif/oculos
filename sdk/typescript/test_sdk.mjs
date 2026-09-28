/**
 * OculOS TypeScript SDK tests (Node 18+). Build first: `npm run build` (or `npm test`).
 *
 * Offline tests always run against a tiny in-process fake server. Live tests
 * run only when an OculOS server is reachable at OCULOS_URL (default
 * http://127.0.0.1:7878); set OCULOS_TOKEN if it requires a token.
 */

import http from "node:http";
import assert from "node:assert/strict";
import { OculOS, OculOSError, ELEMENT_TYPES } from "./dist/index.js";

const PNG = Buffer.from("\x89PNG\r\n\x1a\nfake", "latin1");

// ── Fake server ──────────────────────────────────────────────────────────────

const seen = [];
function fakeServer() {
  const server = http.createServer((req, res) => {
    let raw = "";
    req.on("data", (c) => (raw += c));
    req.on("end", () => {
      const u = new URL(req.url, "http://x");
      const q = Object.fromEntries(u.searchParams);
      const body = raw ? JSON.parse(raw) : undefined;
      seen.push({ method: req.method, path: u.pathname, q, headers: req.headers, body });
      const send = (status, obj, type = "application/json") => {
        res.writeHead(status, { "content-type": type });
        res.end(type === "application/json" ? JSON.stringify(obj) : obj);
      };
      const ok = (data) => send(200, { success: true, data, error: null });
      const err = (s, code, error) => send(s, { success: false, data: null, error, code });

      if (u.pathname === "/health") return ok({ status: "running", version: "0.2.0", auth_required: true });
      if (u.pathname === "/slow") return setTimeout(() => ok(null), 500);
      if (req.headers["x-oculos-token"] !== "t0k") return err(401, "unauthorized", "Missing or invalid API token.");
      if (u.pathname === "/windows") return ok([{ pid: 1, hwnd: 2, title: "T", exe_name: "a.exe" }]);
      if (u.pathname.endsWith("/wait")) {
        if (q.q === "never") return err(408, "timeout", "No matching element appeared within 100ms");
        return ok(q.until === "gone" ? [] : [{ oculos_id: "0123456789abcdef" }]);
      }
      if (u.pathname.endsWith("/screenshot")) return send(200, PNG, "image/png");
      if (u.pathname === "/interact/batch") {
        return ok(body.actions.map((a, index) => ({ index, action: a.action, element_id: a.element_id, success: true, error: null })));
      }
      if (u.pathname === "/interact/deadbeefdeadbeef/click") return err(404, "not_found", "Element not found");
      if (u.pathname === "/html") return send(502, "<html>bad gateway</html>", "text/html");
      return err(404, "not_found", "No such endpoint");
    });
  });
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve(server)));
}

async function rejects(promise, check) {
  try {
    await promise;
  } catch (e) {
    assert.ok(e instanceof OculOSError, `expected OculOSError, got ${e}`);
    check(e);
    return;
  }
  assert.fail("expected rejection");
}

async function offline() {
  const saved = process.env.OCULOS_TOKEN;
  delete process.env.OCULOS_TOKEN;
  try {
    assert.equal(new OculOS().baseUrl, "http://127.0.0.1:7878");
    assert.equal(new OculOS("http://h:1/").baseUrl, "http://h:1");
    assert.ok(ELEMENT_TYPES.includes("SplitButton") && ELEMENT_TYPES.length === 41);

    const server = await fakeServer();
    const url = `http://127.0.0.1:${server.address().port}`;
    try {
      await rejects(new OculOS(url).listWindows(), (e) => {
        assert.equal(e.code, "unauthorized");
        assert.equal(e.status, 401);
      });

      process.env.OCULOS_TOKEN = "t0k"; // token from the environment
      const client = new OculOS({ baseUrl: url, timeoutMs: 5000 });
      delete process.env.OCULOS_TOKEN;
      assert.equal((await client.health()).auth_required, true);
      assert.equal((await client.listWindows())[0].exe_name, "a.exe");
      assert.equal(seen.at(-1).headers["x-oculos-token"], "t0k");

      // waitFor: params, hwnd, until=gone, timeout
      const found = await client.waitFor({ pid: 1, query: "OK", type: "button", interactive: true, timeoutMs: 1234 });
      assert.equal(found[0].oculos_id, "0123456789abcdef");
      assert.equal(seen.at(-1).path, "/windows/1/wait");
      assert.deepEqual(seen.at(-1).q, { q: "OK", type: "button", interactive: "true", timeout: "1234", until: "appears" });
      assert.deepEqual(await client.waitFor({ hwnd: 2, query: "Saving", until: "gone" }), []);
      assert.equal(seen.at(-1).path, "/hwnd/2/wait");
      await rejects(client.waitFor({ pid: 1, query: "never", timeoutMs: 100 }), (e) => {
        assert.equal(e.code, "timeout");
        assert.equal(e.status, 408);
      });
      await rejects(client.waitFor({ query: "x" }), (e) => assert.equal(e.code, "invalid_input"));

      // screenshots
      const shot = await client.screenshot(1);
      assert.ok(shot instanceof Uint8Array);
      assert.deepEqual(Buffer.from(shot), PNG);
      assert.deepEqual(Buffer.from(await client.screenshotElement("0123456789abcdef")), PNG);

      // batch
      const res = await client.batch([{ element_id: "0123456789abcdef", action: "click" }], { delayMs: 50 });
      assert.equal(res[0].success, true);
      assert.deepEqual(seen.at(-1).body, {
        actions: [{ element_id: "0123456789abcdef", action: "click" }],
        stop_on_error: true,
        delay_ms: 50,
      });
      await client.batch([{ element_id: "0123456789abcdef", action: "focus" }], { stopOnError: false });
      assert.equal(seen.at(-1).body.stop_on_error, false);

      // errors
      await rejects(client.click("deadbeefdeadbeef"), (e) => assert.equal(e.code, "not_found"));
      await rejects(client.click("../windows/1/close"), (e) => assert.equal(e.code, "invalid_input"));
      await rejects(client.request("GET", "/html"), (e) => {
        assert.equal(e.status, 502);
        assert.equal(e.code, undefined);
      });
      await rejects(new OculOS({ baseUrl: url, timeoutMs: 100 }).request("GET", "/slow"), (e) =>
        assert.match(e.message, /timed out/),
      );
      await rejects(new OculOS("http://127.0.0.1:9").health(), (e) => assert.match(e.message, /Cannot reach/));
    } finally {
      server.close();
    }
  } finally {
    if (saved !== undefined) process.env.OCULOS_TOKEN = saved;
  }
}

// ── Live tests (real server) ─────────────────────────────────────────────────

async function live(client) {
  let passed = 0;
  let failed = 0;
  const check = async (name, fn) => {
    try {
      const msg = await fn();
      console.log(`  ✓ ${name}${msg ? ` — ${msg}` : ""}`);
      passed++;
    } catch (e) {
      console.log(`  ✗ ${name} — ${e.message}`);
      failed++;
    }
  };

  const wins = await client.listWindows();
  if (!wins.length) {
    console.log("  ⊘ no windows open — live element tests skipped");
    return 0;
  }
  const { pid, hwnd } = wins[0];
  await check("health()", async () => `auth_required=${(await client.health()).auth_required}`);
  await check("getTree()", async () => `root=${(await client.getTree(pid)).type}`);
  // HWND endpoints exist on Windows and macOS; Linux answers "unsupported".
  const hwndCall = async (fn) => {
    try {
      return await fn();
    } catch (e) {
      if (e.code === "unsupported") return "unsupported on this platform";
      throw e;
    }
  };
  await check("getTreeHwnd()", () => hwndCall(async () => `root=${(await client.getTreeHwnd(hwnd)).type}`));
  await check("findElements(interactive)", async () => `${(await client.findElements(pid, { interactive: true })).length} elements`);
  await check("findElementsHwnd()", () =>
    hwndCall(async () => `${(await client.findElementsHwnd(hwnd, { interactive: true })).length} elements`),
  );
  await check("stable ids", async () => {
    const a = (await client.findElements(pid, { interactive: true })).map((e) => e.oculos_id);
    const b = (await client.findElements(pid, { interactive: true })).map((e) => e.oculos_id);
    assert.deepEqual(a, b);
    return `${a.length} ids stable`;
  });
  await check("waitFor(until=gone)", async () => {
    assert.deepEqual(await client.waitFor({ pid, query: "no-such-element-xyz", until: "gone", timeoutMs: 1000 }), []);
  });
  await check("waitFor timeout", () =>
    rejects(client.waitFor({ pid, query: "no-such-element-xyz", timeoutMs: 500 }), (e) => assert.equal(e.code, "timeout")),
  );
  await check("unknown type", () =>
    rejects(client.findElements(pid, { type: "Buton" }), (e) => assert.equal(e.code, "invalid_input")),
  );
  await check("unknown id", () => rejects(client.click("ffffffffffffffff"), (e) => assert.equal(e.code, "not_found")));
  await check("batch validation", () =>
    rejects(client.batch([{ element_id: "ffffffffffffffff", action: "send-keys", keys: "{NOPE}" }]), (e) =>
      assert.equal(e.code, "invalid_input"),
    ),
  );
  console.log(`\n  live: ${passed}/${passed + failed} passed`);
  return failed;
}

console.log("OculOS TypeScript SDK tests\n");
try {
  await offline();
  console.log("  ✓ offline tests passed");
} catch (e) {
  console.log(`  ✗ offline tests failed — ${e.stack}`);
  process.exit(1);
}

const client = new OculOS({ baseUrl: process.env.OCULOS_URL ?? "http://127.0.0.1:7878", timeoutMs: 10_000 });
let reachable = true;
try {
  await client.health();
} catch (e) {
  reachable = false;
  console.log(`  ⊘ live tests skipped — no server at ${client.baseUrl}`);
}
process.exit(reachable && (await live(client)) ? 1 : 0);
