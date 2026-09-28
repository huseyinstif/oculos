# Security Policy

## Supported Versions

| Version | Supported |
|---------|-----------|
| 0.2.x   | ✅        |
| < 0.2   | ❌ (the dashboard XSS and permissive CORS fixed in 0.2.0 affect these) |

## Reporting a Vulnerability

If you discover a security vulnerability in OculOS, please report it responsibly:

1. **Do NOT open a public issue.**
2. Email **mail@huseyintintas.com** with:
   - Description of the vulnerability
   - Steps to reproduce
   - Potential impact
3. You will receive a response within 48 hours.

## Security Model

OculOS gives full control over the desktop session it runs in, so access to its API must be protected:

- **Loopback by default** — it binds to `127.0.0.1:7878` and is not reachable from the network.
- **Host check** — requests whose `Host` header is not an IP address, `localhost` or a name allowed with `--allow-host` are rejected (DNS-rebinding protection).
- **Origin check / no CORS** — browser requests with an `Origin` other than the server itself (or one allowed with `--allow-origin`) are rejected, including `Origin: null`. No CORS headers are sent unless `--allow-origin` is used, so websites you visit cannot call the API.
- **Optional token** — `--token <T>` or `OCULOS_TOKEN` requires `X-OculOS-Token: <T>` or `Authorization: Bearer <T>` (`?token=` for WebSocket) on every route except `GET /health` and the dashboard page. When bound to a non-loopback address without a token, OculOS generates one and prints it at startup — the API is never exposed to a network unauthenticated.
- **Dashboard** — served with `X-Frame-Options: DENY`, `nosniff` and `no-referrer`; it treats all window titles and element text as untrusted and escapes them. The token is only embedded in the page for clients on the same machine; remote browsers must supply it (`/?token=<token>`).

**OculOS does not include:**
- Encryption (HTTP, not HTTPS)
- Rate limiting
- Per-client permissions (a valid token grants full control)
- Sandboxing of interactions

**If you need remote access**, prefer an SSH tunnel or VPN over binding to `0.0.0.0`:

```bash
ssh -L 7878:127.0.0.1:7878 user@remote-machine
```

If you do bind to a network address, set a strong token (`OCULOS_TOKEN=$(openssl rand -hex 16)`) and restrict access with a firewall.

## Scope

The following are considered security issues:
- Remote code execution without user interaction
- Privilege escalation through the API
- Unintended network exposure, or bypassing the Host/Origin/token checks
- Script injection into the dashboard (e.g. through window titles or element labels)
- Data exfiltration through the accessibility tree

The following are **not** security issues:
- An authenticated local user controlling desktop apps (this is the intended behavior)
- Accessibility tree exposing UI content (this is how OS accessibility works)
- Anyone who holds the API token controlling the desktop (that is what the token is for)
