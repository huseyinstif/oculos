# oculos-sdk

Python SDK for [OculOS](https://github.com/huseyinstif/oculos) — control any desktop app through JSON.

## Install

```bash
# From the repo root:
cd sdk/python
pip install .
```

> PyPI package (`pip install oculos-sdk`) coming soon.

## Quick Start

```python
from oculos import OculOS, OculOSError

client = OculOS()  # default: http://127.0.0.1:7878, token from $OCULOS_TOKEN

# Find the window
calc = next(w for w in client.list_windows() if "calc" in w["exe_name"].lower())
pid = calc["pid"]

# Find a button and click it
buttons = client.find_elements(pid, query="Submit", element_type="Button")
client.click(buttons[0]["oculos_id"])

# Type into a text field
client.set_text(element_id, "hello world")

# Keyboard input: chords, repeats, literal braces
client.send_keys(element_id, "{CTRL+A}new text{ENTER}")
client.send_keys(element_id, "{TAB 3}{CTRL+SHIFT+T}")
client.send_keys(element_id, 'literal {{"json": true}}')

# Wait for a dialog to appear, then for a spinner to go away
ok = client.wait_for(pid=pid, q="OK", type="Button", timeout_ms=5000)
client.wait_for(pid=pid, q="Loading", until="gone", timeout_ms=10000)

# Screenshots (PNG bytes)
open("window.png", "wb").write(client.screenshot(pid))
open("button.png", "wb").write(client.screenshot_element(ok[0]["oculos_id"]))

# Several actions in one request (validated up front, stops at the first failure)
results = client.batch(
    [
        {"element_id": name_id, "action": "set-text", "text": "Ada"},
        {"element_id": ok[0]["oculos_id"], "action": "click"},
    ],
    stop_on_error=True,
    delay_ms=100,
)
```

Element ids (`oculos_id`) are stable: the same element found twice gets the same id. They expire after 30 minutes without use, so call `find_elements` again if you get a `not_found` error.

## Authentication

The server needs no token when it listens on loopback (the default). If it was started with `--token`, or bound to a non-loopback address (a token is then generated and printed in its log), pass the token:

```python
client = OculOS("http://192.168.1.20:7878", token="…")  # or export OCULOS_TOKEN=…
```

It is sent as the `X-OculOS-Token` header on every request.

## Errors

Every API error raises `OculOSError` with `message`, `code` and `status`:

```python
try:
    client.click(element_id)
except OculOSError as e:
    if e.code == "not_found":      # element gone or id expired → find it again
        ...
    elif e.code == "unauthorized": # missing / wrong token
        ...
```

| `code` | HTTP | Meaning |
|--------|------|---------|
| `not_found` | 404 | Element id unknown/expired, or window gone |
| `invalid_input` | 400 | Bad parameter (unknown element type, bad key syntax, invalid batch step…) |
| `unsupported` | 400 | The element or platform can't do that |
| `timeout` | 408 | `wait_for` condition not met in time |
| `permission_denied` | 403 | OS refused access (e.g. macOS Accessibility permission) |
| `forbidden` | 403 | Host/Origin not allowed |
| `unauthorized` | 401 | Token missing or wrong |
| `internal` | 500 | Unexpected server error |

Non-JSON responses also raise `OculOSError` (with `code=None`). Connection problems raise the usual `requests` exceptions.

## All Methods

| Method | Description |
|--------|-------------|
| `OculOS(base_url=, token=, timeout=30.0)` | Client; `token` defaults to `$OCULOS_TOKEN` |
| `list_windows()` | List all visible windows |
| `get_tree(pid)` / `get_tree_hwnd(hwnd)` | Full UI element tree |
| `find_elements(pid, query=, element_type=, interactive=)` | Search elements |
| `find_elements_hwnd(hwnd, ...)` | Search by window handle |
| `wait_for(pid=\|hwnd=, q=, type=, interactive=, timeout_ms=5000, until="appears"\|"gone")` | Wait for elements to appear / disappear |
| `focus_window(pid)` | Bring window to foreground |
| `close_window(pid)` | Close window |
| `screenshot(pid)` | Window PNG as `bytes` |
| `screenshot_element(id)` | Element PNG as `bytes` |
| `click(id)` | Click element |
| `set_text(id, text)` | Replace text content |
| `send_keys(id, keys)` | Keyboard input (`{ENTER}`, `{CTRL+SHIFT+T}`, `{TAB 3}`, `{{`…) |
| `focus(id)` | Move focus |
| `toggle(id)` | Toggle checkbox |
| `expand(id)` / `collapse(id)` | Expand / collapse dropdown, tree item, menu |
| `select(id)` | Select list item, radio button, tab |
| `set_range(id, value)` | Set slider value |
| `scroll(id, direction)` | Scroll: up, down, left, right, page-up, page-down |
| `scroll_into_view(id)` | Scroll into viewport |
| `highlight(id, duration_ms=2000)` | Highlight on screen |
| `batch(actions, stop_on_error=True, delay_ms=0)` | Up to 100 actions in one request |
| `health()` | Server status, version, `auth_required` |

## Tests

```bash
python test_sdk.py   # offline tests always; live tests if a server is running
```

## Requirements

- Python 3.9+
- OculOS server running (`oculos` binary)

## License

MIT
