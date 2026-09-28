# OculOS — AI Agent Instructions

You have access to **OculOS**, a local REST API that lets you read and control any desktop application through its UI Automation tree.

**Base URL:** `http://127.0.0.1:7878`

**Auth:** if the server was started with a token (or bound to a network address), send it on every request:
`X-OculOS-Token: <token>` (or `Authorization: Bearer <token>`). `GET /health` returns `auth_required`
and never needs the token. A `401` with code `unauthorized` means the token is missing or wrong — ask the user for it.

---

## Core Workflow

```
1. list_windows          → find the target application (get pid / hwnd)
2. find_elements / tree  → locate the elements you need (get oculos_id)
3. interact              → act on those elements
4. wait / find again     → confirm the UI reached the state you expect
```

> **Element ids are stable.** An `oculos_id` is 16 hex characters. Finding the same element
> again returns the same id, so you can re-run `find` as often as you like. An id stops working
> when the element disappears or after 30 minutes without use — you then get a `not_found`
> error: run `find` again and use the fresh id.

---

## Discovery

### List all windows
```
GET /windows
```
Returns: `[{ pid, hwnd, title, exe_name, rect, visible }]`

### Search for elements (preferred — fast)
```
GET /windows/{pid}/find?q=Submit&type=Button&interactive=true
GET /hwnd/{hwnd}/find?q=Search&interactive=true
```
Parameters:
- `q` — case-insensitive substring match on label or automation_id
- `type` — element type filter, case-insensitive (see the list below; an unknown type → 400 `invalid_input` listing the valid ones)
- `interactive=true` — return only elements that have at least one action

Element types: Window, Button, SplitButton, Edit, Text, CheckBox, RadioButton, ComboBox, ListBox,
ListItem, TreeView, TreeItem, Menu, MenuBar, MenuItem, TabControl, TabItem, ToolBar, StatusBar,
ScrollBar, Slider, Spinner, ProgressBar, Image, Link, Group, Pane, Dialog, Document, DataGrid,
DataItem, Header, HeaderItem, Table, TitleBar, ToolTip, Separator, Calendar, Thumb, Custom, Unknown.

### Wait for something to appear — or to go away
```
GET /windows/{pid}/wait?q=OK&type=Button&timeout=5000
GET /hwnd/{hwnd}/wait?q=Loading&until=gone&timeout=10000
```
Same filters as `find`, plus:
- `timeout` — milliseconds (default 5000, max 30000)
- `until` — `appears` (default: return as soon as something matches) or `gone` (return once nothing matches, e.g. a progress dialog closed)

Returns the matching elements (empty for `until=gone`), or **408** with code `timeout`.
Prefer `wait` over sleeping after an action that opens a dialog or loads content.

### Full UI tree (for exploration)
```
GET /windows/{pid}/tree
GET /hwnd/{hwnd}/tree
```
Use the HWND variant when a process has multiple windows (e.g. Teams). HWND routes work on
Windows and macOS; on Linux `hwnd` is 0 and they return `unsupported` — use the PID routes.

### Screenshots (PNG)
```
GET /windows/{pid}/screenshot
GET /interact/{id}/screenshot
```
Available on Windows; other platforms answer `unsupported` (same for `highlight`).

---

## Interactions

All interaction endpoints accept a JSON body where noted.

| Endpoint | Body | When to use |
|----------|------|-------------|
| `POST /interact/{id}/click` | — | Buttons, links, menu items |
| `POST /interact/{id}/set-text` | `{"text":"…"}` | Input fields (replaces all text) |
| `POST /interact/{id}/send-keys` | `{"keys":"…"}` | Keyboard simulation (see below) |
| `POST /interact/{id}/focus` | — | Move keyboard focus |
| `POST /interact/{id}/toggle` | — | CheckBox, ToggleButton |
| `POST /interact/{id}/expand` | — | ComboBox, TreeItem, MenuItem |
| `POST /interact/{id}/collapse` | — | ComboBox, TreeItem, MenuItem |
| `POST /interact/{id}/select` | — | ListItem, RadioButton, TabItem |
| `POST /interact/{id}/set-range` | `{"value":75}` | Slider, Spinner |
| `POST /interact/{id}/scroll` | `{"direction":"down"}` | Scroll containers (`up`, `down`, `left`, `right`, `page-up`, `page-down`) |
| `POST /interact/{id}/scroll-into-view` | — | Bring element into viewport |
| `POST /interact/{id}/highlight` | `{"duration_ms":2000}` | Show the user which element you mean |

A successful response means the action really happened; if the platform could not do it you get an error instead.

### Batch — several actions in one request
```
POST /interact/batch
{
  "actions": [
    {"element_id": "a3f8c2d1e4b5f607", "action": "set-text", "text": "Ada"},
    {"element_id": "a3f8c2d1e4b5f607", "action": "send-keys", "keys": "{TAB}"},
    {"element_id": "0b1c2d3e4f506172", "action": "click"}
  ],
  "stop_on_error": true,
  "delay_ms": 100
}
```
- Up to 100 actions. `action` is any interaction name above except `highlight`; add `text`, `keys`, `value` or `direction` where the action needs it.
- Every step is validated first; if one is invalid the request fails with 400 and **nothing runs**.
- `stop_on_error` (default `true`) stops at the first failing step so later steps never run against an unexpected UI. Set it to `false` to attempt every step.
- `delay_ms` pauses between steps (max 5000).
- Response `data`: `[{ index, action, element_id, success, error, code? }]` — one entry per step that ran. Check `success` on each.

### Window operations
```
POST /windows/{pid}/focus    — bring to foreground
POST /windows/{pid}/close    — close gracefully
```

---

## UiElement fields

Every element returned by the API has:

| Field | Description |
|-------|-------------|
| `oculos_id` | **Use this in all /interact calls** (stable, 16 hex chars) |
| `type` | Element type: Button, Edit, Text, CheckBox, ComboBox… |
| `label` | Accessible name (what a screen reader would announce) |
| `value` | Current text content (for Edit, ComboBox, etc.) |
| `enabled` | `false` = grayed out, skip it |
| `focused` | Is this the currently focused element? |
| `actions` | **List of valid actions for this element** — only call what's listed here |
| `toggle_state` | `"On"` / `"Off"` / `"Indeterminate"` (for CheckBox) |
| `is_selected` | `true`/`false` (for ListItem, RadioButton, TabItem) |
| `expand_state` | `"Collapsed"` / `"Expanded"` / `"PartiallyExpanded"` / `"LeafNode"` |
| `range` | `{ value, minimum, maximum, step }` (for Slider, Spinner) |
| `automation_id` | Stable developer-assigned ID (good for search queries) |
| `help_text` | Tooltip text |
| `rect` | `{ x, y, width, height }` in screen coordinates |
| `children` | Child elements (nested) |

**Always check `actions` before interacting.** If `actions` is empty, the element is read-only.

---

## send-keys syntax

Plain text is typed as-is (Unicode is fine). Special keys go in curly braces; names are case-insensitive.

```
{ENTER}  {TAB}  {ESC}  {SPACE}  {BACKSPACE}  {DELETE}  {INSERT}
{HOME}   {END}  {PGUP} {PGDN}   {UP} {DOWN} {LEFT} {RIGHT}
{F1}–{F24}  {CAPSLOCK}  {PRINTSCREEN}  {MENU}

{CTRL+A}  {CTRL+C}  {CTRL+V}  {CTRL+Z}  {ALT+F4}  {WIN+D}   chords
{CTRL+SHIFT+T}  {CTRL+ALT+T}                                several modifiers
{MOD+C}   MOD = Cmd on macOS, Ctrl on Windows/Linux (portable shortcuts)
{WIN}     a lone modifier is pressed and released
{TAB 3}   {BACKSPACE 10}                                    repeat 1–100 times
{{  }}    literal { and }
```
Modifiers: `CTRL`, `ALT` (`OPTION`), `SHIFT`, `WIN` (`CMD`, `SUPER`, `META`), `MOD`.
A newline in the text presses Enter, a tab character presses Tab.
The whole string is checked before typing starts: invalid syntax (e.g. `{FOO}`, an unclosed `{`) returns
400 `invalid_input` and nothing is typed.

Examples:
```
"hello world{ENTER}"
"{CTRL+A}replacement text{ENTER}"
"{MOD+S}"
"{TAB 3}{SPACE}"
"{{\"json\": true}}"      → types {"json": true}
```

---

## Errors

All endpoints return:
```json
{ "success": true, "data": <result>, "error": null }
```
On error:
```json
{ "success": false, "data": null, "error": "Element 'a3f8c2d1e4b5f607' not found or no longer available — call find/tree again to get a fresh oculos_id.", "code": "not_found" }
```

| `code` | HTTP | What to do |
|--------|------|------------|
| `not_found` | 404 | Element id: the element is gone or the id expired → run `find` again and retry with the new id. Window: list windows again. |
| `invalid_input` | 400 | Fix the request: element type name, key syntax, scroll direction, batch step (the message says which one). Don't retry unchanged. |
| `unsupported` | 400 | This element/platform can't do that action → use one listed in `actions`, or another approach (e.g. `send-keys` instead of `set-text`). |
| `timeout` | 408 | The `wait` condition wasn't met → check the query, take a screenshot/tree to see the current state, or wait longer. |
| `permission_denied` | 403 | The OS blocked access (e.g. macOS Accessibility permission) → ask the user to grant it. |
| `forbidden` | 403 | Host/Origin rejected by the server's security checks → call `http://127.0.0.1:7878` directly. |
| `unauthorized` | 401 | Token missing or wrong → send `X-OculOS-Token`; ask the user for the token. |
| `internal` | 500 | Unexpected failure → retry once, then report. |

---

## Common patterns

### Click a button
```bash
# 1. Find the button
GET /windows/{pid}/find?q=Submit&type=Button

# 2. Click it
POST /interact/{oculos_id}/click
```

### Type into a text field
```bash
# 1. Find the input
GET /windows/{pid}/find?q=Search&type=Edit

# 2. Set text (fast, atomic)
POST /interact/{oculos_id}/set-text
{"text": "my search query"}

# 3. Submit
POST /interact/{oculos_id}/send-keys
{"keys": "{ENTER}"}
```

### Select from a dropdown
```bash
# 1. Find and expand the combo
GET /windows/{pid}/find?q=Language&type=ComboBox
POST /interact/{oculos_id}/expand

# 2. Wait for the option to show up
GET /windows/{pid}/wait?q=English&type=ListItem&timeout=3000

# 3. Select it
POST /interact/{oculos_id}/select
```

### Wait for work to finish
```bash
POST /interact/{save_button_id}/click
GET  /windows/{pid}/wait?q=Saving&until=gone&timeout=15000
```

### Control a multi-window app (e.g. Teams)
```bash
# 1. List windows — note all entries with the same PID but different hwnd
GET /windows

# 2. Inspect each window separately
GET /hwnd/{hwnd1}/tree
GET /hwnd/{hwnd2}/tree

# 3. Interact as normal using oculos_id from the relevant window
```

### Check/uncheck a checkbox
```bash
GET /windows/{pid}/find?q=Remember me&type=CheckBox
# Check toggle_state in response — "Off" means currently unchecked
POST /interact/{oculos_id}/toggle
```

---

## App compatibility

| App type | Coverage | Notes |
|----------|----------|-------|
| Win32 native | Excellent | Full tree, all interactions |
| WPF / .NET | Excellent | Full tree, all interactions |
| Electron (Chrome, Teams, VS Code, Slack) | Good | Shallow tree but key elements exposed |
| Qt | Good | Full tree |
| UWP / WinUI (Settings, Store) | Poor | Sandboxed — limited UIA access |
