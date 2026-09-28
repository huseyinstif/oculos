"""Find all buttons in a window and highlight them one by one."""

import sys, os, time
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "sdk", "python"))

from oculos import OculOS, OculOSError

client = OculOS()  # token from $OCULOS_TOKEN if the server needs one

# Use the first window
windows = client.list_windows()
if not windows:
    print("No windows found.")
    sys.exit(1)

# Pick a window (prefer Calculator, fall back to first)
target = next((w for w in windows if "calc" in w["exe_name"].lower()), windows[0])
pid = target["pid"]
print(f"Target: {target['title']} (PID {pid})")

# Find all buttons
buttons = client.find_elements(pid, element_type="Button", interactive=True)
print(f"Found {len(buttons)} buttons\n")

for i, btn in enumerate(buttons[:10]):  # limit to 10
    label = btn["label"] or "(no label)"
    print(f"  [{i+1}] Highlighting: {label}")
    try:
        client.highlight(btn["oculos_id"], duration_ms=800)
    except OculOSError as e:
        print(f"      skip — {e}")  # e.g. [unsupported] on platforms without an overlay
    time.sleep(1)

print("\nDone!")
