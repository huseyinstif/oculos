"""OculOS Python SDK tests.

Offline tests always run (they use a tiny in-process fake server). The live
tests run only when an OculOS server is reachable at OCULOS_URL
(default http://127.0.0.1:7878); set OCULOS_TOKEN if it requires a token.

    python test_sdk.py          # or: pytest test_sdk.py
"""

import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import parse_qs, urlparse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from oculos import OculOS, OculOSError, __version__  # noqa: E402

PNG = b"\x89PNG\r\n\x1a\nfake"


# ── Fake server ───────────────────────────────────────────────────────────────


class _Fake(BaseHTTPRequestHandler):
    requests = []  # (method, path, query, headers, body)

    def log_message(self, *args):  # keep test output clean
        pass

    def _reply(self, status, obj=None, raw=None, ctype="application/json"):
        data = raw if raw is not None else json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _handle(self, method):
        u = urlparse(self.path)
        length = int(self.headers.get("Content-Length") or 0)
        body = json.loads(self.rfile.read(length)) if length else None
        q = {k: v[0] for k, v in parse_qs(u.query).items()}
        _Fake.requests.append((method, u.path, q, dict(self.headers), body))
        ok = lambda data: self._reply(200, {"success": True, "data": data, "error": None})  # noqa: E731
        err = lambda s, c, m: self._reply(s, {"success": False, "data": None, "error": m, "code": c})  # noqa: E731

        if u.path == "/health":
            return ok({"status": "running", "version": "0.2.0", "auth_required": True})
        if self.headers.get("X-OculOS-Token") != "t0k":
            return err(401, "unauthorized", "Missing or invalid API token.")
        if u.path == "/windows":
            return ok([{"pid": 1, "hwnd": 2, "title": "T", "exe_name": "a.exe"}])
        if u.path.endswith("/wait"):
            if q.get("q") == "never":
                return err(408, "timeout", "No matching element appeared within 100ms")
            return ok([] if q.get("until") == "gone" else [{"oculos_id": "0123456789abcdef"}])
        if u.path.endswith("/screenshot"):
            return self._reply(200, raw=PNG, ctype="image/png")
        if u.path == "/interact/batch":
            return ok([{"index": i, "action": a["action"], "element_id": a["element_id"],
                        "success": True, "error": None} for i, a in enumerate(body["actions"])])
        if u.path == "/interact/deadbeefdeadbeef/click":
            return err(404, "not_found", "Element 'deadbeefdeadbeef' not found")
        if u.path == "/interact/0123456789abcdef/send-keys":
            return err(400, "invalid_input", "Unknown key 'NOPE' in '{NOPE}'")
        if u.path == "/html":
            return self._reply(502, raw=b"<html>bad gateway</html>", ctype="text/html")
        return err(404, "not_found", "No such endpoint")

    def do_GET(self):
        self._handle("GET")

    def do_POST(self):
        self._handle("POST")


def _start_fake():
    srv = HTTPServer(("127.0.0.1", 0), _Fake)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv, f"http://127.0.0.1:{srv.server_address[1]}"


# ── Offline tests ─────────────────────────────────────────────────────────────


def test_offline():
    old = os.environ.pop("OCULOS_TOKEN", None)
    try:
        c = OculOS()
        assert c.base_url == "http://127.0.0.1:7878"
        assert c.token is None and "X-OculOS-Token" not in c._session.headers
        assert OculOS("http://h:1/").base_url == "http://h:1"

        os.environ["OCULOS_TOKEN"] = "from-env"
        assert OculOS()._session.headers["X-OculOS-Token"] == "from-env"
        assert OculOS(token="explicit").token == "explicit"
        del os.environ["OCULOS_TOKEN"]

        e = OculOSError("gone", code="not_found", status=404)
        assert (e.message, e.code, e.status) == ("gone", "not_found", 404)
        assert str(e) == "gone [not_found]"

        try:
            c.wait_for(q="x")
            raise AssertionError("wait_for without pid/hwnd must raise")
        except ValueError:
            pass
        try:
            c.click("../windows/1/close")
            raise AssertionError("path-like ids must be rejected")
        except OculOSError as ex:
            assert ex.code == "invalid_input"

        srv, url = _start_fake()
        try:
            # token required -> unauthorized
            try:
                OculOS(url, token="").list_windows()
                raise AssertionError("expected unauthorized")
            except OculOSError as ex:
                assert (ex.code, ex.status) == ("unauthorized", 401), ex

            client = OculOS(url, token="t0k", timeout=5)
            assert client.health()["auth_required"] is True
            assert client.list_windows()[0]["exe_name"] == "a.exe"

            # wait_for: params, hwnd variant, until=gone, timeout error
            _Fake.requests.clear()
            found = client.wait_for(pid=1, q="OK", type="button", interactive=True, timeout_ms=1234)
            assert found[0]["oculos_id"] == "0123456789abcdef"
            _, path, q, _, _ = _Fake.requests[-1]
            assert path == "/windows/1/wait"
            assert q == {"q": "OK", "type": "button", "interactive": "true",
                         "timeout": "1234", "until": "appears"}, q
            assert client.wait_for(hwnd=2, q="Saving", until="gone") == []
            assert _Fake.requests[-1][1] == "/hwnd/2/wait"
            try:
                client.wait_for(pid=1, q="never", timeout_ms=100)
                raise AssertionError("expected timeout")
            except OculOSError as ex:
                assert (ex.code, ex.status) == ("timeout", 408)

            # screenshots come back as bytes
            assert client.screenshot(1) == PNG
            assert client.screenshot_element("0123456789abcdef") == PNG

            # batch: body shape + defaults
            res = client.batch([{"element_id": "0123456789abcdef", "action": "click"}], delay_ms=50)
            assert res[0]["success"] is True
            body = _Fake.requests[-1][4]
            assert body == {"actions": [{"element_id": "0123456789abcdef", "action": "click"}],
                            "stop_on_error": True, "delay_ms": 50}, body

            # envelope errors carry code + status
            try:
                client.click("deadbeefdeadbeef")
                raise AssertionError("expected not_found")
            except OculOSError as ex:
                assert (ex.code, ex.status) == ("not_found", 404)
            try:
                client.send_keys("0123456789abcdef", "{NOPE}")
                raise AssertionError("expected invalid_input")
            except OculOSError as ex:
                assert ex.code == "invalid_input"

            # non-JSON response -> OculOSError
            try:
                client._get("/html")
                raise AssertionError("expected non-JSON error")
            except OculOSError as ex:
                assert ex.status == 502 and ex.code is None
        finally:
            srv.shutdown()
    finally:
        os.environ.pop("OCULOS_TOKEN", None)
        if old is not None:
            os.environ["OCULOS_TOKEN"] = old


# ── Live tests (real server) ─────────────────────────────────────────────────


def run_live(client):
    passed = failed = 0

    def check(name, fn):
        nonlocal passed, failed
        try:
            msg = fn()
            print(f"  ✓ {name}" + (f" — {msg}" if msg else ""))
            passed += 1
        except Exception as e:  # noqa: BLE001
            print(f"  ✗ {name} — {type(e).__name__}: {e}")
            failed += 1

    h = client.health()
    windows = client.list_windows()
    if not windows:
        print("  ⊘ no windows open — live element tests skipped")
        return 0
    pid, hwnd = windows[0]["pid"], windows[0]["hwnd"]

    check("health()", lambda: f"version={h['version']}, auth_required={h.get('auth_required')}")
    check("list_windows()", lambda: f"{len(windows)} windows")
    check("get_tree(pid)", lambda: f"root={client.get_tree(pid)['type']}")
    def hwnd_call(fn):
        # HWND endpoints exist on Windows and macOS; Linux answers "unsupported".
        try:
            return fn()
        except OculOSError as e:
            if e.code == "unsupported":
                return "unsupported on this platform"
            raise

    check("get_tree_hwnd(hwnd)", lambda: hwnd_call(lambda: f"root={client.get_tree_hwnd(hwnd)['type']}"))
    check("find_elements(interactive)", lambda: f"{len(client.find_elements(pid, interactive=True))} elements")
    check("find_elements_hwnd()", lambda: hwnd_call(
        lambda: f"{len(client.find_elements_hwnd(hwnd, interactive=True))} elements"))

    def stable_ids():
        a = [e["oculos_id"] for e in client.find_elements(pid, interactive=True)]
        b = [e["oculos_id"] for e in client.find_elements(pid, interactive=True)]
        assert a == b, "ids changed between two identical finds"
        return f"{len(a)} ids stable"

    check("stable ids", stable_ids)
    check("wait_for(pid)", lambda: f"{len(client.wait_for(pid=pid, timeout_ms=2000))} elements")

    def wait_gone():
        assert client.wait_for(pid=pid, q="no-such-element-xyz", until="gone", timeout_ms=1000) == []

    check("wait_for(until=gone)", wait_gone)

    def wait_timeout():
        try:
            client.wait_for(pid=pid, q="no-such-element-xyz", timeout_ms=500)
        except OculOSError as e:
            assert e.code == "timeout", e
            return "timeout raised"
        raise AssertionError("expected timeout")

    check("wait_for timeout", wait_timeout)

    def type_filter():
        try:
            client.find_elements(pid, element_type="Buton")
        except OculOSError as e:
            assert e.code == "invalid_input", e
            return "unknown type rejected"
        raise AssertionError("expected invalid_input")

    check("element_type validation", type_filter)

    def stale_id():
        try:
            client.click("ffffffffffffffff")
        except OculOSError as e:
            assert e.code == "not_found", e
            return "not_found"
        raise AssertionError("expected not_found")

    check("unknown id", stale_id)

    def bad_batch():
        try:
            client.batch([{"element_id": "ffffffffffffffff", "action": "send-keys", "keys": "{NOPE}"}])
        except OculOSError as e:
            assert e.code == "invalid_input", e
            return "rejected before running"
        raise AssertionError("expected invalid_input")

    check("batch validation", bad_batch)

    def screenshot():
        try:
            png = client.screenshot(pid)
        except OculOSError as e:
            if e.code == "unsupported":
                return "unsupported on this platform"
            raise
        assert png[:8] == b"\x89PNG\r\n\x1a\n"
        return f"{len(png)} bytes"

    check("screenshot(pid)", screenshot)

    print(f"\n  live: {passed}/{passed + failed} passed")
    return failed


def main():
    print(f"OculOS Python SDK {__version__} tests\n")
    test_offline()
    print("  ✓ offline tests passed")

    client = OculOS(os.environ.get("OCULOS_URL", "http://127.0.0.1:7878"), timeout=10)
    try:
        client.health()
    except Exception as e:  # noqa: BLE001
        print(f"  ⊘ live tests skipped — no server at {client.base_url} ({type(e).__name__})")
        return 0
    return 1 if run_live(client) else 0


if __name__ == "__main__":
    sys.exit(main())
