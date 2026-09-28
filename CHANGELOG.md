# Changelog

All notable changes to OculOS will be documented in this file.

Format based on [Keep a Changelog](https://keepachangelog.com/).

## [0.2.0] — 2026-09-28

### Security
- **No more `Access-Control-Allow-Origin: *`** — any website could previously drive the desktop through a visitor's local OculOS. CORS headers are now sent only for origins allowed with `--allow-origin`.
- **Host and Origin checks** — the `Host` header must be an IP literal, `localhost` or allowed via `--allow-host` (blocks DNS rebinding); browser requests with a foreign or `null` `Origin` are rejected with 403 `forbidden`.
- **Optional API token** — `--token` / `OCULOS_TOKEN`, sent as `X-OculOS-Token`, `Authorization: Bearer` or `?token=` (WebSocket). Binding to a non-loopback address without a token generates one and prints it at startup. `GET /health` and the dashboard page stay public; the dashboard receives the token automatically on the local machine, remote browsers open `/?token=<token>`.
- **Dashboard XSS fixed** — window titles, element labels, values and other app-provided text were inserted into the page unescaped (a web page `<title>` could run script in the dashboard). All such data is now escaped or set as text, inline handlers no longer embed data, and element ids are validated.

### Fixed
- **MCP**: logs no longer go to stdout (they corrupted the JSON-RPC stream); no responses are sent to notifications; `ping` is supported; tool failures are returned as `isError: true` results instead of protocol errors; the protocol version is negotiated with the client.
- **Linux**: the backend now connects to the AT-SPI accessibility bus and detects the interfaces elements really implement, instead of guessing.
- **Memory**: element ids are stable and the element registry is bounded with idle expiry, fixing unbounded memory growth when agents poll `find`/`tree`.
- **Windows**: process-handle leak fixed.
- **No false "success"** — click and focus-window report an error when the platform could not perform them.
- **send-keys**: `{SPACE}`, `{WIN+D}` and multi-modifier chords work on every platform; invalid syntax is rejected before anything is typed.
- **Element type filter** is validated case-insensitively; unknown types return 400 with the list of valid types.
- **Windows screenshots** use `PrintWindow`, set the alpha channel correctly and are DPI-aware.
- **macOS**: element rects are reported, scrolling happens at the element, and window operations are fixed.

### Changed
- **Stable element ids** — 16 hex characters derived from the element, so the same element keeps the same `oculos_id`; ids expire after 30 minutes idle.
- **Typed errors** — every error response carries a `code` (`not_found`, `invalid_input`, `unsupported`, `timeout`, `permission_denied`, `forbidden`, `unauthorized`, `internal`) with a matching HTTP status.
- **send-keys rewritten** around one parser shared by all platforms: case-insensitive names, multi-modifier chords (`{CTRL+SHIFT+T}`), `{MOD+…}` (Cmd on macOS, Ctrl elsewhere), repeats (`{TAB 3}`), literal braces (`{{`, `}}`), `\n` → Enter. On Linux, keystrokes are sent in batched `xdotool` calls.
- **Batch** validates every step before running any, and accepts `stop_on_error` (default `true`) and `delay_ms`; results include `code`.
- **Dashboard embedded in the binary** — release binaries and the Docker image no longer need a `static/` directory (`--static-dir` remains as a development override).
- **Recorder** exports selector-based scripts (window by `exe_name`, element by `automation_id` or label + type) that survive restarts, with `OCULOS_TOKEN` support and correct quoting.
- **MCP tool output** is compact JSON (null/empty fields dropped); `find_elements` takes a `limit` (default 100).

### Added
- `GET /interact/{id}/screenshot` (element screenshot), `GET /hwnd/{hwnd}/wait`, and `until=gone` for waits.
- WebSocket events for every action (plus `tree_loaded` and `windows`), with lag handling for slow clients.
- New element types: `SplitButton`, `MenuBar`, `Spinner`, `Header`, `TitleBar`, `ToolTip`, `Separator`, `Calendar`, `Thumb`.
- New MCP tools: `wait_for_element`, `screenshot_window` / `screenshot_element` (returned as images), `batch_actions`, `highlight_element`.
- `auth_required` in `GET /health`; dashboard shows the auth state and error codes.
- **SDKs 0.2.0** (Python & TypeScript): token support, `wait_for` / `waitFor`, window and element screenshots, `batch`, request timeouts, `OculOSError` with `code` and `status`.
- Unit tests, and CI that enforces `cargo clippy -D warnings` and runs the tests on Windows, Linux and macOS.

### Performance
- **Windows**: UI Automation `CacheRequest` fetches a whole tree in one cross-process call, and `find` conditions are evaluated on the server side.
- **Linux**: concurrent D-Bus queries, and filtering before building elements.
- **macOS**: batched accessibility attribute reads.
- **MCP output size**: measured on `gtk3-widget-factory`, `find_elements` (interactive only) is 3.1× smaller and a full `get_ui_tree` 7.7× smaller than the 0.1.0 output (compact JSON, dropped defaults, 16-char ids).
- Unused dependencies removed (`reqwest`, `thiserror`, `tokio-stream`, axum `multipart`); `image` built with PNG support only.

## [0.1.0] — 2026-03-08

### Added
- **REST API** — full UI automation over HTTP (discovery, interactions, window ops)
- **MCP server** — `--mcp` flag for AI agent integration (Claude, Cursor, Windsurf…)
- **Web dashboard** — element tree inspector, recorder, live WebSocket events
- **Cross-platform** — Windows (UI Automation), Linux (AT-SPI2), macOS (Accessibility API)
- **Element interactions** — click, set-text, send-keys, toggle, expand, collapse, select, set-range, scroll, highlight
- **Wait/poll endpoint** — `GET /windows/{pid}/wait` with configurable timeout
- **Screenshot capture** — `GET /windows/{pid}/screenshot` returns PNG (Windows)
- **Batch operations** — `POST /interact/batch` for multiple actions in one request
- **Smart error codes** — 404 for not found, 400 for invalid, 500 for server errors
- **Python SDK** — `sdk/python/` with full API wrapper
- **TypeScript SDK** — `sdk/typescript/` with typed async client
- **GitHub Actions CI** — build + lint on Windows, Linux, macOS
- **Release workflow** — auto-build binaries on tag push
- **Examples** — 7 ready-to-run scripts (Python + curl)
- **OpenAPI spec** — `openapi.yaml` for API documentation
- **Docker support** — `Dockerfile` for Linux builds
