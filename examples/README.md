# Examples

Make sure OculOS is running before running any example:

```bash
./target/release/oculos
```

If the server requires a token (started with `--token`, or bound to a non-loopback
address), export it first — the Python SDK and the curl cheatsheet pick it up:

```bash
export OCULOS_TOKEN=...   # printed in the server log at startup
```

| Example | Description |
|---------|-------------|
| `01_list_windows.py` | List all open windows |
| `02_find_and_click.py` | Find a Calculator button and click it |
| `03_type_in_notepad.py` | Type text into Notepad (`set_text` + `send_keys` syntax) |
| `04_highlight_all_buttons.py` | Highlight all buttons in a window one by one |
| `05_batch_operations.py` | Execute multiple actions in one request (`stop_on_error`, `delay_ms`) |
| `06_wait_for_element.py` | Wait for an element to appear, or to be gone (`until="gone"`) |
| `07_curl_cheatsheet.sh` | Every API endpoint as a curl command |

### Run

```bash
python examples/01_list_windows.py
```
