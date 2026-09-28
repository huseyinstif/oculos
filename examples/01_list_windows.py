"""List all open windows and their details.

Set OCULOS_TOKEN if the server requires a token (the SDK reads it automatically).
"""

import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "sdk", "python"))

from oculos import OculOS

client = OculOS()  # http://127.0.0.1:7878, token from $OCULOS_TOKEN

health = client.health()
print(f"OculOS {health['version']} on {health['platform']} "
      f"(auth {'required' if health.get('auth_required') else 'off'})")

windows = client.list_windows()
print(f"Found {len(windows)} windows:\n")

for w in windows:
    print(f"  PID: {w['pid']:>6}  HWND: {w['hwnd']:>10}  {w['exe_name']:<25} {w['title']}")
