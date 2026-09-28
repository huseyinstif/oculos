/** Every element type the server emits (mirrors `ElementType` in src/types.rs). */
export const ELEMENT_TYPES = [
  "Window",
  "Button",
  "SplitButton",
  "Edit",
  "Text",
  "CheckBox",
  "RadioButton",
  "ComboBox",
  "ListBox",
  "ListItem",
  "TreeView",
  "TreeItem",
  "Menu",
  "MenuBar",
  "MenuItem",
  "TabControl",
  "TabItem",
  "ToolBar",
  "StatusBar",
  "ScrollBar",
  "Slider",
  "Spinner",
  "ProgressBar",
  "Image",
  "Link",
  "Group",
  "Pane",
  "Dialog",
  "Document",
  "DataGrid",
  "DataItem",
  "Header",
  "HeaderItem",
  "Table",
  "TitleBar",
  "ToolTip",
  "Separator",
  "Calendar",
  "Thumb",
  "Custom",
  "Unknown",
] as const;

export type ElementType = (typeof ELEMENT_TYPES)[number];

/** Actions listed in `UiElement.actions` (and accepted by batch requests). */
export type ElementAction =
  | "click"
  | "set-text"
  | "send-keys"
  | "focus"
  | "toggle"
  | "expand"
  | "collapse"
  | "select"
  | "set-range"
  | "scroll"
  | "scroll-into-view";

export type ScrollDirection = "up" | "down" | "left" | "right" | "page-up" | "page-down";

export type ToggleState = "On" | "Off" | "Indeterminate";

export type ExpandState = "Collapsed" | "Expanded" | "PartiallyExpanded" | "LeafNode";

/** Machine-readable error kinds returned in the `code` field. */
export type ErrorCode =
  | "not_found"
  | "invalid_input"
  | "unsupported"
  | "timeout"
  | "permission_denied"
  | "forbidden"
  | "unauthorized"
  | "internal";

export interface Window {
  pid: number;
  hwnd: number;
  title: string;
  exe_name: string;
  rect: Rect;
  visible: boolean;
}

export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface UiElement {
  /** Stable id (16 hex chars) — same element, same id; expires after 30 min idle. */
  oculos_id: string;
  type: ElementType;
  label: string;
  value: string | null;
  text_content?: string | null;
  enabled: boolean;
  focused: boolean;
  is_keyboard_focusable?: boolean;
  actions: ElementAction[];
  toggle_state: ToggleState | null;
  is_selected: boolean | null;
  expand_state: ExpandState | null;
  range: Range | null;
  automation_id: string | null;
  class_name?: string | null;
  help_text: string | null;
  keyboard_shortcut?: string | null;
  rect: Rect;
  children: UiElement[];
}

export interface Range {
  value: number;
  minimum: number;
  maximum: number;
  step: number;
  read_only?: boolean;
}

export interface HealthInfo {
  status: string;
  version: string;
  platform: string;
  arch: string;
  uptime_secs: number;
  /** True when the server requires an API token. */
  auth_required: boolean;
}

export interface FindOptions {
  /** Case-insensitive substring of the label or automation_id. */
  query?: string;
  /** Element type filter (case-insensitive). */
  type?: ElementType | (string & {});
  /** Only elements that have at least one action. */
  interactive?: boolean;
}

export interface WaitOptions extends FindOptions {
  /** Target window by process id… */
  pid?: number;
  /** …or by window handle (exactly one of pid / hwnd). */
  hwnd?: number;
  /** How long the server waits, in ms (default 5000, max 30000). */
  timeoutMs?: number;
  /** "appears" (default) or "gone" — wait until nothing matches. */
  until?: "appears" | "gone";
}

export interface BatchAction {
  element_id: string;
  action: ElementAction;
  text?: string;
  keys?: string;
  value?: number;
  direction?: ScrollDirection;
}

export interface BatchOptions {
  /** Stop at the first failing step (default true). */
  stopOnError?: boolean;
  /** Pause between steps in ms (default 0, max 5000). */
  delayMs?: number;
}

export interface BatchResult {
  index: number;
  action: string;
  element_id: string;
  success: boolean;
  error: string | null;
  code?: ErrorCode;
}

export interface ApiResponse<T> {
  success: boolean;
  data: T | null;
  error: string | null;
  code?: ErrorCode;
}

export interface OculOSOptions {
  /** Server address (default http://127.0.0.1:7878). */
  baseUrl?: string;
  /** API token, sent as X-OculOS-Token. Defaults to process.env.OCULOS_TOKEN. */
  token?: string;
  /** Per-request timeout in ms (default 30000). Waits/batches get more automatically. */
  timeoutMs?: number;
}
