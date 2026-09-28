"""Open Notepad's text area and type into it."""

import sys, os
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "sdk", "python"))

from oculos import OculOS

client = OculOS()  # token from $OCULOS_TOKEN if the server needs one

# Find Notepad window
windows = client.list_windows()
notepad = next((w for w in windows if "notepad" in w["exe_name"].lower()), None)

if not notepad:
    print("Notepad not found. Open Notepad first.")
    sys.exit(1)

pid = notepad["pid"]
print(f"Found Notepad — PID {pid}")

# Focus the window
client.focus_window(pid)

# Find the text editor area
editors = client.find_elements(pid, element_type="Edit", interactive=True)
if not editors:
    editors = client.find_elements(pid, element_type="Document", interactive=True)
if not editors:
    print("No text area found.")
    sys.exit(1)

editor = editors[0]
print(f"Found editor: type={editor['type']} (id: {editor['oculos_id']})")

# Replace the content, then append a line with keyboard input.
# send-keys syntax: {ENTER}, {TAB 3} (repeat), {CTRL+SHIFT+T} (chords),
# {MOD+A} (Cmd on macOS, Ctrl elsewhere), {{ and }} for literal braces.
client.set_text(editor["oculos_id"], "Hello from OculOS! 🚀")
client.send_keys(editor["oculos_id"], "{CTRL+END}{ENTER}{{braces}} and a tab:{TAB}done")
print("Text set!")
