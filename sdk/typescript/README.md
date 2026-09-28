# oculos-sdk

TypeScript SDK for [OculOS](https://github.com/huseyinstif/oculos) — control any desktop app through JSON.

## Install

```bash
# From the repo root:
cd sdk/typescript
npm install
npm run build
```

> npm package (`npm install oculos-sdk`) coming soon.

## Quick Start

```typescript
import { OculOS, OculOSError } from "oculos-sdk";

const client = new OculOS(); // http://127.0.0.1:7878, token from process.env.OCULOS_TOKEN

// Find the window
const calc = (await client.listWindows()).find((w) => w.exe_name.toLowerCase().includes("calc"))!;

// Find a button and click it
const buttons = await client.findElements(calc.pid, { query: "Submit", type: "Button" });
await client.click(buttons[0].oculos_id);

// Type into a text field
await client.setText(elementId, "hello world");

// Keyboard input: chords, repeats, literal braces
await client.sendKeys(elementId, "{CTRL+A}new text{ENTER}");
await client.sendKeys(elementId, "{TAB 3}{CTRL+SHIFT+T}");

// Wait for a dialog to appear, then for a spinner to go away
const [ok] = await client.waitFor({ pid: calc.pid, query: "OK", type: "Button", timeoutMs: 5000 });
await client.waitFor({ pid: calc.pid, query: "Loading", until: "gone", timeoutMs: 10000 });

// Screenshots (PNG bytes)
const png: Uint8Array = await client.screenshotElement(ok.oculos_id);

// Several actions in one request (validated up front, stops at the first failure)
const results = await client.batch(
  [
    { element_id: nameId, action: "set-text", text: "Ada" },
    { element_id: ok.oculos_id, action: "click" },
  ],
  { stopOnError: true, delayMs: 100 },
);
```

Element ids (`oculos_id`) are stable: the same element found twice gets the same id. They expire after 30 minutes without use, so call `findElements` again after a `not_found` error.

## Options & authentication

```typescript
const client = new OculOS({
  baseUrl: "http://192.168.1.20:7878", // default http://127.0.0.1:7878
  token: "…",                          // default process.env.OCULOS_TOKEN (Node)
  timeoutMs: 30000,                    // per request; waits/batches get more automatically
});
```

A token is only needed when the server runs with `--token` or is bound to a non-loopback address (it then generates one and prints it in its log). It is sent as the `X-OculOS-Token` header. `new OculOS("http://host:7878")` still works.

## Errors

Every failure rejects with `OculOSError` (`message`, `code`, `status`):

```typescript
try {
  await client.click(id);
} catch (e) {
  if (e instanceof OculOSError && e.code === "not_found") {
    // element gone or id expired → find it again
  }
}
```

`code` is one of `not_found` (404), `invalid_input` (400), `unsupported` (400), `timeout` (408), `permission_denied` (403), `forbidden` (403), `unauthorized` (401), `internal` (500). Client-side failures (server unreachable, request timeout, non-JSON response) have no `code`.

## All Methods

| Method | Description |
|--------|-------------|
| `new OculOS(opts?)` | `{ baseUrl, token, timeoutMs }` or a URL string |
| `listWindows()` | List all visible windows |
| `getTree(pid)` / `getTreeHwnd(hwnd)` | Full UI element tree |
| `findElements(pid, { query, type, interactive })` | Search elements |
| `findElementsHwnd(hwnd, opts?)` | Search by window handle |
| `waitFor({ pid \| hwnd, query, type, interactive, timeoutMs, until })` | Wait for elements to appear / be `"gone"` |
| `focusWindow(pid)` | Bring window to foreground |
| `closeWindow(pid)` | Close window |
| `screenshot(pid)` | Window PNG as `Uint8Array` |
| `screenshotElement(id)` | Element PNG as `Uint8Array` |
| `click(id)` | Click element |
| `setText(id, text)` | Replace text content |
| `sendKeys(id, keys)` | Keyboard input (`{ENTER}`, `{CTRL+SHIFT+T}`, `{TAB 3}`, `{{`…) |
| `focus(id)` | Move focus |
| `toggle(id)` | Toggle checkbox |
| `expand(id)` / `collapse(id)` | Expand / collapse |
| `select(id)` | Select list item, radio button, tab |
| `setRange(id, value)` | Set slider value |
| `scroll(id, direction)` | `up`, `down`, `left`, `right`, `page-up`, `page-down` |
| `scrollIntoView(id)` | Scroll into viewport |
| `highlight(id, durationMs?)` | Highlight on screen |
| `batch(actions, { stopOnError, delayMs })` | Up to 100 actions in one request |
| `health()` | Server status, version, `auth_required` |

Types (`UiElement`, `ElementType`, `ELEMENT_TYPES`, `BatchAction`, `BatchResult`, `ErrorCode`, …) are exported from the package root.

## Tests

```bash
npm test   # builds, then runs offline tests (and live tests if a server is running)
```

## Requirements

- Node.js 18+ (or any runtime with `fetch` and `AbortController`)
- OculOS server running (`oculos` binary)

## License

MIT
