import type {
  ApiResponse,
  BatchAction,
  BatchOptions,
  BatchResult,
  ErrorCode,
  FindOptions,
  HealthInfo,
  OculOSOptions,
  ScrollDirection,
  UiElement,
  WaitOptions,
  Window,
} from "./types.js";

const DEFAULT_URL = "http://127.0.0.1:7878";
const TOKEN_HEADER = "X-OculOS-Token";
/** Longest wait the server accepts, in ms. */
const MAX_WAIT_MS = 30_000;

type Params = Record<string, string | number | boolean | undefined>;

interface RequestOptions {
  params?: Params;
  body?: unknown;
  timeoutMs?: number;
  binary?: boolean;
}

/** OCULOS_TOKEN from the environment when running under Node (undefined in browsers). */
function envToken(): string | undefined {
  const proc = (globalThis as { process?: { env?: Record<string, string | undefined> } }).process;
  return proc?.env?.OCULOS_TOKEN || undefined;
}

/** Reject ids that would change the URL path (e.g. "../windows/1/close"). */
function idSeg(id: string): string {
  if (!id || /[/?#%]/.test(id) || id === "." || id === "..") {
    throw new OculOSError(`Invalid element id ${JSON.stringify(id)}`, "invalid_input");
  }
  return id;
}

function intSeg(n: number): string {
  if (!Number.isSafeInteger(n) || n < 0) {
    throw new OculOSError(`Invalid pid/hwnd ${String(n)}`, "invalid_input");
  }
  return String(n);
}

function findParams(opts: FindOptions): Params {
  return {
    q: opts.query,
    type: opts.type,
    interactive: opts.interactive === undefined ? undefined : opts.interactive,
  };
}

export class OculOS {
  readonly baseUrl: string;
  readonly timeoutMs: number;
  private readonly token?: string;

  /**
   * @param options server URL string, or `{ baseUrl, token, timeoutMs }`.
   *   `token` defaults to `process.env.OCULOS_TOKEN` when running under Node.
   */
  constructor(options: string | OculOSOptions = {}) {
    const opts: OculOSOptions = typeof options === "string" ? { baseUrl: options } : options;
    this.baseUrl = (opts.baseUrl ?? DEFAULT_URL).replace(/\/+$/, "");
    this.token = opts.token ?? envToken();
    this.timeoutMs = opts.timeoutMs ?? 30_000;
  }

  // ── Discovery ──────────────────────────────────────────────

  async listWindows(): Promise<Window[]> {
    return this.request<Window[]>("GET", "/windows");
  }

  async getTree(pid: number): Promise<UiElement> {
    return this.request<UiElement>("GET", `/windows/${intSeg(pid)}/tree`);
  }

  async getTreeHwnd(hwnd: number): Promise<UiElement> {
    return this.request<UiElement>("GET", `/hwnd/${intSeg(hwnd)}/tree`);
  }

  async findElements(pid: number, opts: FindOptions = {}): Promise<UiElement[]> {
    return this.request<UiElement[]>("GET", `/windows/${intSeg(pid)}/find`, {
      params: findParams(opts),
    });
  }

  async findElementsHwnd(hwnd: number, opts: FindOptions = {}): Promise<UiElement[]> {
    return this.request<UiElement[]>("GET", `/hwnd/${intSeg(hwnd)}/find`, {
      params: findParams(opts),
    });
  }

  /**
   * Wait until matching elements appear (or, with `until: "gone"`, disappear).
   * Pass exactly one of `pid` / `hwnd`. Rejects with an `OculOSError` whose
   * `code` is `"timeout"` when the condition is not met in time.
   */
  async waitFor(opts: WaitOptions): Promise<UiElement[]> {
    if ((opts.pid === undefined) === (opts.hwnd === undefined)) {
      throw new OculOSError("waitFor() needs exactly one of pid or hwnd", "invalid_input");
    }
    const waitMs = opts.timeoutMs ?? 5000;
    const path =
      opts.pid !== undefined ? `/windows/${intSeg(opts.pid)}/wait` : `/hwnd/${intSeg(opts.hwnd!)}/wait`;
    return this.request<UiElement[]>("GET", path, {
      params: {
        ...findParams(opts),
        interactive: opts.interactive ? true : undefined,
        timeout: waitMs,
        until: opts.until ?? "appears",
      },
      // The HTTP timeout must outlast the server-side wait.
      timeoutMs: Math.max(this.timeoutMs, Math.min(waitMs, MAX_WAIT_MS) + 10_000),
    });
  }

  // ── Window operations ──────────────────────────────────────

  async focusWindow(pid: number): Promise<void> {
    await this.request("POST", `/windows/${intSeg(pid)}/focus`);
  }

  async closeWindow(pid: number): Promise<void> {
    await this.request("POST", `/windows/${intSeg(pid)}/close`);
  }

  /** Window screenshot as PNG bytes. */
  async screenshot(pid: number): Promise<Uint8Array> {
    return this.request<Uint8Array>("GET", `/windows/${intSeg(pid)}/screenshot`, { binary: true });
  }

  /** Element screenshot as PNG bytes. */
  async screenshotElement(elementId: string): Promise<Uint8Array> {
    return this.request<Uint8Array>("GET", `/interact/${idSeg(elementId)}/screenshot`, {
      binary: true,
    });
  }

  // ── Element interactions ───────────────────────────────────

  async click(elementId: string): Promise<void> {
    await this.request("POST", `/interact/${idSeg(elementId)}/click`);
  }

  async setText(elementId: string, text: string): Promise<void> {
    await this.request("POST", `/interact/${idSeg(elementId)}/set-text`, { body: { text } });
  }

  /** Keyboard input, e.g. "hello{ENTER}", "{CTRL+SHIFT+T}", "{TAB 3}"; "{{" / "}}" for literal braces. */
  async sendKeys(elementId: string, keys: string): Promise<void> {
    await this.request("POST", `/interact/${idSeg(elementId)}/send-keys`, { body: { keys } });
  }

  async focus(elementId: string): Promise<void> {
    await this.request("POST", `/interact/${idSeg(elementId)}/focus`);
  }

  async toggle(elementId: string): Promise<void> {
    await this.request("POST", `/interact/${idSeg(elementId)}/toggle`);
  }

  async expand(elementId: string): Promise<void> {
    await this.request("POST", `/interact/${idSeg(elementId)}/expand`);
  }

  async collapse(elementId: string): Promise<void> {
    await this.request("POST", `/interact/${idSeg(elementId)}/collapse`);
  }

  async select(elementId: string): Promise<void> {
    await this.request("POST", `/interact/${idSeg(elementId)}/select`);
  }

  async setRange(elementId: string, value: number): Promise<void> {
    await this.request("POST", `/interact/${idSeg(elementId)}/set-range`, { body: { value } });
  }

  async scroll(elementId: string, direction: ScrollDirection): Promise<void> {
    await this.request("POST", `/interact/${idSeg(elementId)}/scroll`, { body: { direction } });
  }

  async scrollIntoView(elementId: string): Promise<void> {
    await this.request("POST", `/interact/${idSeg(elementId)}/scroll-into-view`);
  }

  async highlight(elementId: string, durationMs: number = 2000): Promise<void> {
    await this.request("POST", `/interact/${idSeg(elementId)}/highlight`, {
      body: { duration_ms: durationMs },
    });
  }

  /**
   * Run up to 100 actions in one request. All steps are validated first (an
   * invalid step rejects with `invalid_input` and nothing runs). Resolves with
   * one result per executed step; by default execution stops at the first failure.
   */
  async batch(actions: BatchAction[], opts: BatchOptions = {}): Promise<BatchResult[]> {
    const delayMs = opts.delayMs ?? 0;
    return this.request<BatchResult[]>("POST", "/interact/batch", {
      body: { actions, stop_on_error: opts.stopOnError ?? true, delay_ms: delayMs },
      timeoutMs: this.timeoutMs + actions.length * Math.max(delayMs, 0),
    });
  }

  // ── System ─────────────────────────────────────────────────

  async health(): Promise<HealthInfo> {
    return this.request<HealthInfo>("GET", "/health");
  }

  // ── Internals ──────────────────────────────────────────────

  private async request<T = unknown>(
    method: "GET" | "POST",
    path: string,
    opts: RequestOptions = {},
  ): Promise<T> {
    let url = `${this.baseUrl}${path}`;
    if (opts.params) {
      const qs = new URLSearchParams();
      for (const [k, v] of Object.entries(opts.params)) {
        if (v !== undefined) qs.set(k, String(v));
      }
      const s = qs.toString();
      if (s) url += `?${s}`;
    }

    const headers: Record<string, string> = {};
    if (this.token) headers[TOKEN_HEADER] = this.token;
    if (opts.body !== undefined) headers["Content-Type"] = "application/json";

    const timeoutMs = opts.timeoutMs ?? this.timeoutMs;
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), timeoutMs);
    try {
      let res: Response;
      try {
        res = await fetch(url, {
          method,
          headers,
          body: opts.body === undefined ? undefined : JSON.stringify(opts.body),
          signal: controller.signal,
        });
      } catch (e) {
        if (controller.signal.aborted) {
          throw new OculOSError(`${method} ${path} timed out after ${timeoutMs} ms`, undefined, undefined, e);
        }
        throw new OculOSError(`Cannot reach OculOS at ${this.baseUrl}: ${(e as Error).message}`, undefined, undefined, e);
      }

      const contentType = res.headers.get("content-type") ?? "";
      if (opts.binary && res.ok && !contentType.includes("json")) {
        return new Uint8Array(await res.arrayBuffer()) as T;
      }

      let body: ApiResponse<T>;
      try {
        body = (await res.json()) as ApiResponse<T>;
      } catch {
        throw new OculOSError(`HTTP ${res.status}: non-JSON response from ${method} ${path}`, undefined, res.status);
      }
      if (!body || typeof body !== "object" || !body.success) {
        throw new OculOSError(body?.error ?? `HTTP ${res.status}`, body?.code, res.status);
      }
      if (opts.binary) {
        throw new OculOSError(`Expected binary data from ${method} ${path}, got JSON`, undefined, res.status);
      }
      return body.data as T;
    } finally {
      clearTimeout(timer);
    }
  }
}

export class OculOSError extends Error {
  /** Machine-readable error kind from the server (undefined for client-side errors). */
  readonly code?: ErrorCode;
  /** HTTP status (undefined when no response was received). */
  readonly status?: number;
  /** Underlying error (network failure, abort…). */
  readonly cause?: unknown;

  constructor(message: string, code?: ErrorCode, status?: number, cause?: unknown) {
    super(message);
    this.name = "OculOSError";
    this.code = code;
    this.status = status;
    if (cause !== undefined) this.cause = cause;
  }
}
