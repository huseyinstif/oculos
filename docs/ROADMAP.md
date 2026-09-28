# OculOS Roadmap

_Last updated: 2026-09-28. Based on a survey of the computer-use landscape (sources linked
inline; some figures come from secondary reporting and are approximate)._

## Where OculOS stands

Frontier models now operate computers from pixels alone — OSWorld-Verified scores are in
the 75–86% range (human baseline ≈72%). The value of an accessibility layer like OculOS is
therefore no longer raw capability but **speed, token cost, determinism and
verifiability**. OculOS should position itself as the fastest, cheapest *cross-platform*
structured layer that complements a model's native computer-use tool (Claude, GPT, Gemini),
not as a replacement for vision.

Closest alternatives:

| Project | Platforms | Notable ideas |
|---|---|---|
| [Terminator](https://github.com/mediar-ai/terminator) (Rust) | Windows | compact YAML tree, `role:Button && name:Save` selectors, before/after UI diffs, workflows with fallbacks, recorder → deterministic code, opt-in OCR/vision |
| [Windows-MCP](https://github.com/CursorTouch/Windows-MCP) (Python) | Windows | labelled snapshots + optional screenshot, coordinate actions, clipboard/process/app tools, Streamable HTTP with auth. Had [CVE-2026-48989](https://github.com/CursorTouch/Windows-MCP/security/advisories/GHSA-vrxg-gm77-7q5g): wildcard CORS + no auth — the same class of bug OculOS fixed in 0.2.0 |
| [Cua Driver](https://github.com/trycua/cua) | Windows, macOS, Linux | a11y tree + screenshots, element refs, background (no focus-steal) input — the main cross-platform competitor |
| [Peekaboo](https://github.com/steipete/Peekaboo) | macOS | annotated screenshots with element ids, fuzzy click with wait, background input |
| [UFO²](https://github.com/microsoft/UFO) | Windows | hybrid UIA + OmniParser detection, GUI + native API (COM) actions, speculative multi-action |
| Anthropic `browser_toolset` | Browser | `read_page` text snapshot (`button "Search" [ref_4]`), stable refs, depth/subtree scoping, char budget |

OculOS's differentiators: Rust, three operating systems, REST + WebSocket + MCP in one
binary, an explicit `actions` list per element.

## Done in 0.2.0

Security hardening (Host/Origin checks, optional token, no wildcard CORS), MCP protocol
fixes and new tools (wait, screenshots as images, batch, highlight, annotations, compact
output), stable element ids with a bounded registry, a shared send-keys parser, Windows UIA
CacheRequest, Linux accessibility-bus + concurrent queries, macOS rect/window fixes, typed
error codes, tests and CI enforcement. See [CHANGELOG](../CHANGELOG.md).

## P0 — quick wins (days)

1. **Compact text snapshot format.** Tree/find output as indented lines, e.g.
   `button "Save" [3f2a…] click` with `depth`, subtree (`from=<id>`), visible-only,
   interactive-only and a character budget; collapse unnamed wrapper panes. Expected to cut
   tokens several-fold versus JSON (estimate, to be measured).
2. **Post-action feedback.** Every action optionally returns a compact diff of the window
   (added / removed / changed nodes + the newly focused element), and can take a wait
   condition in the same call (`click … then wait for "Saved"`). This targets the main
   failure mode of long tasks: acting without verifying.
3. **Coordinate primitives.** Click / double / right / drag / hover / scroll at `x,y` plus a
   display inventory (monitors, scale factors), in the same coordinate space as screenshots,
   so OculOS refs and a model's native computer tool can be mixed.
4. **MCP modernisation.** Move to the official Rust SDK (`rmcp`), add Streamable HTTP
   transport (with the same Host/Origin/token protections), `structuredContent` +
   `outputSchema`, and confirmation for destructive tools (elicitation / the 2026-07-28
   multi-round-trip `input_required` flow).

## P1 — weeks

5. **Event-driven waits.** UIA event handlers, AT-SPI signals and AXObserver instead of
   polling; `wait_for` resolves on the event, and events stream over the WebSocket.
6. **Selectors + auto re-resolution.** Selectors like `role:Button && name:Save` or
   automation-id paths returned with every element; stale ids are transparently
   re-resolved. The dashboard recorder already records selectors — replay them server-side.
7. **Vision fallback.** Set-of-Mark screenshots annotated with element ids, `zoom` on a
   region, and OS-native OCR (Windows.Media.Ocr, Apple Vision, Tesseract) merged into the
   tree as text nodes for canvas / custom-drawn / shallow Electron UIs. Capture covered
   windows with Windows.Graphics.Capture / ScreenCaptureKit / xdg portals; add macOS and
   Linux screenshots.
8. **System primitives.** Launch app / file / URL, clipboard get/set, window
   move/resize/minimise/maximise, process list, file-dialog helper.
9. **Safety layer.** Per-app allow/deny list (finance, crypto, password managers denied by
   default), confirmation for destructive actions, dry-run mode, JSONL audit log with
   before/after screenshots, a global kill-switch hotkey, password-field redaction, and
   marking on-screen text as untrusted data in tool outputs.

## P2 — bigger bets

10. **Background mode.** Prefer pattern-only actions (Invoke/Value/Toggle work without
    focus) and report `requires_focus` per action; later an isolated agent desktop.
11. **Browser / Electron bridge.** Chrome DevTools Protocol (`Accessibility.getFullAXTree`,
    DOM) to fix shallow Electron trees.
12. **App adapters (GUI + API).** COM for Office, AppleScript/JXA on macOS, D-Bus on Linux —
    hybrid GUI+API agents need far fewer steps.
13. **Distribution.** One-line MCP install (MCPB / `claude mcp add`), signed binaries, a
    live-view/approval UI.
14. **Public evals.** Run WindowsAgentArena and OSWorld subsets; publish success rate,
    latency per action and tokens per task against Terminator, Windows-MCP and Cua Driver.

## Performance plan

- Done: UIA CacheRequest (whole tree in one cross-process call, server-side find
  conditions), Linux concurrent D-Bus queries + filter-before-build, macOS batched attribute
  reads, compact MCP output, batch execution in a single blocking task.
- Next: text snapshots (fewer tokens), diffs instead of re-reading trees, event-driven
  waits instead of 250 ms polling, a dedicated UIA MTA thread with a warm cache per window,
  and published latency benchmarks.
