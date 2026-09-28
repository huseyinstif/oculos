"""Demonstrate waiting — for an element to appear, and for one to disappear."""

import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "sdk", "python"))

from oculos import OculOS, OculOSError

client = OculOS()  # token from $OCULOS_TOKEN if the server needs one

# Pick first window
windows = client.list_windows()
if not windows:
    print("No windows found.")
    sys.exit(1)

target = windows[0]
pid = target["pid"]
print(f"Target: {target['title']} (PID {pid})")

# Wait for a Button to appear (should be instant for most apps)
print("Waiting for a Button element (timeout: 3s)...")
try:
    buttons = client.wait_for(pid=pid, type="Button", interactive=True, timeout_ms=3000)
    print(f"Found {len(buttons)} buttons!")
    for btn in buttons[:5]:
        print(f"  - {btn['label']}")
except OculOSError as e:
    if e.code != "timeout":
        raise
    print("Timeout — no matching element found.")

# Wait for something to go away, e.g. a "Loading" spinner or progress dialog.
# For apps with several windows per process, wait on one window with hwnd=
# (Windows and macOS; on Linux hwnd is 0, so use pid=).
where = {"hwnd": target["hwnd"]} if target["hwnd"] else {"pid": pid}
print("\nWaiting until no element matches 'Loading' (timeout: 2s)...")
try:
    client.wait_for(**where, q="Loading", until="gone", timeout_ms=2000)
    print("Nothing matches 'Loading' — ready.")
except OculOSError as e:
    if e.code != "timeout":
        raise
    print("Still loading after 2s.")
