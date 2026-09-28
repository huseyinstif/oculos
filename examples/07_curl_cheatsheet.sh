#!/bin/bash
# OculOS curl cheatsheet — every endpoint in one file.
# Make sure OculOS is running: ./target/release/oculos
#
# Auth: only needed when the server runs with --token or is bound to a
# non-loopback address (it then prints a generated token at startup).
#   export OCULOS_TOKEN=...      # sent as X-OculOS-Token on every call below
# (Authorization: Bearer <token> works too; WebSocket clients use /ws?token=<token>.)
#
# Every JSON reply is {"success":bool,"data":...,"error":...,"code"?:...}; on errors
# "code" is one of not_found, invalid_input, unsupported, timeout,
# permission_denied, forbidden, unauthorized, internal.

BASE="${OCULOS_URL:-http://127.0.0.1:7878}"
TOKEN="${OCULOS_TOKEN:-}"
AUTH=()
[ -n "$TOKEN" ] && AUTH=(-H "X-OculOS-Token: $TOKEN")

# curl wrapper: adds the token header (if any)
oc() { curl -s "${AUTH[@]}" "$@"; }
# POST a JSON body
post() { oc -X POST -H "Content-Type: application/json" -d "$2" "$BASE$1"; }
pp() { python3 -m json.tool; }

# ── Health (no token needed) ──────────────────────────────────────────────────
echo "=== Health ==="
curl -s "$BASE/health" | pp          # … "auth_required": true|false

# ── List windows ──────────────────────────────────────────────────────────────
echo -e "\n=== Windows ==="
oc "$BASE/windows" | pp

# ── Get UI tree (replace PID / HWND) ─────────────────────────────────────────
PID=12345
HWND=67890
echo -e "\n=== UI Tree (PID=$PID) ==="
oc "$BASE/windows/$PID/tree" | pp | head -50
echo -e "\n=== UI Tree (HWND=$HWND) — several windows per process (Windows/macOS; Linux: use PID) ==="
oc "$BASE/hwnd/$HWND/tree" | pp | head -50

# ── Find elements ────────────────────────────────────────────────────────────
# q = substring of label/automation_id, type = element type (case-insensitive;
# an unknown type → 400 invalid_input listing the valid ones).
echo -e "\n=== Find Buttons ==="
oc "$BASE/windows/$PID/find?type=button&interactive=true" | pp
oc -G --data-urlencode "q=Save as" "$BASE/hwnd/$HWND/find" | pp

# ── Wait for element ─────────────────────────────────────────────────────────
# timeout in ms (default 5000, max 30000); 408 "timeout" when it expires.
echo -e "\n=== Wait for Submit button (5s timeout) ==="
oc "$BASE/windows/$PID/wait?q=Submit&type=Button&timeout=5000" | pp
echo -e "\n=== Wait by HWND ==="
oc "$BASE/hwnd/$HWND/wait?q=OK&type=Button&timeout=3000" | pp
echo -e "\n=== Wait until a 'Loading' element is gone ==="
oc "$BASE/windows/$PID/wait?q=Loading&until=gone&timeout=10000" | pp

# ── Screenshots (PNG) ────────────────────────────────────────────────────────
echo -e "\n=== Window screenshot ==="
oc -o window.png "$BASE/windows/$PID/screenshot"
echo "Saved window.png ($(wc -c < window.png) bytes)"

ID="0123456789abcdef"   # an oculos_id from find/tree (stable for the same element)
echo -e "\n=== Element screenshot ==="
oc -o element.png "$BASE/interact/$ID/screenshot"
echo "Saved element.png ($(wc -c < element.png) bytes)"

# ── Click ─────────────────────────────────────────────────────────────────────
echo -e "\n=== Click ==="
oc -X POST "$BASE/interact/$ID/click" | pp

# ── Set text ──────────────────────────────────────────────────────────────────
echo -e "\n=== Set Text ==="
post "/interact/$ID/set-text" '{"text":"Hello from OculOS"}' | pp

# ── Send keys ─────────────────────────────────────────────────────────────────
# Plain text is typed as-is ("\n" = Enter, "\t" = Tab). Special keys in braces:
#   {ENTER} {TAB} {ESC} {SPACE} {BACKSPACE} {DELETE} {HOME} {END} {PGUP} {PGDN}
#   {UP} {DOWN} {LEFT} {RIGHT} {F1}…{F24} {INSERT} {CAPSLOCK} {PRINTSCREEN} {MENU}
# Chords: {CTRL+A}, {CTRL+SHIFT+T}, {ALT+F4}, {WIN+D}; {MOD+C} = Cmd on macOS, Ctrl elsewhere
# Repeat: {TAB 3}, {BACKSPACE 10} (1–100).  Literal braces: {{ and }}.
# Names are case-insensitive; invalid syntax → 400 invalid_input (nothing is typed).
echo -e "\n=== Send Keys ==="
post "/interact/$ID/send-keys" '{"keys":"{CTRL+A}new text{ENTER}"}' | pp
post "/interact/$ID/send-keys" '{"keys":"{CTRL+SHIFT+T}"}' | pp
post "/interact/$ID/send-keys" '{"keys":"{TAB 3}{SPACE}"}' | pp
post "/interact/$ID/send-keys" '{"keys":"{{\"json\": true}}{ENTER}"}' | pp   # types {"json": true}
post "/interact/$ID/send-keys" '{"keys":"{MOD+S}"}' | pp                   # save (Cmd/Ctrl)

# ── Focus / Toggle / Expand / Collapse / Select / Scroll into view ───────────
echo -e "\n=== Focus ===";            oc -X POST "$BASE/interact/$ID/focus" | pp
echo -e "\n=== Toggle ===";           oc -X POST "$BASE/interact/$ID/toggle" | pp
echo -e "\n=== Expand ===";           oc -X POST "$BASE/interact/$ID/expand" | pp
echo -e "\n=== Collapse ===";         oc -X POST "$BASE/interact/$ID/collapse" | pp
echo -e "\n=== Select ===";           oc -X POST "$BASE/interact/$ID/select" | pp
echo -e "\n=== Scroll into view ===";  oc -X POST "$BASE/interact/$ID/scroll-into-view" | pp

# ── Set range (slider) ───────────────────────────────────────────────────────
echo -e "\n=== Set Range ==="
post "/interact/$ID/set-range" '{"value":75}' | pp

# ── Scroll ────────────────────────────────────────────────────────────────────
# direction: up, down, left, right, page-up, page-down
echo -e "\n=== Scroll Down ==="
post "/interact/$ID/scroll" '{"direction":"down"}' | pp

# ── Highlight ─────────────────────────────────────────────────────────────────
echo -e "\n=== Highlight ==="
post "/interact/$ID/highlight" '{"duration_ms":2000}' | pp

# ── Batch ─────────────────────────────────────────────────────────────────────
# Up to 100 actions; all are validated first (one bad step → 400, nothing runs).
# stop_on_error (default true) stops at the first failing step; delay_ms pauses
# between steps (max 5000). Each result: {index, action, element_id, success, error, code?}
echo -e "\n=== Batch ==="
post "/interact/batch" '{
  "actions": [
    {"element_id": "0123456789abcdef", "action": "set-text", "text": "Ada"},
    {"element_id": "0123456789abcdef", "action": "send-keys", "keys": "{TAB 2}{ENTER}"},
    {"element_id": "fedcba9876543210", "action": "click"}
  ],
  "stop_on_error": true,
  "delay_ms": 200
}' | pp

# ── Focus window ──────────────────────────────────────────────────────────────
echo -e "\n=== Focus Window ==="
oc -X POST "$BASE/windows/$PID/focus" | pp

# ── Close window ──────────────────────────────────────────────────────────────
echo -e "\n=== Close Window ==="
oc -X POST "$BASE/windows/$PID/close" | pp

# ── WebSocket (live events) ──────────────────────────────────────────────────
# websocat "ws://127.0.0.1:7878/ws${TOKEN:+?token=$TOKEN}"
# → {"type":"action","data":{"action":"click","element_id":"…","success":true}}
