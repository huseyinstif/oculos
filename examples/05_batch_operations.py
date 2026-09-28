"""Demonstrate batch operations — multiple actions in one request.

All steps are validated before anything runs (an invalid step -> OculOSError
with code "invalid_input"). By default the batch stops at the first failing
step; pass stop_on_error=False to run every step regardless.
"""

import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "sdk", "python"))

from oculos import OculOS

client = OculOS()  # token from $OCULOS_TOKEN if the server needs one

# Find a window with interactive elements
windows = client.list_windows()
if not windows:
    print("No windows found.")
    sys.exit(1)

target = windows[0]
pid = target["pid"]
print(f"Target: {target['title']} (PID {pid})")

# Find the first few buttons
buttons = client.find_elements(pid, element_type="Button", interactive=True)
if len(buttons) < 2:
    print("Not enough buttons found for batch demo.")
    sys.exit(1)

# Focus the first 3 buttons in sequence, 300 ms apart
actions = [{"element_id": btn["oculos_id"], "action": "focus"} for btn in buttons[:3]]
# A step that is valid but will fail at run time (unknown element)
actions.append({"element_id": "ffffffffffffffff", "action": "focus"})

print(f"Sending batch with {len(actions)} actions (stop_on_error=False)...")
results = client.batch(actions, stop_on_error=False, delay_ms=300)

for result in results:
    status = "✅" if result["success"] else f"❌ [{result.get('code')}] {result['error']}"
    print(f"  [{result['index']}] {result['action']} → {status}")

print("\nDone!")
