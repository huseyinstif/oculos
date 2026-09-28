"""Find a button in Calculator and click it."""

import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "sdk", "python"))

from oculos import OculOS, OculOSError

client = OculOS()  # token from $OCULOS_TOKEN if the server needs one

# Find Calculator window
windows = client.list_windows()
calc = next((w for w in windows if "calc" in w["exe_name"].lower()), None)

if not calc:
    print("Calculator not found. Open Calculator first.")
    sys.exit(1)

pid = calc["pid"]
print(f"Found Calculator — PID {pid}")

# Find the "5" button (the element type filter is case-insensitive)
buttons = client.find_elements(pid, query="5", element_type="Button", interactive=True)
if not buttons:
    print("Button '5' not found.")
    sys.exit(1)

btn = buttons[0]
print(f"Found button: '{btn['label']}' (id: {btn['oculos_id']})")

# Click it. Ids are stable, but if the element went away the server answers
# with code "not_found" — just search again.
try:
    client.click(btn["oculos_id"])
except OculOSError as e:
    if e.code != "not_found":
        raise
    client.click(client.find_elements(pid, query="5", element_type="Button")[0]["oculos_id"])
print("Clicked!")
