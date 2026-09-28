"""OculOS Python client."""

from __future__ import annotations

import os
from typing import Any, Iterable, Mapping, Optional

import requests

DEFAULT_URL = "http://127.0.0.1:7878"
TOKEN_HEADER = "X-OculOS-Token"

#: Longest wait the server accepts (``GET .../wait?timeout=``), in milliseconds.
MAX_WAIT_MS = 30_000


class OculOSError(Exception):
    """Raised when the OculOS API returns an error.

    Attributes:
        message: the server's error message.
        code: machine-readable error kind, or ``None`` if the server sent none:
            ``not_found`` (stale element id / window gone -> find again),
            ``invalid_input``, ``unsupported``, ``timeout``, ``permission_denied``,
            ``forbidden``, ``unauthorized`` (token missing or wrong), ``internal``.
        status: HTTP status code.
    """

    def __init__(
        self, message: str, code: Optional[str] = None, status: Optional[int] = None
    ):
        super().__init__(message)
        self.message = message
        self.code = code
        self.status = status

    def __str__(self) -> str:
        return f"{self.message} [{self.code}]" if self.code else self.message

    def __repr__(self) -> str:
        return (
            f"OculOSError({self.message!r}, code={self.code!r}, status={self.status!r})"
        )


class OculOS:
    """Thin wrapper around the OculOS REST API.

    Args:
        base_url: server address (default ``http://127.0.0.1:7878``).
        token: API token, sent as ``X-OculOS-Token``. Defaults to the
            ``OCULOS_TOKEN`` environment variable; only needed when the server
            runs with ``--token`` or is bound to a non-loopback address.
        timeout: HTTP timeout in seconds for ordinary requests. Waits and
            batches automatically get a longer one.
    """

    def __init__(
        self,
        base_url: str = DEFAULT_URL,
        token: Optional[str] = None,
        timeout: float = 30.0,
    ):
        self.base_url = base_url.rstrip("/")
        self.token = token if token is not None else os.environ.get("OCULOS_TOKEN") or None
        self.timeout = timeout
        self._session = requests.Session()
        if self.token:
            self._session.headers[TOKEN_HEADER] = self.token

    # ── Discovery ──────────────────────────────────────────────

    def list_windows(self) -> list[dict]:
        """List all visible windows."""
        return self._get("/windows")

    def get_tree(self, pid: int) -> dict:
        """Get the full UI element tree for a window."""
        return self._get(f"/windows/{int(pid)}/tree")

    def get_tree_hwnd(self, hwnd: int) -> dict:
        """Get the UI element tree by window handle."""
        return self._get(f"/hwnd/{int(hwnd)}/tree")

    def find_elements(
        self,
        pid: int,
        *,
        query: Optional[str] = None,
        element_type: Optional[str] = None,
        interactive: Optional[bool] = None,
    ) -> list[dict]:
        """Search for UI elements in a window.

        ``query`` is a case-insensitive substring of the label or automation_id;
        ``element_type`` (e.g. ``"Button"``) is case-insensitive.
        """
        params = _find_params(query, element_type, interactive)
        return self._get(f"/windows/{int(pid)}/find", params=params)

    def find_elements_hwnd(
        self,
        hwnd: int,
        *,
        query: Optional[str] = None,
        element_type: Optional[str] = None,
        interactive: Optional[bool] = None,
    ) -> list[dict]:
        """Search for UI elements by window handle."""
        params = _find_params(query, element_type, interactive)
        return self._get(f"/hwnd/{int(hwnd)}/find", params=params)

    def wait_for(
        self,
        pid: Optional[int] = None,
        hwnd: Optional[int] = None,
        q: Optional[str] = None,
        type: Optional[str] = None,  # noqa: A002 — mirrors the API parameter
        interactive: bool = False,
        timeout_ms: int = 5000,
        until: str = "appears",
    ) -> list[dict]:
        """Wait until matching elements appear (or, with ``until="gone"``, disappear).

        Pass exactly one of ``pid`` or ``hwnd``. Returns the matching elements
        (an empty list for ``until="gone"``). Raises :class:`OculOSError` with
        ``code == "timeout"`` when the condition is not met within
        ``timeout_ms`` (server maximum: 30000).
        """
        if (pid is None) == (hwnd is None):
            raise ValueError("wait_for() needs exactly one of pid or hwnd")
        params = _find_params(q, type, interactive or None)
        params["timeout"] = int(timeout_ms)
        params["until"] = until
        path = f"/windows/{int(pid)}/wait" if pid is not None else f"/hwnd/{int(hwnd)}/wait"
        # The HTTP timeout must outlast the server-side wait.
        http_timeout = max(self.timeout, min(int(timeout_ms), MAX_WAIT_MS) / 1000.0 + 10.0)
        return self._request("GET", path, params=params, timeout=http_timeout)

    # ── Window operations ──────────────────────────────────────

    def focus_window(self, pid: int) -> None:
        """Bring a window to the foreground."""
        self._post(f"/windows/{int(pid)}/focus")

    def close_window(self, pid: int) -> None:
        """Close a window gracefully."""
        self._post(f"/windows/{int(pid)}/close")

    def screenshot(self, pid: int) -> bytes:
        """Capture a window as PNG bytes."""
        return self._request("GET", f"/windows/{int(pid)}/screenshot", binary=True)

    def screenshot_element(self, element_id: str) -> bytes:
        """Capture a single element as PNG bytes."""
        return self._request("GET", f"/interact/{_id(element_id)}/screenshot", binary=True)

    # ── Element interactions ───────────────────────────────────

    def click(self, element_id: str) -> dict:
        """Click an element."""
        return self._post(f"/interact/{_id(element_id)}/click")

    def set_text(self, element_id: str, text: str) -> dict:
        """Replace the text content of an input field."""
        return self._post(f"/interact/{_id(element_id)}/set-text", json={"text": text})

    def send_keys(self, element_id: str, keys: str) -> dict:
        """Send keyboard input, e.g. ``"hello{ENTER}"``, ``"{CTRL+SHIFT+T}"``, ``"{TAB 3}"``.

        Use ``{{`` / ``}}`` for literal braces. Invalid syntax raises
        :class:`OculOSError` (``invalid_input``) before anything is typed.
        """
        return self._post(f"/interact/{_id(element_id)}/send-keys", json={"keys": keys})

    def focus(self, element_id: str) -> dict:
        """Move keyboard focus to an element."""
        return self._post(f"/interact/{_id(element_id)}/focus")

    def toggle(self, element_id: str) -> dict:
        """Toggle a checkbox or toggle button."""
        return self._post(f"/interact/{_id(element_id)}/toggle")

    def expand(self, element_id: str) -> dict:
        """Expand a dropdown, tree item, or menu."""
        return self._post(f"/interact/{_id(element_id)}/expand")

    def collapse(self, element_id: str) -> dict:
        """Collapse a dropdown, tree item, or menu."""
        return self._post(f"/interact/{_id(element_id)}/collapse")

    def select(self, element_id: str) -> dict:
        """Select a list item, radio button, or tab."""
        return self._post(f"/interact/{_id(element_id)}/select")

    def set_range(self, element_id: str, value: float) -> dict:
        """Set a slider or spinner value."""
        return self._post(f"/interact/{_id(element_id)}/set-range", json={"value": value})

    def scroll(self, element_id: str, direction: str) -> dict:
        """Scroll a container: up, down, left, right, page-up or page-down."""
        return self._post(
            f"/interact/{_id(element_id)}/scroll", json={"direction": direction}
        )

    def scroll_into_view(self, element_id: str) -> dict:
        """Scroll an element into the visible viewport."""
        return self._post(f"/interact/{_id(element_id)}/scroll-into-view")

    def highlight(self, element_id: str, duration_ms: int = 2000) -> dict:
        """Highlight an element on screen."""
        return self._post(
            f"/interact/{_id(element_id)}/highlight", json={"duration_ms": duration_ms}
        )

    def batch(
        self,
        actions: Iterable[Mapping[str, Any]],
        stop_on_error: bool = True,
        delay_ms: int = 0,
    ) -> list[dict]:
        """Run several interactions in one request (max 100).

        Each action is ``{"element_id": ..., "action": ...}`` plus ``text``,
        ``keys``, ``value`` or ``direction`` where the action needs it. All
        steps are validated first: an invalid step raises
        :class:`OculOSError` (``invalid_input``) and nothing runs. Returns one
        ``{index, action, element_id, success, error, code?}`` per executed
        step; with ``stop_on_error`` (default) execution stops at the first
        failure.
        """
        actions = [dict(a) for a in actions]
        body = {"actions": actions, "stop_on_error": bool(stop_on_error), "delay_ms": int(delay_ms)}
        http_timeout = self.timeout + len(actions) * max(int(delay_ms), 0) / 1000.0
        return self._request("POST", "/interact/batch", json=body, timeout=http_timeout)

    # ── System ─────────────────────────────────────────────────

    def health(self) -> dict:
        """Server status, version, platform, uptime and ``auth_required``."""
        return self._get("/health")

    # ── Internals ──────────────────────────────────────────────

    def _get(self, path: str, params: Optional[dict] = None) -> Any:
        return self._request("GET", path, params=params)

    def _post(self, path: str, json: Optional[dict] = None) -> Any:
        return self._request("POST", path, json=json)

    def _request(
        self,
        method: str,
        path: str,
        *,
        params: Optional[dict] = None,
        json: Optional[dict] = None,
        timeout: Optional[float] = None,
        binary: bool = False,
    ) -> Any:
        r = self._session.request(
            method,
            f"{self.base_url}{path}",
            params=params,
            json=json,
            timeout=timeout if timeout is not None else self.timeout,
        )
        content_type = r.headers.get("Content-Type", "")
        if binary and r.ok and "json" not in content_type:
            return r.content
        try:
            body = r.json()
        except ValueError:
            raise OculOSError(
                f"HTTP {r.status_code}: non-JSON response from {method} {path}",
                status=r.status_code,
            ) from None
        if not isinstance(body, dict) or not body.get("success"):
            body = body if isinstance(body, dict) else {}
            raise OculOSError(
                body.get("error") or f"HTTP {r.status_code}",
                code=body.get("code"),
                status=r.status_code,
            )
        if binary:
            raise OculOSError(
                f"Expected binary data from {method} {path}, got JSON", status=r.status_code
            )
        return body.get("data")


def _find_params(
    query: Optional[str], element_type: Optional[str], interactive: Optional[bool]
) -> dict[str, Any]:
    params: dict[str, Any] = {}
    if query is not None:
        params["q"] = query
    if element_type is not None:
        params["type"] = element_type
    if interactive is not None:
        params["interactive"] = str(bool(interactive)).lower()
    return params


def _id(element_id: str) -> str:
    """Validate an element id before putting it into a URL path."""
    element_id = str(element_id)
    if not element_id or any(c in element_id for c in "/?#%") or element_id in (".", ".."):
        raise OculOSError(f"Invalid element id {element_id!r}", code="invalid_input")
    return element_id
