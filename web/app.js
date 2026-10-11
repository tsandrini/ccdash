const TOKEN_KEY = "ccdash.token";
const THEME_KEY = "ccdash.theme";
const VIEW_KEY = "ccdash.view";
const WORKSPACE_KEY = "ccdash.workspace";
const HISTORY_RANGE_KEY = "ccdash.range.history";
const ANALYTICS_RANGE_KEY = "ccdash.range.analytics";
const STALE_STREAM_MS = 45_000;
const STRIP_MS = 60 * 60_000;
const PAGE = 10;

const ACTIVITY = {
  permission: { label: "Needs permission", rank: 0 },
  your_turn: { label: "Your turn", rank: 1 },
  working: { label: "Working", rank: 2 },
  idle: { label: "Idle", rank: 3 },
  gone: { label: "Ended", rank: 4 },
  finished: { label: "Finished", rank: 5 },
};
const STATES = ["working", "permission", "your_turn"];

const state = {
  host: "",
  skew: 0,
  sessions: new Map(),
  rateLimits: null,
  /** Last-hour history lanes by session id, for the card strips. */
  recent: new Map(),
  detailId: null,
  detailTab: "conversation",
  /** Loaded conversation of the open session. */
  convo: null,
  range: { kind: "days", days: 1 },
  history: null,
  aRange: { kind: "days", days: 7 },
  analytics: null,
  workspaces: [],
  workspace: localStorage.getItem(WORKSPACE_KEY),
  /** Session to open from `#session=…` once the snapshot has it. */
  pendingSession: null,
  /** Activity groups already opened automatically while a turn runs. */
  autoOpened: new Set(),
};

// --- DOM helpers ------------------------------------------------------------

function h(tag, attrs = {}, ...children) {
  const el = document.createElement(tag);
  setAttrs(el, attrs);
  append(el, children);
  return el;
}

const SVG_NS = "http://www.w3.org/2000/svg";

function s(tag, attrs = {}, ...children) {
  const el = document.createElementNS(SVG_NS, tag);
  setAttrs(el, attrs);
  append(el, children);
  return el;
}

function setAttrs(el, attrs) {
  for (const [key, value] of Object.entries(attrs)) {
    if (value === null || value === undefined || value === false) continue;
    if (key.startsWith("on")) el.addEventListener(key.slice(2), value);
    else if (key === "text") el.textContent = value;
    else el.setAttribute(key, value === true ? "" : value);
  }
}

function append(el, children) {
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false) continue;
    el.append(
      child instanceof Node ? child : document.createTextNode(String(child)),
    );
  }
}

const $ = (sel) => document.querySelector(sel);

const joined = (items) =>
  items
    .filter((item) => item !== null && item !== undefined && item !== false)
    .flatMap((item, i) =>
      i
        ? [h("span", { class: "sep", "aria-hidden": "true" }, "·"), item]
        : [item],
    );

// --- formatting -------------------------------------------------------------

const now = () => Date.now() + state.skew;

function fmtDuration(ms) {
  const secs = Math.max(0, Math.round(ms / 1000));
  if (secs < 60) return `${secs}s`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ${String(mins % 60).padStart(2, "0")}m`;
  return `${Math.floor(hours / 24)}d ${hours % 24}h`;
}

const timeFmt = new Intl.DateTimeFormat(undefined, {
  hour: "2-digit",
  minute: "2-digit",
});
const dayFmt = new Intl.DateTimeFormat(undefined, {
  weekday: "short",
  day: "numeric",
});
const fmtTime = (ts) => timeFmt.format(new Date(ts));
const fmtStamp = (ts) =>
  new Date(ts).toDateString() === new Date(now()).toDateString()
    ? fmtTime(ts)
    : `${dayFmt.format(ts)}, ${fmtTime(ts)}`;
const fmtUsd = (usd) =>
  usd >= 100 ? `$${usd.toFixed(0)}` : `$${usd.toFixed(2)}`;
const plural = (n, word, many = `${word}s`) => `${n} ${n === 1 ? word : many}`;

function fmtBytes(bytes) {
  const gb = bytes / 2 ** 30;
  if (gb >= 1) return `${gb.toFixed(gb >= 10 ? 0 : 1)} GB`;
  return `${Math.round(bytes / 2 ** 20)} MB`;
}

const compact = new Intl.NumberFormat(undefined, {
  notation: "compact",
  maximumFractionDigits: 1,
});

/** Byte points rescaled to whole MB or GB for a readable axis. */
function byteSeries(points) {
  const peak = Math.max(0, ...points.map(([, v]) => v));
  const [scale, unit] = peak >= 2 ** 30 ? [2 ** 30, "GB"] : [2 ** 20, "MB"];
  return {
    points: points.map(([t, v]) => [t, v / scale]),
    fmt: (v) => `${Number(v.toFixed(2))} ${unit}`,
    floorMax: unit === "GB" ? 1 : 256,
  };
}

function fmtHours(ms) {
  const hours = ms / 3_600_000;
  return hours >= 10 || hours === 0
    ? `${Math.round(hours)}h`
    : `${hours.toFixed(1)}h`;
}
const known = (v) => v !== null && v !== undefined;

function place(cwd) {
  const parts = (cwd || "").split("/").filter(Boolean);
  return parts.slice(-2).join("/") || cwd || "";
}

const label = (s) => s.title || s.name || s.id.slice(0, 8);
const isAgent = (s) => s.kind === "background";

/** Background sessions answer to their parent, so a done turn is "finished". */
const shown = (s) =>
  isAgent(s) && s.activity === "your_turn" ? "finished" : s.activity;

/** State color for bars: finished agents read as idle. */
const barActivity = (s) => (shown(s) === "finished" ? "idle" : s.activity);

const children = (s) =>
  [...state.sessions.values()].filter(
    (c) => c.parent === s.id && isAgent(c) && c.activity !== "gone",
  );

/** Short-lived sessions that never worked (the `--resume` picker). */
const stub = (s) =>
  s.activity === "gone" &&
  !s.prompt &&
  s.activity_since - s.started_at < 30_000;

/** Rough plain text of a markdown reply, for one-line previews. */
function plain(text) {
  return (text || "")
    .replace(/```[\s\S]*?(```|$)/g, " ")
    .replace(/^\s*\|.*$/gm, "")
    .replace(/!?\[([^\]]*)\]\([^)]*\)/g, "$1")
    .replace(/`([^`]*)`/g, "$1")
    .replace(/^\s{0,3}(#{1,6}\s+|>\s?|[-*+]\s+(\[[ x]\]\s+)?|\d+\.\s+)/gm, "")
    .replace(/(\*\*|__|~~)(\S(?:.*?\S)?)\1/g, "$2")
    .replace(/\s+/g, " ")
    .trim();
}

/** `mcp__plugin_hm_playwright__browser_click` → `playwright: browser_click`. */
function toolName(name) {
  const mcp = /^mcp__(.+?)__(.+)$/.exec(name || "");
  return mcp ? `${mcp[1].split("_").at(-1)}: ${mcp[2]}` : name;
}

const toolLabel = (tool) =>
  tool.summary
    ? `${toolName(tool.name)}: ${tool.summary}`
    : toolName(tool.name);

function meterLevel(pct) {
  if (pct >= 90) return "critical";
  if (pct >= 75) return "warning";
  return "normal";
}

// --- API --------------------------------------------------------------------

class Unauthorized extends Error {}

const token = () => localStorage.getItem(TOKEN_KEY) || "";

/** Reads `#token=…` and `#session=…` (opened once the session is known). */
function takeHash() {
  const token = location.hash.match(/token=([^&]+)/);
  const session = location.hash.match(/session=([^&]+)/);
  if (!token && !session) return;
  if (token) localStorage.setItem(TOKEN_KEY, decodeURIComponent(token[1]));
  if (session) state.pendingSession = decodeURIComponent(session[1]);
  history.replaceState(null, "", location.pathname + location.search);
  openPendingSession();
}

function openPendingSession() {
  const id = state.pendingSession;
  if (!id || !state.sessions.has(id)) return;
  state.pendingSession = null;
  openDetail(id);
}

async function api(path, init = {}) {
  const res = await fetch(`api/v1/${path}`, {
    ...init,
    headers: { Authorization: `Bearer ${token()}` },
  });
  if (res.status === 401) throw new Unauthorized();
  if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
  return res;
}

const getJson = async (path) => (await api(path)).json();

function askToken() {
  setConn("offline", "token needed");
  const dialog = $("#login");
  if (!dialog.open) dialog.showModal();
}

// --- live stream ------------------------------------------------------------

let streamAbort = null;
let streaming = false;
let lastStreamMessage = 0;

function setConn(kind, text = kind) {
  const el = $("#conn");
  el.dataset.state = kind;
  el.querySelector(".conn-label").textContent = text;
}

async function runStream() {
  if (streaming) return;
  streaming = true;
  try {
    await streamLoop();
  } finally {
    streaming = false;
  }
}

async function streamLoop() {
  let delay = 1000;
  for (;;) {
    streamAbort = new AbortController();
    try {
      setConn("connecting");
      const res = await api("stream", { signal: streamAbort.signal });
      setConn("live");
      delay = 1000;
      lastStreamMessage = Date.now();
      const reader = res.body.pipeThrough(new TextDecoderStream()).getReader();
      let buffer = "";
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        lastStreamMessage = Date.now();
        buffer += value;
        let end;
        while ((end = buffer.indexOf("\n\n")) >= 0) {
          handleSse(buffer.slice(0, end));
          buffer = buffer.slice(end + 2);
        }
      }
    } catch (e) {
      if (e instanceof Unauthorized) return askToken();
    }
    setConn("reconnecting");
    await new Promise((resolve) => setTimeout(resolve, delay));
    delay = Math.min(delay * 2, 30_000);
  }
}

function reconnectIfStale() {
  if (streamAbort && Date.now() - lastStreamMessage > STALE_STREAM_MS)
    streamAbort.abort();
}

function handleSse(raw) {
  let event = "message";
  let data = "";
  for (const line of raw.split("\n")) {
    if (line.startsWith("event:")) event = line.slice(6).trim();
    else if (line.startsWith("data:")) data += line.slice(5).trimStart();
  }
  if (event === "lagged") return void refreshSnapshot();
  if (!data) return;
  const payload = JSON.parse(data);
  if (event === "snapshot") applySnapshot(payload);
  else if (event === "update") applyUpdate(payload);
}

async function refreshSnapshot() {
  try {
    applySnapshot(await getJson("snapshot"));
  } catch (e) {
    if (e instanceof Unauthorized) askToken();
  }
}

function applySnapshot(snapshot) {
  state.host = snapshot.host;
  state.skew = snapshot.now - Date.now();
  state.sessions = new Map(snapshot.sessions.map((s) => [s.id, s]));
  state.rateLimits = snapshot.rate_limits;
  const known = state.workspaces.join();
  state.workspaces = snapshot.workspaces || [];
  if (state.workspaces.join() !== known) renderScopebar();
  scheduleRender();
  loadRecent();
  if (state.detailId) scheduleDetailRefresh();
  openPendingSession();
}

function applyUpdate(update) {
  if (update.type === "session") {
    const previous = state.sessions.get(update.session.id);
    state.sessions.set(update.session.id, update.session);
    if (
      state.detailId === update.session.id &&
      (!previous ||
        detailSignature(previous) !== detailSignature(update.session))
    ) {
      scheduleDetailRefresh();
    }
  } else if (update.type === "removed") {
    state.sessions.delete(update.id);
  } else if (update.type === "rate_limits") {
    state.rateLimits = update.rate_limits;
  }
  scheduleRender();
}

/** Changes that add or alter conversation items. */
const detailSignature = (s) =>
  [
    s.activity,
    s.prompt?.at,
    s.reply?.at,
    s.turn?.tools,
    s.turn?.ended_at,
    s.subagents.length,
    s.tasks.map((t) => t.status).join(),
  ].join("|");

// --- sessions view ----------------------------------------------------------

let renderQueued = false;

function scheduleRender() {
  if (renderQueued) return;
  renderQueued = true;
  requestAnimationFrame(() => {
    renderQueued = false;
    renderSessions();
    if (state.detailId) {
      renderDetailHead();
      if (state.detailTab === "details") renderDetailBody();
      else renderLive();
    }
  });
}

function byAttention(a, b) {
  return (
    ACTIVITY[a.activity].rank - ACTIVITY[b.activity].rank ||
    a.activity_since - b.activity_since
  );
}

function renderSessions() {
  $("#host").textContent = state.host;
  const all = [...state.sessions.values()]
    .filter((s) => !stub(s) && inWorkspace(s))
    .sort(byAttention);
  const mine = all.filter((s) => !isAgent(s));
  const agents = all.filter(isAgent);
  const needs = all.filter(
    (s) => s.activity === "permission" || shown(s) === "your_turn",
  );
  const working = mine.filter((s) => s.activity === "working");
  const idle = mine.filter((s) => s.activity === "idle");
  const running = agents.filter(
    (s) => s.activity !== "gone" && s.activity !== "permission",
  );
  const gone = all.filter((s) => s.activity === "gone");

  $("#hero-value").textContent = needs.length;
  $("#hero-label").textContent =
    needs.length === 1 ? "session needs you" : "sessions need you";
  $("#hero-sub").textContent = needs.length
    ? `longest wait ${fmtDuration(now() - needs.reduce((m, s) => Math.min(m, s.activity_since), Infinity))}`
    : all.length
      ? "nothing is waiting on you"
      : "";
  $("#hero-sub").className = "hero-sub muted";

  const busyAgents = agents.filter((s) => s.activity === "working").length;
  $("#tiles").replaceChildren(
    countTile("Working", working.length),
    countTile("Idle", idle.length),
    countTile(
      "Background agents",
      running.length,
      running.length ? `${busyAgents} working` : null,
    ),
    limitTile("5-hour limit", state.rateLimits?.five_hour),
    limitTile("7-day limit", state.rateLimits?.seven_day),
  );

  const groups = [];
  if (!all.length) {
    groups.push(
      h("div", {
        class: "empty",
        text: "No Claude Code sessions are running.",
      }),
    );
  } else {
    groups.push(
      h(
        "div",
        { class: "legend strip-legend" },
        h("span", { class: "muted", text: "Bars show the last hour:" }),
        [...STATES, "idle"].map(legendChip),
      ),
    );
  }
  if (needs.length) groups.push(group("Needs you", needs));
  if (working.length) groups.push(group("Working", working));
  if (idle.length) groups.push(group("Idle", idle));
  if (running.length) {
    groups.push(
      group(
        "Background agents",
        running,
        false,
        "Started by Claude or with claude --bg; they report to the session that started them.",
      ),
    );
  }
  if (gone.length) groups.push(group("Ended", gone, true));
  $("#groups").replaceChildren(...groups);
}

const legendChip = (activity) =>
  h(
    "span",
    { "data-activity": activity },
    h("i", { "aria-hidden": "true" }),
    ACTIVITY[activity].label,
  );

function countTile(name, value, sub = null) {
  return h(
    "div",
    { class: "tile" },
    h("div", { class: "tile-label", text: name }),
    h("div", { class: "tile-value", text: value }),
    sub ? h("div", { class: "tile-sub", text: sub }) : null,
  );
}

function limitTile(name, limit) {
  if (!limit) {
    return h(
      "div",
      { class: "tile" },
      h("div", { class: "tile-label", text: name }),
      h("div", { class: "tile-value muted", text: "–" }),
      h("div", { class: "tile-sub", text: "no statusline data yet" }),
    );
  }
  const pct = Math.round(limit.used_pct);
  const level = meterLevel(pct);
  const resetsIn = limit.resets_at * 1000 - now();
  return h(
    "div",
    { class: "tile" },
    h("div", { class: "tile-label", text: name }),
    h("div", {
      class: "tile-value",
      text: `${pct}%${level === "normal" ? "" : " ⚠︎"}`,
    }),
    h(
      "div",
      {
        class: "meter",
        "data-level": level,
        role: "meter",
        "aria-valuenow": pct,
        "aria-valuemin": 0,
        "aria-valuemax": 100,
        "aria-label": name,
      },
      meterFill(pct),
    ),
    h("div", {
      class: "tile-sub",
      text: resetsIn > 0 ? `resets in ${fmtDuration(resetsIn)}` : "",
    }),
  );
}

function meterFill(pct) {
  const fill = h("span");
  fill.style.width = `${Math.min(100, Math.max(0, pct))}%`;
  return fill;
}

function group(title, sessions, collapsed = false, note = null) {
  const cards = h(
    "div",
    { class: "cards" },
    note ? h("p", { class: "group-note muted", text: note }) : null,
    sessions.map(card),
  );
  if (collapsed) {
    return h(
      "details",
      { class: "group" },
      h(
        "summary",
        {},
        title,
        h("span", { class: "muted", text: sessions.length }),
      ),
      cards,
    );
  }
  return h(
    "section",
    { class: "group" },
    h("h2", {}, title, h("span", { class: "muted", text: sessions.length })),
    cards,
  );
}

function badge(activity) {
  return h(
    "span",
    { class: "badge", "data-activity": activity },
    h("span", { class: "dot", "aria-hidden": "true" }),
    ACTIVITY[activity].label,
  );
}

const clock = (since) =>
  h("span", {
    class: "since",
    "data-since": since,
    text: fmtDuration(now() - since),
  });

function card(s) {
  const where = [place(s.cwd), s.model, isAgent(s) ? `bg ${s.short_id}` : null]
    .filter(Boolean)
    .join(" · ");
  return h(
    "button",
    {
      class: isAgent(s) ? "session session-agent" : "session",
      type: "button",
      "data-activity": shown(s),
      onclick: () => openDetail(s.id),
    },
    h(
      "span",
      { class: "session-top" },
      badge(shown(s)),
      clock(s.activity_since),
    ),
    h("span", { class: "session-title", text: label(s) }),
    h("span", { class: "session-where", title: s.cwd, text: where }),
    isAgent(s)
      ? h("span", { class: "session-parent", text: parentText(s) })
      : null,
    s.hooked || s.activity === "gone"
      ? [focusLines(s), taskBar(s.tasks), cardProgress(s), cardMeta(s)]
      : h("span", {
          class: "session-note",
          text: "Started before ccdash was installed, so only busy and idle are known. Resume it to see details.",
        }),
    strip(s),
  );
}

/** What matters most for the session's state: the pending request, or the
 * running tool and what Claude last said. */
function focusLines(s) {
  if (s.waiting_for)
    return h("span", { class: "session-focus mono", text: s.waiting_for });
  const lines = [];
  const working = s.activity === "working";
  if (working && s.tool) {
    lines.push(
      h(
        "span",
        { class: "session-tool" },
        h("span", { class: "mono", text: toolLabel(s.tool) }),
        clock(s.tool.started_at),
      ),
    );
  }
  const [who, text] =
    working && s.message
      ? ["Claude", s.message.text]
      : !working && s.reply
        ? ["Claude", s.reply.text]
        : ["You", s.prompt?.text];
  const preview = plain(text);
  if (preview) {
    lines.push(
      h(
        "span",
        { class: "session-detail" },
        h("span", { class: "who", text: who }),
        " ",
        preview,
      ),
    );
  }
  return lines;
}

function taskBar(tasks) {
  if (!tasks?.length) return null;
  const done = tasks.filter((t) => t.status === "completed").length;
  const active = tasks.find((t) => t.status === "in_progress");
  return h(
    "span",
    { class: "session-tasks" },
    h(
      "span",
      { class: "meter", "data-level": "neutral" },
      meterFill((100 * done) / tasks.length),
    ),
    h("span", { class: "tasks-count", text: `${done}/${tasks.length}` }),
    active
      ? h("span", {
          class: "tasks-active",
          text: active.active_form || active.subject,
        })
      : null,
  );
}

function turnFacts(s) {
  const t = s.turn;
  if (!t) return [];
  const running =
    !t.ended_at && (s.activity === "working" || s.activity === "permission");
  return [
    running
      ? h("span", {}, "turn ", clock(t.started_at))
      : t.ended_at
        ? `turn took ${fmtDuration(t.ended_at - t.started_at)}`
        : null,
    t.tools ? plural(t.tools, "tool") : null,
    t.failures ? `${t.failures} failed` : null,
    t.files.length ? plural(t.files.length, "file") : null,
  ];
}

function parentText(s) {
  if (!s.parent) return "Background session";
  const parent = state.sessions.get(s.parent);
  return `Started by ${parent ? label(parent) : `session ${s.parent.slice(0, 8)}`}`;
}

function cardProgress(s) {
  const agents = children(s).length;
  const items = [
    ...(s.activity === "gone" ? [] : turnFacts(s)),
    s.subagents.length ? plural(s.subagents.length, "subagent") : null,
    agents ? plural(agents, "background agent") : null,
  ].filter(Boolean);
  return items.length
    ? h("span", { class: "session-meta" }, joined(items))
    : null;
}

function cardMeta(s) {
  const items = [
    known(s.context_pct)
      ? h(
          "span",
          { class: "meta-item" },
          h(
            "span",
            { class: "meter", "data-level": meterLevel(s.context_pct) },
            meterFill(s.context_pct),
          ),
          `context ${Math.round(s.context_pct)}%`,
        )
      : null,
    s.cost_usd ? fmtUsd(s.cost_usd) : null,
    s.resources
      ? h("span", {
          title: plural(s.resources.procs, "process", "processes"),
          text: `${s.resources.cpu_pct}% CPU`,
        })
      : null,
    s.resources ? fmtBytes(s.resources.mem_bytes) : null,
  ].filter(Boolean);
  return items.length
    ? h("span", { class: "session-meta" }, joined(items))
    : null;
}

/** Last-hour activity bar; the newest stretch comes from the live state. */
function strip(s) {
  const t1 = now();
  const t0 = t1 - STRIP_MS;
  const segs = (state.recent.get(s.id)?.segments || []).map((seg) => [...seg]);
  if (s.activity !== "gone") {
    const from = Math.max(s.activity_since, t0);
    while (segs.length && segs.at(-1)[0] >= from) segs.pop();
    const last = segs.at(-1);
    if (last) last[1] = Math.min(last[1], from);
    if (last && last[2] === barActivity(s)) last[1] = t1;
    else segs.push([from, t1, barActivity(s)]);
  }
  const bar = h("span", { class: "strip", "aria-hidden": "true" });
  for (const [a, b, activity] of segs) {
    if (b <= t0 || activity === "gone") continue;
    const left = ((Math.max(a, t0) - t0) / STRIP_MS) * 100;
    const width = ((Math.min(b, t1) - Math.max(a, t0)) / STRIP_MS) * 100;
    const seg = h("i", { "data-activity": activity });
    seg.style.left = `${left}%`;
    seg.style.width = `calc(${width}% - 2px)`;
    bar.append(seg);
  }
  return bar;
}

async function loadRecent() {
  if ($("#view-sessions").hidden || !token()) return;
  try {
    const data = await getJson(`history?since=${Math.round(now() - STRIP_MS)}`);
    state.recent = new Map(data.lanes.map((lane) => [lane.id, lane]));
    scheduleRender();
  } catch (e) {
    if (e instanceof Unauthorized) askToken();
  }
}

function tickClocks() {
  for (const el of document.querySelectorAll("[data-since]")) {
    el.textContent = fmtDuration(now() - Number(el.dataset.since));
  }
}

// --- session sheet ----------------------------------------------------------

let detailTimer = null;

/** Refetches the newest exchanges at most once a second. */
function scheduleDetailRefresh() {
  if (detailTimer) return;
  detailTimer = setTimeout(() => {
    detailTimer = null;
    if (state.detailId) loadConversation("refresh");
  }, 1000);
}

function openDetail(id) {
  state.detailId = id;
  state.convo = {
    id,
    exchanges: [],
    more: false,
    stats: null,
    resources: null,
    loaded: false,
  };
  state.autoOpened = new Set();
  const dialog = $("#detail");
  dialog.replaceChildren(
    h("div", { class: "detail-head" }),
    h("div", { class: "detail-body" }),
  );
  renderDetailHead();
  renderDetailBody();
  if (!dialog.open) dialog.showModal();
  loadConversation("initial");
}

function setDetailTab(tab) {
  state.detailTab = tab;
  renderDetailHead();
  renderDetailBody();
  if (tab === "conversation") scrollToEnd();
  else $("#detail").scrollTop = 0;
}

function renderDetailHead() {
  const s = state.sessions.get(state.detailId);
  const head = $("#detail .detail-head");
  if (!s || !head) return;
  const actions = [];
  if (s.remote_url) {
    actions.push(
      h("a", {
        class: "btn",
        href: s.remote_url,
        target: "_blank",
        rel: "noopener noreferrer",
        text: "Open in Claude",
      }),
    );
  }
  head.replaceChildren(
    h("button", {
      class: "icon-btn detail-close",
      type: "button",
      "aria-label": "Close",
      text: "✕",
      onclick: () => $("#detail").close(),
    }),
    h(
      "div",
      { class: "session-top" },
      badge(shown(s)),
      clock(s.activity_since),
    ),
    h("h2", { id: "detail-title", text: label(s) }),
    h("div", { class: "session-where", title: s.cwd, text: s.cwd }),
    isAgent(s) ? parentLink(s) : null,
    h(
      "div",
      { class: "detail-bar" },
      h(
        "div",
        { class: "filters", role: "tablist", "aria-label": "Session view" },
        [
          ["conversation", "Conversation"],
          ["details", "Details"],
        ].map(([tab, text]) =>
          h("button", {
            type: "button",
            role: "tab",
            "aria-selected": String(state.detailTab === tab),
            text,
            onclick: () => setDetailTab(tab),
          }),
        ),
      ),
      actions,
    ),
  );
}

async function loadConversation(mode) {
  const c = state.convo;
  if (!c) return;
  const id = c.id;
  const query =
    mode === "older"
      ? `?before=${c.exchanges[0].at}&limit=${PAGE}`
      : `?limit=${mode === "refresh" ? 4 : PAGE}`;
  let page;
  try {
    page = await getJson(`sessions/${encodeURIComponent(id)}${query}`);
  } catch (e) {
    if (e instanceof Unauthorized) return askToken();
    return;
  }
  if (state.convo !== c) return;
  const conv = page.conversation;
  const dialog = $("#detail");
  const atEnd =
    dialog.scrollHeight - dialog.scrollTop - dialog.clientHeight < 80;
  const heightBefore = dialog.scrollHeight;

  if (mode === "older") {
    c.exchanges = [...conv.exchanges, ...c.exchanges];
    c.more = conv.more;
  } else if (mode === "initial" || !conv.more || !c.exchanges.length) {
    c.exchanges = conv.exchanges;
    c.more = conv.more;
  } else {
    const cut = conv.exchanges[0].at;
    const kept = c.exchanges.filter((e) => e.at < cut);
    if (kept.length === c.exchanges.length && kept.length)
      return void loadConversation("initial");
    c.exchanges = [...kept, ...conv.exchanges];
  }
  if (page.stats) c.stats = page.stats;
  if (page.resources) c.resources = page.resources;
  c.loaded = true;
  renderDetailBody();

  if (state.detailTab !== "conversation") return;
  if (mode === "older") dialog.scrollTop += dialog.scrollHeight - heightBefore;
  else if (mode === "initial" || atEnd) scrollToEnd();
}

function scrollToEnd() {
  const dialog = $("#detail");
  dialog.scrollTop = dialog.scrollHeight;
}

function renderDetailBody() {
  const body = $("#detail .detail-body");
  const s = state.sessions.get(state.detailId);
  if (!body || !s) return;
  const openKeys = new Set(
    [...body.querySelectorAll("details[data-key][open]")].map(
      (d) => d.dataset.key,
    ),
  );
  if (state.detailTab === "details") {
    body.replaceChildren(...detailsTab(s, state.convo?.stats));
  } else {
    body.replaceChildren(...conversationTab(s));
    clampReplies(body);
    openRunningSteps(body, s);
  }
  for (const d of body.querySelectorAll("details[data-key]")) {
    if (openKeys.has(d.dataset.key)) d.open = true;
  }
}

/** Unfolds the newest tool group once while the turn is still running. */
function openRunningSteps(body, s) {
  if (s.activity !== "working" && s.activity !== "permission") return;
  const group = [...body.querySelectorAll(".exchange")]
    .at(-1)
    ?.querySelectorAll("details.activity");
  const last = group?.[group.length - 1];
  if (!last || state.autoOpened.has(last.dataset.key)) return;
  state.autoOpened.add(last.dataset.key);
  last.open = true;
}

function parentLink(s) {
  const parent = s.parent && state.sessions.get(s.parent);
  if (!parent)
    return h("div", { class: "session-parent", text: parentText(s) });
  return h(
    "div",
    { class: "session-parent" },
    "Started by ",
    h("button", {
      class: "link-btn",
      type: "button",
      text: label(parent),
      onclick: () => openDetail(parent.id),
    }),
  );
}

function agentList(list) {
  return h(
    "ul",
    { class: "agents" },
    list.map((c) =>
      h(
        "li",
        {},
        h("button", {
          class: "link-btn",
          type: "button",
          text: label(c),
          onclick: () => openDetail(c.id),
        }),
        badge(shown(c)),
        clock(c.activity_since),
      ),
    ),
  );
}

function unhookedNote(s) {
  return h(
    "div",
    { class: "callout" },
    h("strong", { text: "No hook data for this session." }),
    " It started before ccdash was installed, so only busy and idle are known. Exit it and run ",
    h("code", { text: `claude --resume ${s.id}` }),
    " in the same directory to see its conversation and progress.",
  );
}

// --- conversation tab -------------------------------------------------------

function conversationTab(s) {
  const c = state.convo;
  const out = [];
  if (!s.hooked && s.activity !== "gone") out.push(unhookedNote(s));
  if (c.more) {
    out.push(
      h("button", {
        class: "btn btn-quiet load-older",
        type: "button",
        text: "Show earlier messages",
        onclick: (event) => {
          event.currentTarget.disabled = true;
          loadConversation("older");
        },
      }),
    );
  }
  if (c.loaded && !c.exchanges.length && s.hooked) {
    out.push(h("p", { class: "muted center", text: "No messages yet." }));
  }
  const lastReply = lastReplyAt(c.exchanges);
  for (const ex of c.exchanges) out.push(exchange(ex, lastReply));
  out.push(h("div", { id: "live" }, liveBlock(s)));
  return out;
}

function lastReplyAt(exchanges) {
  for (let i = exchanges.length - 1; i >= 0; i--) {
    const reply = exchanges[i].items.findLast((it) => it.kind === "reply");
    if (reply) return reply.at;
  }
  return null;
}

const GROUPED = new Set(["tool", "permission", "task"]);

function exchange(ex, lastReply) {
  const out = [];
  if (ex.prompt !== null) {
    out.push(
      h(
        "div",
        { class: "msg msg-user" },
        h("div", { class: "bubble clamp", text: ex.prompt }),
        h("time", { text: fmtStamp(ex.at) }),
      ),
    );
  }
  let run = [];
  let omitted = ex.omitted;
  const flush = () => {
    if (!run.length) return;
    out.push(activityGroup(run, omitted));
    run = [];
    omitted = 0;
  };
  for (const item of ex.items) {
    if (GROUPED.has(item.kind)) {
      run.push(item);
      continue;
    }
    flush();
    out.push(convItem(item, item.at === lastReply));
  }
  flush();
  return h("section", { class: "exchange" }, out);
}

function activityGroup(items, omitted) {
  const tools = items.filter((i) => i.kind === "tool");
  const failed = tools.filter((t) => !t.ok).length;
  const asks = items.filter((i) => i.kind === "permission").length;
  const tasks = items.filter((i) => i.kind === "task").length;
  const span = items.at(-1).at - items[0].at;
  const counts = new Map();
  for (const t of tools) counts.set(t.name, (counts.get(t.name) || 0) + 1);
  const top = [...counts]
    .sort((a, b) => b[1] - a[1])
    .slice(0, 3)
    .map(([name, n]) => `${toolName(name)}${n > 1 ? ` ×${n}` : ""}`)
    .join(", ");
  const summary = [
    tools.length + omitted ? plural(tools.length + omitted, "tool call") : null,
    failed ? `${failed} failed` : null,
    asks ? plural(asks, "permission request") : null,
    tasks ? plural(tasks, "task update") : null,
    span >= 1000 ? fmtDuration(span) : null,
  ].filter(Boolean);
  return h(
    "details",
    { class: "activity", "data-key": `g${items[0].at}` },
    h(
      "summary",
      {},
      h("span", { class: "activity-sum" }, joined(summary)),
      top ? h("span", { class: "activity-top", text: top }) : null,
    ),
    stepList(items, omitted),
  );
}

const VISIBLE_STEPS = 15;

/** The newest steps, with a button for the earlier ones in long runs. */
function stepList(items, omitted) {
  const list = h(
    "ol",
    { class: "steps" },
    omitted
      ? h("li", {
          class: "muted",
          text: `${plural(omitted, "earlier tool call")} not shown`,
        })
      : null,
  );
  const hidden = items.length - VISIBLE_STEPS;
  if (hidden > 0) {
    list.append(
      h(
        "li",
        { class: "steps-more" },
        h("button", {
          class: "show-more",
          type: "button",
          text: `Show ${plural(hidden, "earlier step")}`,
          onclick: (event) =>
            event.currentTarget.parentElement.replaceWith(
              ...items.slice(0, hidden).map(step),
            ),
        }),
      ),
    );
  }
  list.append(...items.slice(Math.max(0, hidden)).map(step));
  return list;
}

function step(item) {
  const time = h("time", { text: fmtTime(item.at) });
  if (item.kind === "permission") {
    return h(
      "li",
      { class: "step-ask" },
      time,
      h("span", { class: "step-icon", text: "?" }),
      h("span", {
        class: "step-text",
        text: `Asked permission: ${toolName(item.tool)}${item.summary ? ` ${item.summary}` : ""}`,
      }),
    );
  }
  if (item.kind === "task") {
    const verb = {
      pending: "added",
      in_progress: "started",
      completed: "completed",
      deleted: "removed",
    }[item.status];
    return h(
      "li",
      {},
      time,
      h("span", {
        class: "step-icon",
        text: item.status === "completed" ? "✓" : "☐",
      }),
      h("span", { class: "step-text", text: `Task ${verb}: ${item.subject}` }),
    );
  }
  return h(
    "li",
    { class: item.ok ? null : "step-failed" },
    time,
    h("span", {
      class: "step-icon",
      text: item.ok ? "" : "✗",
      title: item.ok ? null : "failed",
    }),
    h(
      "span",
      { class: "step-text" },
      h("strong", { text: toolName(item.name) }),
      item.summary
        ? h("span", { class: "mono", text: ` ${item.summary}` })
        : null,
    ),
    known(item.ms) && item.ms >= 1000
      ? h("span", { class: "step-ms", text: fmtDuration(item.ms) })
      : null,
  );
}

function convItem(item, isLastReply) {
  switch (item.kind) {
    case "message":
      return h(
        "div",
        { class: "msg msg-claude msg-step" },
        prose(item.html, true),
      );
    case "reply":
      return h(
        "div",
        { class: "msg msg-claude" },
        prose(item.html, !isLastReply),
        h("time", { text: fmtStamp(item.at) }),
      );
    case "error":
      return h(
        "div",
        { class: "conv-note conv-error" },
        h("strong", { text: "Turn failed: " }),
        item.error,
      );
    case "subagent":
      return h(
        "details",
        { class: "subagent", "data-key": `a${item.at}` },
        h(
          "summary",
          {},
          h("span", {
            class: "subagent-kind",
            text: item.agent_type || "Subagent",
          }),
          h("span", { text: item.description || "finished" }),
          h(
            "span",
            { class: "muted" },
            joined(
              [
                known(item.tools) ? plural(item.tools, "tool call") : null,
                known(item.ms) ? fmtDuration(item.ms) : null,
              ].filter(Boolean),
            ),
          ),
        ),
        item.html
          ? prose(item.html)
          : h("p", { class: "muted", text: "No final message." }),
      );
    case "compact":
      return h(
        "details",
        { class: "conv-note compact", "data-key": `c${item.at}` },
        h("summary", {
          text: `Context compacted${item.trigger ? ` (${item.trigger})` : ""} · ${fmtTime(item.at)}`,
        }),
        item.html ? prose(item.html) : null,
      );
    case "start":
      if (item.source === "compact") return null;
      return divider(
        {
          startup: "Session started",
          resume: "Session resumed",
          clear: "Conversation cleared",
        }[item.source] || `Session started (${item.source})`,
        item.at,
      );
    case "end":
      return divider(`Session ended (${item.reason})`, item.at);
    default:
      return null;
  }
}

const divider = (text, at) =>
  h(
    "div",
    { class: "divider" },
    h("span", { text: `${text} · ${fmtStamp(at)}` }),
  );

function prose(html, clamp = false) {
  const el = h("div", { class: clamp ? "prose clamp" : "prose" });
  // NOTE: server-rendered markdown with raw HTML escaped (src/markdown.rs)
  el.innerHTML = html;
  for (const a of el.querySelectorAll("a")) {
    a.target = "_blank";
    a.rel = "noopener noreferrer";
  }
  return el;
}

function clampReplies(root) {
  for (const el of root.querySelectorAll(".clamp")) {
    if (el.scrollHeight <= el.clientHeight + 8) {
      el.classList.remove("clamp");
      continue;
    }
    el.after(
      h("button", {
        class: "show-more",
        type: "button",
        text: "Show all",
        onclick: (event) => {
          el.classList.remove("clamp");
          event.currentTarget.remove();
        },
      }),
    );
  }
}

/** What the session is doing right now, below the newest exchange. */
function liveBlock(s) {
  if (!s || s.activity === "gone" || !s.hooked) return null;
  const rows = [];
  if (s.activity === "permission") {
    rows.push(
      h(
        "div",
        { class: "live-row" },
        badge("permission"),
        h("span", { class: "mono", text: s.waiting_for || "" }),
      ),
    );
  } else if (s.activity === "working") {
    rows.push(
      h(
        "div",
        { class: "live-row" },
        badge("working"),
        s.tool
          ? [
              h("span", { class: "mono", text: toolLabel(s.tool) }),
              clock(s.tool.started_at),
            ]
          : h("span", { class: "muted", text: "thinking" }),
      ),
    );
  }
  const said = liveMessage(s);
  if (said) {
    rows.push(
      h(
        "div",
        { class: "live-said" },
        h("span", { class: "who", text: "Claude" }),
        " ",
        said,
      ),
    );
  }
  const facts =
    s.activity === "working" || s.activity === "permission"
      ? turnFacts(s).filter(Boolean)
      : [];
  if (facts.length) rows.push(h("div", { class: "live-facts" }, joined(facts)));
  if (s.subagents.length) rows.push(subagentList(s.subagents));
  if (s.tasks.length) {
    const done = s.tasks.filter((t) => t.status === "completed").length;
    rows.push(
      h(
        "details",
        { class: "live-tasks", "data-key": "tasks" },
        h("summary", {}, taskBar(s.tasks) || `${done}/${s.tasks.length} tasks`),
        taskList(s.tasks),
      ),
    );
  }
  return rows.length
    ? h("div", { class: "live", "data-activity": s.activity }, rows)
    : null;
}

/** Claude's streaming text, until the finished message is in the loaded conversation. */
function liveMessage(s) {
  if (!s.message || s.activity !== "working") return null;
  const shown = (state.convo?.exchanges || [])
    .at(-1)
    ?.items.some((it) => it.kind === "message" && it.at >= s.message.at);
  return shown ? null : s.message.text;
}

function renderLive() {
  const live = $("#live");
  const s = state.sessions.get(state.detailId);
  if (!live || !s) return;
  const open = live.querySelector("details[open]") !== null;
  live.replaceChildren(...[liveBlock(s)].filter(Boolean));
  const details = live.querySelector("details");
  if (details && open) details.open = true;
}

function subagentList(subagents) {
  return h(
    "ul",
    { class: "subagents" },
    subagents.map((a) =>
      h(
        "li",
        {},
        h("span", {
          class: "dot",
          "data-activity": "working",
          "aria-hidden": "true",
        }),
        h("span", { class: "subagent-kind", text: a.agent_type || "Subagent" }),
        h("span", { text: a.description || "" }),
        h(
          "span",
          { class: "muted" },
          joined(
            [
              a.tool
                ? h("span", { class: "mono", text: toolLabel(a.tool) })
                : null,
              a.tools ? plural(a.tools, "tool") : null,
              clock(a.started_at),
            ].filter(Boolean),
          ),
        ),
      ),
    ),
  );
}

function taskList(tasks) {
  const icon = { completed: "✓", in_progress: "◐", pending: "○" };
  return h(
    "ul",
    { class: "tasks" },
    tasks.map((t) =>
      h(
        "li",
        { "data-status": t.status },
        h("span", {
          class: "task-icon",
          "aria-hidden": "true",
          text: icon[t.status],
        }),
        h("span", {
          text:
            t.status === "in_progress" && t.active_form
              ? t.active_form
              : t.subject,
        }),
        h("span", {
          class: "visually-hidden",
          text: ` (${t.status.replace("_", " ")})`,
        }),
      ),
    ),
  );
}

// --- details tab ------------------------------------------------------------

function detailsTab(s, stats) {
  const out = [];
  if (!s.hooked && s.activity !== "gone") out.push(unhookedNote(s));
  if (s.waiting_for) {
    out.push(
      h("h3", { text: "Waiting for" }),
      h("p", { class: "mono", text: s.waiting_for }),
    );
  }
  if (s.turn) {
    const t = s.turn;
    out.push(
      h("h3", { text: t.ended_at ? "Last turn" : "Current turn" }),
      factList([
        ["Started", fmtStamp(t.started_at)],
        [
          "Duration",
          t.ended_at
            ? fmtDuration(t.ended_at - t.started_at)
            : `${fmtDuration(now() - t.started_at)} so far`,
        ],
        [
          "Tool calls",
          `${t.tools}${t.failures ? ` (${t.failures} failed)` : ""}`,
        ],
        ["Subagent tool calls", t.subagent_tools || null],
      ]),
    );
    if (t.files.length) {
      out.push(
        h("h3", { text: `Files edited (${t.files.length})` }),
        h(
          "ul",
          { class: "files" },
          t.files.map((f) => h("li", { class: "mono", text: f })),
        ),
      );
    }
  }
  const agents = children(s);
  if (agents.length) {
    out.push(
      h("h3", { text: `Background agents (${agents.length})` }),
      agentList(agents),
    );
  }
  if (s.subagents.length) {
    out.push(
      h("h3", { text: `Running subagents (${s.subagents.length})` }),
      subagentList(s.subagents),
    );
  }
  if (s.tasks.length) {
    const done = s.tasks.filter((t) => t.status === "completed").length;
    out.push(
      h("h3", { text: `Tasks (${done}/${s.tasks.length} done)` }),
      taskList(s.tasks),
    );
  }
  out.push(...processSection(s, state.convo?.resources));
  if (stats) {
    out.push(
      h("h3", { text: "Session totals" }),
      factList([
        ["Prompts", stats.prompts],
        [
          "Tool calls",
          `${stats.tools}${stats.failures ? ` (${stats.failures} failed)` : ""}`,
        ],
        ["Files edited", stats.files],
        ["Subagents run", stats.subagents || null],
        ["Compactions", stats.compactions || null],
        ["Working", fmtDuration(stats.working_ms)],
        [
          "Waited on you",
          fmtDuration(stats.permission_ms + stats.your_turn_ms),
        ],
      ]),
    );
  }
  out.push(h("h3", { text: "Session" }), facts(s));
  return out;
}

function processSection(s, history) {
  const r = s.resources;
  if (!r) return [];
  const out = [
    h("h3", { text: "Processes" }),
    factList([
      ["CPU", `${r.cpu_pct}% of one core`],
      ["Memory", fmtBytes(r.mem_bytes)],
      ["Processes", r.procs],
    ]),
    h(
      "table",
      { class: "procs" },
      h(
        "thead",
        {},
        h(
          "tr",
          {},
          ["Process", "CPU", "Memory", "Running"].map((t) =>
            h("th", { text: t }),
          ),
        ),
      ),
      h(
        "tbody",
        {},
        r.top.map((p) =>
          h(
            "tr",
            {},
            h(
              "td",
              {},
              h("strong", { text: p.name }),
              p.cmd ? h("div", { class: "mono muted", text: p.cmd }) : null,
            ),
            h("td", { text: `${p.cpu_pct}%` }),
            h("td", { text: fmtBytes(p.mem_bytes) }),
            h("td", { text: fmtDuration(now() - p.started_at) }),
          ),
        ),
      ),
    ),
  ];
  if (history?.length > 1) {
    const width = Math.max(
      260,
      ($("#detail .detail-body")?.clientWidth || 600) - 40,
    );
    const until = now();
    const since = until - 3_600_000;
    out.push(
      h(
        "div",
        { class: "chart-pair" },
        lineChart({
          title: "CPU, last hour",
          sub: "Average per minute, % of one core",
          points: history.map(([ts, cpu]) => [ts, cpu]),
          since,
          until,
          width: width > 560 ? (width - 12) / 2 : width,
          fmt: (v) => `${Math.round(v)}%`,
          floorMax: 100,
        }),
        lineChart({
          title: "Memory, last hour",
          sub: "Peak per minute (PSS)",
          since,
          until,
          width: width > 560 ? (width - 12) / 2 : width,
          ...byteSeries(history.map(([ts, , rss]) => [ts, rss])),
        }),
      ),
    );
  }
  return out;
}

function factList(rows) {
  return h(
    "dl",
    { class: "facts" },
    rows
      .filter(([, value]) => known(value) && value !== "")
      .flatMap(([k, v]) => [h("dt", { text: k }), h("dd", { text: v })]),
  );
}

function facts(s) {
  return factList([
    ["Model", s.model],
    ["Effort", s.effort],
    ["Permission mode", s.permission_mode],
    ["Context", known(s.context_pct) ? `${Math.round(s.context_pct)}%` : null],
    ["Cost", s.cost_usd ? `${fmtUsd(s.cost_usd)} (API-equivalent)` : null],
    [
      "Lines",
      known(s.lines_added)
        ? `+${s.lines_added} −${s.lines_removed ?? 0}`
        : null,
    ],
    [
      "Kind",
      s.kind === "background" ? `background (${s.short_id})` : "interactive",
    ],
    ["Window", s.window ? `niri #${s.window.id}` : null],
    [
      "Started",
      `${fmtStamp(s.started_at)} (${fmtDuration(now() - s.started_at)} ago)`,
    ],
    ["Directory", s.cwd],
    ["Host", s.host],
    ["Session id", s.id],
  ]);
}

// --- time ranges ------------------------------------------------------------
// After BITE's date-time range picker: a range is the last N minutes, the
// last N calendar days (from local midnight), or a fixed from–to; ‹ › move it
// by its own length, ⊖ doubles it, the form takes quick ranges or exact times.

const MINUTE = 60_000;
/** Retention is 30 days; the API serves 31. */
const MAX_RANGE_MINUTES = 31 * 1440;

function resolveRange(range, t = now()) {
  if (range.kind === "relative")
    return { from: t - range.minutes * MINUTE, to: t };
  if (range.kind === "days") {
    const start = new Date(t);
    start.setHours(0, 0, 0, 0);
    start.setDate(start.getDate() - range.days + 1);
    return { from: start.getTime(), to: t };
  }
  return { from: range.from, to: range.to };
}

/** New data can still arrive in the range. */
const reachesNow = (range, t = now()) =>
  range.kind !== "absolute" || range.to >= t - MINUTE;

/** Moves a range by its own length; forward never passes now (null then). */
function shiftRange(range, direction, t = now()) {
  const { from, to } = resolveRange(range, t);
  const span = to - from;
  if (direction === 1) {
    if (to >= t - MINUTE) return null;
    const end = Math.min(t, to + span);
    return { kind: "absolute", from: end - span, to: end };
  }
  return { kind: "absolute", from: from - span, to: to - span };
}

/** Twice the span around the same middle, never past now or retention. */
function zoomOut(range, t = now()) {
  const { from, to } = resolveRange(range, t);
  const span = Math.min((to - from) * 2, MAX_RANGE_MINUTES * MINUTE);
  const end = Math.min(t, (from + to) / 2 + span / 2);
  return { kind: "absolute", from: end - span, to: end };
}

const pad2 = (n) => String(n).padStart(2, "0");

/** A moment as `<input type="datetime-local">` text, local time. */
function toInputValue(time) {
  const d = new Date(time);
  return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}T${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
}

/** The moment a `datetime-local` value names; null when it names none. */
function fromInputValue(text) {
  const m = /^(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2})$/.exec(text.trim());
  if (!m) return null;
  const [year, month, day, hour, minute] = m.slice(1).map(Number);
  const time = new Date(year, month - 1, day, hour, minute).getTime();
  // NOTE: Date rolls 31 February over instead of refusing it
  return toInputValue(time).startsWith(`${m[1]}-${m[2]}-${m[3]}`) ? time : null;
}

function rangeProblem(from, to, t = now()) {
  if (from === null || to === null) return "incomplete";
  if (from >= to) return "backwards";
  if (from > t) return "future";
  if (from < t - MAX_RANGE_MINUTES * MINUTE) return "too_old";
  return null;
}

const PROBLEM_TEXT = {
  incomplete: "Fill in both times.",
  backwards: "The start has to be before the end.",
  future: "The range starts in the future.",
  too_old: "ccdash keeps 30 days of history.",
};

function spanText(from, to) {
  const start = toInputValue(from).replace("T", " ");
  const end = toInputValue(to).replace("T", " ");
  return start.slice(0, 10) === end.slice(0, 10)
    ? `${start} – ${end.slice(11)}`
    : `${start} – ${end}`;
}

function quickLabel(range) {
  if (range.kind === "days")
    return range.days === 1 ? "Today" : `Last ${range.days} days`;
  const m = range.minutes;
  if (m % 1440 === 0)
    return m === 1440 ? "Last 24 hours" : `Last ${m / 1440} days`;
  if (m % 60 === 0) return m === 60 ? "Last hour" : `Last ${m / 60} hours`;
  return `Last ${m} minutes`;
}

const rangeLabel = (range) =>
  range.kind === "absolute"
    ? spanText(range.from, range.to)
    : quickLabel(range);

const sameRange = (a, b) => JSON.stringify(a) === JSON.stringify(b);

/** `since=…[&until=…]`; ranges that reach now leave `until` to the server. */
function rangeParams(range) {
  const { from, to } = resolveRange(range);
  const params = `since=${Math.round(from)}`;
  return reachesNow(range) ? params : `${params}&until=${Math.round(to)}`;
}

/** `‹ [label] › ⊖` on one line; the label opens the form. */
function rangePicker(value, quick, onchange) {
  const forward = shiftRange(value, 1);
  const shift = (direction) => {
    const next = shiftRange(value, direction);
    if (next) onchange(next);
  };
  const trigger = h(
    "button",
    {
      class: "range-trigger",
      type: "button",
      "aria-haspopup": "dialog",
      title: "Choose time range",
      onclick: () => openRangeForm(trigger, value, quick, onchange),
    },
    h("span", { class: "range-icon", "aria-hidden": "true", text: "◷" }),
    h("span", { text: rangeLabel(value) }),
  );
  return h(
    "div",
    { class: "range-picker" },
    h("button", {
      class: "range-step",
      type: "button",
      "aria-label": "Previous period",
      title: "Previous period",
      text: "‹",
      onclick: () => shift(-1),
    }),
    trigger,
    h("button", {
      class: "range-step",
      type: "button",
      "aria-label": "Next period",
      title: "Next period",
      text: "›",
      disabled: forward === null,
      onclick: () => shift(1),
    }),
    h("button", {
      class: "range-step",
      type: "button",
      "aria-label": "Zoom out",
      title: "Zoom out (twice as long)",
      text: "⊖",
      onclick: () => onchange(zoomOut(value)),
    }),
  );
}

const touchLayout = () => matchMedia("(max-width: 640px)").matches;

/** The form in a popover under `anchor`, or a bottom sheet on a phone. */
function openRangeForm(anchor, value, quick, onapply) {
  const dialog = $("#range-dialog");
  const touch = touchLayout();
  const span = resolveRange(value);
  const close = () => dialog.close();
  const apply = (range) => {
    close();
    onapply(range);
  };
  const error = h("p", { class: "range-error", role: "alert", hidden: true });
  const field = (id, text, initial) => {
    const input = h("input", {
      id,
      type: "datetime-local",
      value: toInputValue(initial),
      oninput: () => (error.hidden = true),
    });
    return [h("label", { for: id, text }), input];
  };
  const [fromLabel, fromInput] = field("range-from", "From", span.from);
  const [toLabel, toInput] = field("range-to", "To", span.to);
  const submit = (event) => {
    event.preventDefault();
    const from = fromInputValue(fromInput.value);
    const to = fromInputValue(toInput.value);
    const problem = rangeProblem(from, to);
    if (problem) {
      error.textContent = PROBLEM_TEXT[problem];
      error.hidden = false;
      return;
    }
    apply({ kind: "absolute", from, to });
  };
  const zone = Intl.DateTimeFormat().resolvedOptions().timeZone;
  const absolute = h(
    "form",
    { class: "range-absolute", onsubmit: submit },
    h("h3", { text: "Absolute range" }),
    fromLabel,
    fromInput,
    toLabel,
    toInput,
    h("p", { class: "range-zone", text: `Local time (${zone})` }),
    error,
    h(
      "div",
      { class: "range-actions" },
      h("button", {
        class: "btn btn-quiet",
        type: "button",
        text: "Until now",
        onclick: () => {
          toInput.value = toInputValue(Math.ceil(now() / MINUTE) * MINUTE);
          error.hidden = true;
        },
      }),
      h("button", { class: "btn", type: "submit", text: "Apply" }),
    ),
  );
  const presets = h(
    "div",
    { class: "range-quick" },
    h("h3", { text: "Quick ranges" }),
    h(
      "div",
      { class: "range-quick-list" },
      quick.map((range) =>
        h("button", {
          type: "button",
          "aria-pressed": String(sameRange(range, value)),
          text: quickLabel(range),
          onclick: () => apply(range),
        }),
      ),
    ),
  );
  dialog.className = touch ? "range-sheet" : "range-popover";
  dialog.replaceChildren(
    h(
      "div",
      { class: "range-form" },
      touch ? [presets, absolute] : [absolute, presets],
    ),
  );
  dialog.onclick = (event) => event.target === dialog && close();
  dialog.showModal();
  if (!touch) {
    const box = anchor.getBoundingClientRect();
    const width = dialog.offsetWidth;
    dialog.style.top = `${box.bottom + 8}px`;
    dialog.style.left = `${Math.max(8, Math.min(box.left, innerWidth - width - 8))}px`;
  } else {
    dialog.style.top = "";
    dialog.style.left = "";
  }
}

/**
 * Grafana's brush: press, drag and release over `svg` between `x0` and `x1`
 * (viewBox units) to pick the span under the pointer. Mouse and pen only, so
 * a finger still scrolls the page; Esc cancels.
 */
function attachBrush(svg, { x0, x1, from, to, snap = (t) => t, onselect }) {
  const box = s("rect", {
    class: "brush",
    y: 0,
    height: "100%",
    visibility: "hidden",
  });
  svg.append(box);
  let start = null;
  const local = (event) => {
    const rect = svg.getBoundingClientRect();
    const scale = svg.viewBox.baseVal.width / rect.width;
    return Math.min(x1, Math.max(x0, (event.clientX - rect.left) * scale));
  };
  const timeAt = (x) => snap(from + ((x - x0) / (x1 - x0)) * (to - from));
  const draw = (x) => {
    box.setAttribute("x", Math.min(start, x));
    box.setAttribute("width", Math.max(1, Math.abs(x - start)));
    box.setAttribute(
      "visibility",
      Math.abs(x - start) >= 4 ? "visible" : "hidden",
    );
  };
  const cancel = () => {
    start = null;
    box.setAttribute("visibility", "hidden");
    removeEventListener("keydown", onKey);
  };
  const onKey = (event) => event.key === "Escape" && cancel();
  svg.classList.add("brushable");
  svg.addEventListener("pointerdown", (event) => {
    if (event.button !== 0 || event.pointerType === "touch") return;
    start = local(event);
    svg.setPointerCapture(event.pointerId);
    addEventListener("keydown", onKey);
  });
  svg.addEventListener(
    "pointermove",
    (event) => start !== null && draw(local(event)),
  );
  svg.addEventListener("pointerup", (event) => {
    if (start === null) return;
    const x = local(event);
    const moved = Math.abs(x - start) >= 4;
    const [a, b] = [timeAt(Math.min(start, x)), timeAt(Math.max(start, x))];
    cancel();
    if (moved && b > a) onselect(a, b);
  });
  svg.addEventListener("pointercancel", cancel);
}

// --- workspaces ---------------------------------------------------------------

const inWorkspace = (s) => !state.workspace || s.workspace === state.workspace;
const workspaceParam = () =>
  state.workspace ? `&workspace=${encodeURIComponent(state.workspace)}` : "";

function renderScopebar() {
  const bar = $("#scopebar");
  const names = state.workspaces;
  bar.hidden = !names.length;
  if (!names.length) return;
  if (state.workspace && !names.includes(state.workspace))
    state.workspace = null;
  bar.replaceChildren(
    h("span", { class: "scope-label", text: "Workspace" }),
    ...[null, ...names].map((name) =>
      h("button", {
        type: "button",
        role: "radio",
        "aria-checked": String(state.workspace === name),
        text: name ?? "All",
        onclick: () => setWorkspace(name),
      }),
    ),
  );
}

function setWorkspace(name) {
  state.workspace = name;
  if (name) localStorage.setItem(WORKSPACE_KEY, name);
  else localStorage.removeItem(WORKSPACE_KEY);
  renderScopebar();
  scheduleRender();
  if (!$("#view-history").hidden) loadHistory();
  if (!$("#view-analytics").hidden) loadAnalytics();
}

// --- history view -----------------------------------------------------------

const HISTORY_QUICK = [
  { kind: "relative", minutes: 60 },
  { kind: "relative", minutes: 180 },
  { kind: "relative", minutes: 360 },
  { kind: "relative", minutes: 720 },
  { kind: "relative", minutes: 1440 },
  { kind: "days", days: 1 },
  { kind: "relative", minutes: 2880 },
  { kind: "days", days: 7 },
];

const ANALYTICS_QUICK = [
  { kind: "days", days: 1 },
  { kind: "relative", minutes: 1440 },
  { kind: "days", days: 7 },
  { kind: "days", days: 14 },
  { kind: "days", days: 30 },
];

function loadRange(key, fallback) {
  try {
    const range = JSON.parse(localStorage.getItem(key));
    return range && range.kind !== "absolute" ? range : fallback;
  } catch {
    return fallback;
  }
}

/** Relative ranges are remembered; a fixed one belongs to the visit. */
function saveRange(key, range) {
  if (range.kind === "absolute") return;
  localStorage.setItem(key, JSON.stringify(range));
}

const snapMinute = (t) => Math.round(t / MINUTE) * MINUTE;
const brushHistory = (from, to) =>
  setHistoryRange({ kind: "absolute", from, to });

function setHistoryRange(range) {
  state.range = range;
  saveRange(HISTORY_RANGE_KEY, range);
  renderHistoryToolbar();
  loadHistory();
}

function renderHistoryToolbar() {
  $("#history-toolbar").replaceChildren(
    rangePicker(state.range, HISTORY_QUICK, setHistoryRange),
  );
}

async function loadHistory() {
  const container = $("#history");
  container.classList.add("loading");
  try {
    state.history = await getJson(
      `history?${rangeParams(state.range)}${workspaceParam()}`,
    );
    renderHistory();
  } catch (e) {
    if (e instanceof Unauthorized) askToken();
  } finally {
    container.classList.remove("loading");
  }
}

function totals(lane) {
  const out = { working: 0, permission: 0, your_turn: 0, idle: 0 };
  for (const [from, to, activity] of lane.segments)
    out[activity] = (out[activity] || 0) + (to - from);
  return out;
}

function renderHistory() {
  const data = state.history;
  if (!data) return;
  const sum = { working: 0, permission: 0, your_turn: 0, idle: 0 };
  for (const lane of data.lanes) {
    for (const [k, v] of Object.entries(totals(lane))) sum[k] += v;
  }
  const tiles = h(
    "div",
    { class: "tiles" },
    statTile(
      "Waited on you",
      fmtDuration(sum.permission + sum.your_turn),
      "total across sessions",
    ),
    statTile("Working time", fmtDuration(sum.working), "total across sessions"),
    statTile("Prompts", data.prompts, plural(data.lanes.length, "session")),
    statTile(
      "API-equivalent cost",
      fmtUsd(data.cost_usd),
      "from statusline totals",
    ),
  );
  $("#history").replaceChildren(
    tiles,
    timelineChart(data),
    projectsChart(data),
    toolsChart(data),
    usageChart(data),
  );
}

function statTile(name, value, sub) {
  return h(
    "div",
    { class: "tile" },
    h("div", { class: "tile-label", text: name }),
    h("div", { class: "tile-value", text: value }),
    sub ? h("div", { class: "tile-sub", text: sub }) : null,
  );
}

function chartWidth() {
  return Math.max(280, ($("#history").clientWidth || 600) - 32);
}

function chartFigure(title, sub, ...extra) {
  return h(
    "figure",
    { class: "chart" },
    h(
      "figcaption",
      {},
      h("div", { class: "chart-title", text: title }),
      h("div", { class: "chart-sub", text: sub }),
    ),
    ...extra,
  );
}

const stateLegend = (states) =>
  h("div", { class: "legend" }, states.map(legendChip));

function timeTicks(since, until, width) {
  const steps = [15, 30, 60, 120, 180, 360, 720, 1440, 2880, 4320, 10080].map(
    (m) => m * 60_000,
  );
  const maxTicks = Math.max(3, Math.floor(width / 110));
  const step =
    steps.find((st) => (until - since) / st <= maxTicks) ?? steps.at(-1);
  const first = new Date(since);
  first.setMinutes(0, 0, 0);
  if (step >= 86_400_000) first.setHours(0);
  else if (step > 3_600_000)
    first.setHours(first.getHours() - (first.getHours() % (step / 3_600_000)));
  const ticks = [];
  for (let t = first.getTime(); t <= until; t += step)
    if (t >= since) ticks.push(t);
  return { ticks, daily: step >= 86_400_000 };
}

function tooltipFor(figure) {
  const tip = h("div", { class: "tooltip", hidden: true });
  figure.append(tip);
  return {
    show(event, value, ...lines) {
      tip.replaceChildren(
        h("strong", { text: value }),
        ...lines
          .filter(Boolean)
          .map((l) => h("div", { class: "muted", text: l })),
      );
      tip.hidden = false;
      const box = figure.getBoundingClientRect();
      const x = event.clientX - box.left;
      const y = event.clientY - box.top;
      const flip = x > box.width - 200;
      tip.style.left = `${flip ? x - tip.offsetWidth - 12 : x + 12}px`;
      tip.style.top = `${Math.max(0, y - tip.offsetHeight - 8)}px`;
    },
    hide() {
      tip.hidden = true;
    },
  };
}

function tableView(headers, rows) {
  return h(
    "details",
    { class: "table-view" },
    h("summary", { text: "Table" }),
    h(
      "table",
      {},
      h(
        "thead",
        {},
        h(
          "tr",
          {},
          headers.map((t) => h("th", { text: t })),
        ),
      ),
      h(
        "tbody",
        {},
        rows.map((r) =>
          h(
            "tr",
            {},
            r.map((c) => h("td", { text: c })),
          ),
        ),
      ),
    ),
  );
}

const ellipsize = (text, max) =>
  text.length > max ? `${text.slice(0, max - 1)}…` : text;

/**
 * Horizontal bars, one row per entry, label above the bar and the value
 * label after it. `parts` are stacked `[value, activity|null]` pairs.
 */
function barRows(figure, rows, { valueText, tipText }, width = chartWidth()) {
  const rowH = 34;
  const barH = 12;
  const valueW = 92;
  const plotW = width - valueW;
  const max = Math.max(...rows.map((r) => r.total), 1);
  const svg = s("svg", {
    viewBox: `0 0 ${width} ${rows.length * rowH}`,
    height: rows.length * rowH,
    role: "img",
    "aria-label": figure.querySelector(".chart-title").textContent,
  });
  const tip = tooltipFor(figure);
  const maxChars = Math.floor(width / 7.2);
  rows.forEach((row, i) => {
    const top = i * rowH;
    svg.append(
      s("text", {
        class: "lane-label",
        x: 0,
        y: top + 12,
        text: ellipsize(row.label, maxChars),
      }),
    );
    let x = 0;
    for (const [value, activity] of row.parts) {
      if (value <= 0) continue;
      const w = (value / max) * plotW;
      const rect = s("rect", {
        class: activity ? "seg" : "seg seg-neutral",
        "data-activity": activity,
        x,
        y: top + 17,
        width: Math.max(1, w - 2),
        height: barH,
        rx: Math.min(3, w / 2),
      });
      rect.addEventListener("pointermove", (event) =>
        tip.show(event, ...tipText(row, value, activity)),
      );
      rect.addEventListener("pointerleave", () => tip.hide());
      svg.append(rect);
      x += w;
    }
    svg.append(
      s("text", {
        x: x + 6,
        y: top + 27,
        text: valueText(row),
      }),
    );
  });
  figure.append(svg);
}

function projectsChart(data) {
  const figure = chartFigure(
    "Time by project",
    "Working and waiting time per directory",
    stateLegend(STATES),
  );
  const by = new Map();
  for (const lane of data.lanes) {
    const key = place(lane.cwd);
    const t = totals(lane);
    const p = by.get(key) || {
      label: key,
      working: 0,
      permission: 0,
      your_turn: 0,
      sessions: 0,
    };
    for (const a of STATES) p[a] += t[a];
    p.sessions += 1;
    by.set(key, p);
  }
  let rows = [...by.values()]
    .map((p) => ({ ...p, total: p.working + p.permission + p.your_turn }))
    .filter((p) => p.total > 0)
    .sort((a, b) => b.total - a.total);
  if (rows.length > 8) {
    const rest = rows.slice(7);
    const other = { label: `Other (${rest.length})`, sessions: 0, total: 0 };
    for (const a of STATES) other[a] = rest.reduce((n, p) => n + p[a], 0);
    other.sessions = rest.reduce((n, p) => n + p.sessions, 0);
    other.total = rest.reduce((n, p) => n + p.total, 0);
    rows = [...rows.slice(0, 7), other];
  }
  if (!rows.length) {
    figure.append(
      h("p", { class: "muted", text: "No session activity in this range." }),
    );
    return figure;
  }
  for (const row of rows) row.parts = STATES.map((a) => [row[a], a]);
  barRows(figure, rows, {
    valueText: (row) => fmtDuration(row.total),
    tipText: (row, value, activity) => [
      fmtDuration(value),
      `${ACTIVITY[activity].label} · ${row.label}`,
      plural(row.sessions, "session"),
    ],
  });
  figure.append(
    tableView(
      ["Project", "Working", "Needs permission", "Your turn", "Sessions"],
      rows.map((r) => [
        r.label,
        fmtDuration(r.working),
        fmtDuration(r.permission),
        fmtDuration(r.your_turn),
        r.sessions,
      ]),
    ),
  );
  return figure;
}

function toolsChart(data) {
  const figure = chartFigure(
    "Tools",
    "Most used tools in the main conversation, by calls",
  );
  if (!data.tools.length) {
    figure.append(
      h("p", { class: "muted", text: "No tool calls in this range." }),
    );
    return figure;
  }
  const rows = data.tools.map((t) => ({
    ...t,
    label: toolName(t.name),
    total: t.calls,
    parts: [[t.calls, null]],
  }));
  barRows(figure, rows, {
    valueText: (row) =>
      `${row.calls}${row.failed ? ` · ${row.failed} failed` : ""}`,
    tipText: (row) => [
      plural(row.calls, "call"),
      row.label,
      row.failed ? `${row.failed} failed` : null,
    ],
  });
  figure.append(
    tableView(
      ["Tool", "Calls", "Failed"],
      rows.map((r) => [r.label, r.calls, r.failed]),
    ),
  );
  return figure;
}

function usageChart(data) {
  const figure = chartFigure(
    "5-hour limit used",
    `Highest reading per ${fmtDuration(data.bucket_ms)}`,
  );
  const points = data.usage.filter((p) => p.five_hour !== null);
  if (!points.length) {
    figure.append(
      h("p", { class: "muted", text: "No statusline data in this range yet." }),
    );
    return figure;
  }
  const width = chartWidth();
  const height = 160;
  const pad = { top: 8, right: 44, bottom: 22, left: 34 };
  const plotW = width - pad.left - pad.right;
  const plotH = height - pad.top - pad.bottom;
  const x = (t) =>
    pad.left + ((t - data.since) / (data.until - data.since)) * plotW;
  const y = (v) => pad.top + plotH - (Math.min(100, v) / 100) * plotH;

  const svg = s("svg", {
    viewBox: `0 0 ${width} ${height}`,
    height,
    role: "img",
    "aria-label": "5-hour limit used over time",
  });
  for (const v of [0, 50, 100]) {
    svg.append(
      s("line", {
        class: v ? "grid" : "baseline",
        x1: pad.left,
        x2: pad.left + plotW,
        y1: y(v),
        y2: y(v),
      }),
      s("text", {
        x: pad.left - 6,
        y: y(v) + 4,
        "text-anchor": "end",
        text: `${v}%`,
      }),
    );
  }
  const { ticks, daily } = timeTicks(data.since, data.until, plotW);
  for (const t of ticks) {
    svg.append(
      s("text", {
        x: x(t),
        y: height - 6,
        "text-anchor": "middle",
        text: daily ? dayFmt.format(t) : fmtTime(t),
      }),
    );
  }

  // NOTE: statusline samples only exist while sessions render, so short gaps are normal
  const maxGap = Math.max(data.bucket_ms * 3, 20 * 60_000);
  const runs = [];
  for (const p of points) {
    const run = runs.at(-1);
    if (run && p.ts - run.at(-1).ts <= maxGap) run.push(p);
    else runs.push([p]);
  }
  for (const run of runs) {
    if (run.length === 1) {
      svg.append(
        s("circle", {
          class: "end-dot",
          cx: x(run[0].ts),
          cy: y(run[0].five_hour),
          r: 3,
        }),
      );
      continue;
    }
    const line = run
      .map(
        (p, i) =>
          `${i ? "L" : "M"}${x(p.ts).toFixed(1)},${y(p.five_hour).toFixed(1)}`,
      )
      .join("");
    const area = `${line}L${x(run.at(-1).ts).toFixed(1)},${y(0)}L${x(run[0].ts).toFixed(1)},${y(0)}Z`;
    svg.append(
      s("path", { class: "area", d: area }),
      s("path", { class: "line", d: line }),
    );
  }
  const last = points.at(-1);
  svg.append(
    s("circle", {
      class: "end-dot",
      cx: x(last.ts),
      cy: y(last.five_hour),
      r: 4,
    }),
    s("text", {
      x: x(last.ts) + 8,
      y: y(last.five_hour) + 4,
      text: `${Math.round(last.five_hour)}%`,
    }),
  );

  const crosshair = s("line", {
    class: "crosshair",
    y1: pad.top,
    y2: pad.top + plotH,
    visibility: "hidden",
  });
  const hit = s("rect", {
    x: pad.left,
    y: pad.top,
    width: plotW,
    height: plotH,
    fill: "transparent",
  });
  svg.append(crosshair, hit);
  attachBrush(svg, {
    x0: pad.left,
    x1: pad.left + plotW,
    from: data.since,
    to: data.until,
    snap: snapMinute,
    onselect: brushHistory,
  });
  figure.append(svg);
  const tip = tooltipFor(figure);
  hit.addEventListener("pointermove", (event) => {
    const box = svg.getBoundingClientRect();
    const t =
      data.since +
      (((event.clientX - box.left) * (width / box.width) - pad.left) / plotW) *
        (data.until - data.since);
    const p = points.reduce((best, q) =>
      Math.abs(q.ts - t) < Math.abs(best.ts - t) ? q : best,
    );
    crosshair.setAttribute("x1", x(p.ts));
    crosshair.setAttribute("x2", x(p.ts));
    crosshair.setAttribute("visibility", "visible");
    tip.show(
      event,
      `${Math.round(p.five_hour)}%`,
      `5-hour limit · ${daily ? dayFmt.format(p.ts) + " " : ""}${fmtTime(p.ts)}`,
      p.seven_day !== null ? `7-day limit ${Math.round(p.seven_day)}%` : "",
    );
  });
  hit.addEventListener("pointerleave", () => {
    crosshair.setAttribute("visibility", "hidden");
    tip.hide();
  });
  figure.append(
    tableView(
      ["Time", "5-hour", "7-day"],
      points.map((p) => [
        `${daily ? dayFmt.format(p.ts) + " " : ""}${fmtTime(p.ts)}`,
        `${Math.round(p.five_hour)}%`,
        p.seven_day !== null ? `${Math.round(p.seven_day)}%` : "–",
      ]),
    ),
  );
  return figure;
}

function timelineChart(data) {
  const figure = chartFigure(
    "Session activity",
    "When each session was working or waiting on you",
    stateLegend([...STATES, "idle"]),
  );
  if (!data.lanes.length) {
    figure.append(
      h("p", { class: "muted", text: "No session activity in this range." }),
    );
    return figure;
  }
  const width = chartWidth();
  const pad = { top: 18, left: 0, right: 0 };
  const laneH = 36;
  const barH = 12;
  const height = pad.top + data.lanes.length * laneH;
  const plotW = width - pad.left - pad.right;
  const x = (t) =>
    pad.left +
    ((Math.max(t, data.since) - data.since) / (data.until - data.since)) *
      plotW;

  const svg = s("svg", {
    viewBox: `0 0 ${width} ${height}`,
    height,
    role: "img",
    "aria-label": "Session activity timeline",
  });
  const { ticks, daily } = timeTicks(data.since, data.until, plotW);
  for (const t of ticks) {
    svg.append(
      s("line", {
        class: "grid",
        x1: x(t),
        x2: x(t),
        y1: pad.top - 4,
        y2: height,
      }),
      x(t) > width - 70
        ? s("text", {
            x: x(t) - 3,
            y: 11,
            "text-anchor": "end",
            text: daily ? dayFmt.format(t) : fmtTime(t),
          })
        : s("text", {
            x: x(t) + 3,
            y: 11,
            text: daily ? dayFmt.format(t) : fmtTime(t),
          }),
    );
  }
  const tip = tooltipFor(figure);
  const maxChars = Math.floor(width / 7.2);
  data.lanes.forEach((lane, i) => {
    const top = pad.top + i * laneH;
    svg.append(
      s("text", {
        class: "lane-label",
        x: pad.left,
        y: top + 13,
        text: ellipsize(
          `${lane.background ? "bg · " : ""}${lane.label} · ${place(lane.cwd)}`,
          maxChars,
        ),
      }),
    );
    for (const [from, to, activity] of lane.segments) {
      const x0 = x(from);
      const w = Math.max(1, x(to) - x0 - 2);
      const seg = s("rect", {
        class: "seg",
        "data-activity": activity,
        x: x0,
        y: top + 18,
        width: w,
        height: barH,
        rx: Math.min(3, w / 2),
      });
      seg.addEventListener("pointermove", (event) =>
        tip.show(
          event,
          fmtDuration(to - from),
          `${ACTIVITY[activity].label} · ${fmtTime(from)}–${fmtTime(to)}`,
          lane.label,
        ),
      );
      seg.addEventListener("pointerleave", () => tip.hide());
      svg.append(seg);
    }
  });
  attachBrush(svg, {
    x0: pad.left,
    x1: pad.left + plotW,
    from: data.since,
    to: data.until,
    snap: snapMinute,
    onselect: brushHistory,
  });
  figure.append(
    svg,
    tableView(
      ["Session", "Working", "Needs permission", "Your turn", "Idle"],
      data.lanes.map((lane) => {
        const t = totals(lane);
        return [
          lane.label,
          fmtDuration(t.working),
          fmtDuration(t.permission),
          fmtDuration(t.your_turn),
          fmtDuration(t.idle),
        ];
      }),
    ),
  );
  return figure;
}

// --- chart building blocks --------------------------------------------------

/** Round tick values covering `lo..hi` (both inclusive of 0). */
function niceTicks(lo, hi, count = 4) {
  const span = hi - lo || 1;
  const raw = span / count;
  const mag = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 2.5, 5, 10].map((m) => m * mag).find((st) => st >= raw);
  const ticks = [];
  for (let t = Math.floor(lo / step) * step; t <= hi + step / 2; t += step)
    ticks.push(Number(t.toFixed(10)));
  if (ticks.at(-1) < hi) ticks.push(ticks.at(-1) + step);
  return ticks;
}

/**
 * One series over time: line + soft area, crosshair tooltip, end label.
 * `points` are `[ts, value]`; long gaps break the line.
 */
function lineChart({
  title,
  sub,
  points,
  since,
  until,
  width,
  fmt,
  floorMax,
  onbrush,
}) {
  const figure = chartFigure(title, sub);
  if (points.length < 2) {
    figure.append(h("p", { class: "muted", text: "Not enough data yet." }));
    return figure;
  }
  const height = 150;
  const pad = { top: 8, right: 52, bottom: 22, left: 46 };
  const plotW = width - pad.left - pad.right;
  const plotH = height - pad.top - pad.bottom;
  const peak = Math.max(...points.map(([, v]) => v));
  const ticks = niceTicks(0, Math.max(peak, floorMax * 0.05), 3);
  const top = ticks.at(-1);
  const x = (t) => pad.left + ((t - since) / (until - since)) * plotW;
  const y = (v) => pad.top + plotH - (v / top) * plotH;
  const svg = s("svg", {
    viewBox: `0 0 ${width} ${height}`,
    height,
    role: "img",
    "aria-label": title,
  });
  for (const v of ticks) {
    svg.append(
      s("line", {
        class: v ? "grid" : "baseline",
        x1: pad.left,
        x2: pad.left + plotW,
        y1: y(v),
        y2: y(v),
      }),
      s("text", {
        x: pad.left - 6,
        y: y(v) + 4,
        "text-anchor": "end",
        text: fmt(v),
      }),
    );
  }
  const { ticks: times, daily } = timeTicks(since, until, plotW);
  for (const t of times) {
    svg.append(
      s("text", {
        x: x(t),
        y: height - 6,
        "text-anchor": "middle",
        text: daily ? dayFmt.format(t) : fmtTime(t),
      }),
    );
  }
  const steps = points.slice(1).map(([t], i) => t - points[i][0]);
  const maxGap = 3 * (steps.sort((a, b) => a - b)[steps.length >> 1] || 60_000);
  const runs = [];
  for (const point of points) {
    const run = runs.at(-1);
    if (run && point[0] - run.at(-1)[0] <= maxGap) run.push(point);
    else runs.push([point]);
  }
  for (const run of runs) {
    if (run.length === 1) {
      svg.append(
        s("circle", {
          class: "end-dot",
          cx: x(run[0][0]),
          cy: y(run[0][1]),
          r: 3,
        }),
      );
      continue;
    }
    const line = run
      .map(
        ([t, v], i) => `${i ? "L" : "M"}${x(t).toFixed(1)},${y(v).toFixed(1)}`,
      )
      .join("");
    svg.append(
      s("path", {
        class: "area",
        d: `${line}L${x(run.at(-1)[0]).toFixed(1)},${y(0)}L${x(run[0][0]).toFixed(1)},${y(0)}Z`,
      }),
      s("path", { class: "line", d: line }),
    );
  }
  const [lastT, lastV] = points.at(-1);
  svg.append(
    s("circle", { class: "end-dot", cx: x(lastT), cy: y(lastV), r: 4 }),
    s("text", { x: x(lastT) + 8, y: y(lastV) + 4, text: fmt(lastV) }),
  );
  const crosshair = s("line", {
    class: "crosshair",
    y1: pad.top,
    y2: pad.top + plotH,
    visibility: "hidden",
  });
  const hit = s("rect", {
    x: pad.left,
    y: pad.top,
    width: plotW,
    height: plotH,
    fill: "transparent",
  });
  svg.append(crosshair, hit);
  if (onbrush) {
    attachBrush(svg, {
      x0: pad.left,
      x1: pad.left + plotW,
      from: since,
      to: until,
      snap: snapMinute,
      onselect: onbrush,
    });
  }
  figure.append(svg);
  const tip = tooltipFor(figure);
  hit.addEventListener("pointermove", (event) => {
    const box = svg.getBoundingClientRect();
    const t =
      since +
      (((event.clientX - box.left) * (width / box.width) - pad.left) / plotW) *
        (until - since);
    const [pt, pv] = points.reduce((best, q) =>
      Math.abs(q[0] - t) < Math.abs(best[0] - t) ? q : best,
    );
    crosshair.setAttribute("x1", x(pt));
    crosshair.setAttribute("x2", x(pt));
    crosshair.setAttribute("visibility", "visible");
    tip.show(event, fmt(pv), daily ? fmtStamp(pt) : fmtTime(pt));
  });
  hit.addEventListener("pointerleave", () => {
    crosshair.setAttribute("visibility", "hidden");
    tip.hide();
  });
  figure.append(
    tableView(
      ["Time", title],
      points.map(([t, v]) => [fmtStamp(t), fmt(v)]),
    ),
  );
  return figure;
}

/**
 * Vertical bars, one per column, stacked from `parts` (`[value, activity]`;
 * activity null = neutral fill). Negative values hang below the baseline.
 */
function columnChart(
  figure,
  cols,
  { width, fmtY, tip, height = 190, brush = null },
) {
  const pad = { top: 10, right: 6, bottom: 24, left: 46 };
  const plotW = width - pad.left - pad.right;
  const plotH = height - pad.top - pad.bottom;
  let hi = 0;
  let lo = 0;
  for (const col of cols) {
    const up = col.parts.reduce((n, [v]) => n + Math.max(0, v), 0);
    const down = col.parts.reduce((n, [v]) => n + Math.min(0, v), 0);
    hi = Math.max(hi, up);
    lo = Math.min(lo, down);
  }
  const ticks = niceTicks(lo, hi || 1, 4);
  const [min, max] = [ticks[0], ticks.at(-1)];
  const y = (v) => pad.top + ((max - v) / (max - min)) * plotH;
  const svg = s("svg", {
    viewBox: `0 0 ${width} ${height}`,
    height,
    role: "img",
    "aria-label": figure.querySelector(".chart-title").textContent,
  });
  for (const v of ticks) {
    svg.append(
      s("line", {
        class: v === 0 ? "baseline" : "grid",
        x1: pad.left,
        x2: pad.left + plotW,
        y1: y(v),
        y2: y(v),
      }),
      s("text", {
        x: pad.left - 6,
        y: y(v) + 4,
        "text-anchor": "end",
        text: fmtY(v),
      }),
    );
  }
  const slot = plotW / cols.length;
  const barW = Math.max(3, Math.min(32, slot * 0.66));
  const every = Math.ceil((cols.length * 52) / plotW);
  const tooltip = tooltipFor(figure);
  cols.forEach((col, i) => {
    const cx = pad.left + slot * (i + 0.5);
    let up = 0;
    let down = 0;
    for (const [value, activity] of col.parts) {
      if (!value) continue;
      const from = value > 0 ? up : down;
      const to = from + value;
      if (value > 0) up = to;
      else down = to;
      const top = y(Math.max(from, to));
      const h0 = Math.abs(y(from) - y(to));
      const rect = s("rect", {
        class: activity ? "seg" : "seg seg-neutral",
        "data-activity": activity,
        x: cx - barW / 2,
        y: top + (value > 0 ? 0 : 2),
        width: barW,
        height: Math.max(1, h0 - 2),
        rx: Math.min(2, barW / 2),
      });
      rect.addEventListener("pointermove", (event) =>
        tooltip.show(event, ...tip(col, value, activity)),
      );
      rect.addEventListener("pointerleave", () => tooltip.hide());
      svg.append(rect);
    }
    if (i % every === 0) {
      svg.append(
        s("text", {
          x: cx,
          y: height - 6,
          "text-anchor": "middle",
          text: col.label,
        }),
      );
    }
  });
  if (brush) attachBrush(svg, { x0: pad.left, x1: pad.left + plotW, ...brush });
  figure.append(svg);
}

const SEQ_LEVELS = 6;

/** Weekday × hour counts, Monday first. */
function heatmapChart(matrix, width) {
  const figure = chartFigure("When you prompt", "Prompts by weekday and hour");
  const max = Math.max(0, ...matrix.flat());
  if (!max) {
    figure.append(
      h("p", { class: "muted", text: "No prompts in this range." }),
    );
    return figure;
  }
  const names = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
  const pad = { top: 16, left: 34 };
  const cell = Math.max(9, Math.min(28, Math.floor((width - pad.left) / 24)));
  const height = pad.top + 7 * cell;
  const level = (n) => (n ? Math.max(1, Math.ceil((n / max) * SEQ_LEVELS)) : 0);
  const svg = s("svg", {
    viewBox: `0 0 ${pad.left + 24 * cell} ${height}`,
    width: pad.left + 24 * cell,
    height,
    role: "img",
    "aria-label": "Prompts by weekday and hour",
  });
  for (let hour = 0; hour < 24; hour += cell < 16 ? 6 : 3) {
    svg.append(
      s("text", {
        x: pad.left + hour * cell + 1,
        y: 11,
        text: String(hour).padStart(2, "0"),
      }),
    );
  }
  const tip = tooltipFor(figure);
  matrix.forEach((row, day) => {
    svg.append(
      s("text", {
        x: 0,
        y: pad.top + day * cell + cell / 2 + 4,
        text: names[day],
      }),
    );
    row.forEach((n, hour) => {
      const rect = s("rect", {
        class: "heat",
        "data-level": level(n),
        x: pad.left + hour * cell,
        y: pad.top + day * cell,
        width: cell - 2,
        height: cell - 2,
        rx: 2,
      });
      rect.addEventListener("pointermove", (event) =>
        tip.show(
          event,
          plural(n, "prompt"),
          `${names[day]} ${String(hour).padStart(2, "0")}:00–${String(hour + 1).padStart(2, "0")}:00`,
        ),
      );
      rect.addEventListener("pointerleave", () => tip.hide());
      svg.append(rect);
    });
  });
  figure.append(
    h("div", { class: "heat-wrap" }, svg),
    h(
      "div",
      { class: "legend heat-legend" },
      h("span", { text: "Fewer" }),
      [...Array(SEQ_LEVELS).keys()].map((i) =>
        h("i", {
          class: "heat-swatch",
          "data-level": i + 1,
          "aria-hidden": "true",
        }),
      ),
      h("span", { text: `More (max ${max})` }),
    ),
    tableView(
      ["Day", ...[...Array(24).keys()].map((hr) => String(hr))],
      matrix.map((row, day) => [names[day], ...row]),
    ),
  );
  return figure;
}

// --- analytics view ---------------------------------------------------------

const BIN_LABELS = ["<30s", "30s–2m", "2–5m", "5–15m", "15m–1h", ">1h"];
const shortDay = new Intl.DateTimeFormat(undefined, {
  day: "numeric",
  month: "numeric",
});

function setAnalyticsRange(range) {
  state.aRange = range;
  saveRange(ANALYTICS_RANGE_KEY, range);
  renderAnalyticsToolbar();
  loadAnalytics();
}

function renderAnalyticsToolbar() {
  $("#analytics-toolbar").replaceChildren(
    rangePicker(state.aRange, ANALYTICS_QUICK, setAnalyticsRange),
  );
}

async function loadAnalytics() {
  const container = $("#analytics");
  container.classList.add("loading");
  try {
    const tz = -new Date().getTimezoneOffset();
    state.analytics = await getJson(
      `analytics?${rangeParams(state.aRange)}&tz=${tz}${workspaceParam()}`,
    );
    renderAnalytics();
  } catch (e) {
    if (e instanceof Unauthorized) askToken();
  } finally {
    container.classList.remove("loading");
  }
}

const hourly = (data) => data.bucket_ms < 86_400_000;

/** Brush over bucket columns: snaps to bucket edges, selects a fixed range. */
function bucketBrush(data) {
  const edges = [
    ...data.buckets.map((b) => b.start),
    data.buckets.at(-1).start + data.bucket_ms,
  ];
  return {
    from: edges[0],
    to: edges.at(-1),
    snap: (t) =>
      edges.reduce((best, e) =>
        Math.abs(e - t) < Math.abs(best - t) ? e : best,
      ),
    onselect: (from, to) =>
      setAnalyticsRange({ kind: "absolute", from, to: Math.min(to, now()) }),
  };
}

function renderAnalytics() {
  const data = state.analytics;
  if (!data) return;
  const width = Math.max(280, ($("#analytics").clientWidth || 600) - 32);
  const half = width > 760 ? (width - 12) / 2 - 32 : width;
  const sum = (key) => data.buckets.reduce((n, d) => n + d[key], 0);
  const peak = Math.max(0, ...data.buckets.map((d) => d.peak_mem_bytes));
  const tiles = h(
    "div",
    { class: "tiles" },
    statTile("Prompts", sum("prompts"), plural(data.sessions, "session")),
    statTile("Working time", fmtDuration(sum("working_ms")), "Claude busy"),
    statTile(
      "Waited on you",
      fmtDuration(sum("permission_ms") + sum("your_turn_ms")),
      "permission prompts and your turn",
    ),
    statTile("API-equivalent cost", fmtUsd(sum("cost_usd")), "from statusline"),
    statTile(
      "Lines changed",
      `+${compact.format(sum("lines_added"))} −${compact.format(sum("lines_removed"))}`,
      "added / removed",
    ),
    statTile(
      "Peak memory",
      peak ? fmtBytes(peak) : "–",
      "all sessions at once",
    ),
  );
  $("#analytics").replaceChildren(
    tiles,
    dailyActivityChart(data, width),
    heatmapChart(data.heatmap, width),
    h(
      "div",
      { class: "chart-pair" },
      histogramChart(
        "How long Claude works per prompt",
        data.turns,
        "working",
        half,
      ),
      histogramChart(
        "How long sessions wait for you",
        data.responses,
        "your_turn",
        half,
      ),
    ),
    h(
      "div",
      { class: "chart-pair" },
      costChart(data, half),
      modelChart(data, half),
    ),
    linesChart(data, width),
    h(
      "div",
      { class: "chart-pair" },
      lineChart({
        title: "Memory",
        sub: "All session process trees together (PSS), average per bucket",
        since: data.since,
        until: data.until,
        width: half,
        ...byteSeries(data.memory),
        onbrush: (from, to) =>
          setAnalyticsRange({ kind: "absolute", from, to }),
      }),
      cpuChart(data, half),
    ),
  );
}

const bucketLabel = (data, ts) =>
  hourly(data)
    ? fmtTime(ts)
    : data.buckets.length > 10
      ? shortDay.format(ts)
      : dayFmt.format(ts);

const bucketName = (data, ts) =>
  hourly(data) ? `${dayFmt.format(ts)} ${fmtTime(ts)}` : dayFmt.format(ts);

function dailyActivityChart(data, width) {
  const figure = chartFigure(
    hourly(data) ? "Hourly activity" : "Daily activity",
    `Time each ${hourly(data) ? "hour" : "day"} that sessions were working or waiting on you`,
    stateLegend(STATES),
  );
  columnChart(
    figure,
    data.buckets.map((d) => ({
      label: bucketLabel(data, d.start),
      day: d,
      parts: [
        [d.working_ms / 3_600_000, "working"],
        [d.permission_ms / 3_600_000, "permission"],
        [d.your_turn_ms / 3_600_000, "your_turn"],
      ],
    })),
    {
      width,
      fmtY: (v) =>
        v === 0 ? "0" : v >= 1 ? `${v}h` : `${Math.round(v * 60)}m`,
      brush: bucketBrush(data),
      tip: (col, value, activity) => [
        fmtDuration(value * 3_600_000),
        `${ACTIVITY[activity].label} · ${bucketName(data, col.day.start)}`,
      ],
    },
  );
  figure.append(
    tableView(
      ["Day", "Working", "Needs permission", "Your turn", "Prompts"],
      data.buckets.map((d) => [
        bucketName(data, d.start),
        fmtDuration(d.working_ms),
        fmtDuration(d.permission_ms),
        fmtDuration(d.your_turn_ms),
        d.prompts,
      ]),
    ),
  );
  return figure;
}

function histogramChart(title, hist, activity, width) {
  const total = hist.counts.reduce((n, c) => n + c, 0);
  const figure = chartFigure(
    title,
    total
      ? `${plural(total, "case")}, median ${fmtDuration(hist.median_ms)}`
      : "No data in this range yet",
  );
  if (!total) return figure;
  columnChart(
    figure,
    hist.counts.map((count, i) => ({
      label: BIN_LABELS[i],
      parts: [[count, activity]],
    })),
    {
      width,
      height: 170,
      fmtY: (v) => String(v),
      tip: (col, value) => [plural(value, "case"), col.label],
    },
  );
  figure.append(
    tableView(
      ["Duration", "Count"],
      hist.counts.map((count, i) => [BIN_LABELS[i], count]),
    ),
  );
  return figure;
}

function costChart(data, width) {
  const figure = chartFigure(
    hourly(data) ? "Cost per hour" : "Cost per day",
    "API-equivalent, from statusline totals",
  );
  columnChart(
    figure,
    data.buckets.map((d) => ({
      label: bucketLabel(data, d.start),
      day: d,
      parts: [[d.cost_usd, null]],
    })),
    {
      width,
      height: 170,
      fmtY: (v) => `$${v}`,
      brush: bucketBrush(data),
      tip: (col, value) => [fmtUsd(value), bucketName(data, col.day.start)],
    },
  );
  figure.append(
    tableView(
      ["Day", "Cost"],
      data.buckets.map((d) => [bucketName(data, d.start), fmtUsd(d.cost_usd)]),
    ),
  );
  return figure;
}

function modelChart(data, width) {
  const figure = chartFigure("Cost by model", "Model shown in the statusline");
  const rows = data.models
    .filter((m) => m.cost_usd > 0)
    .map((m) => ({
      ...m,
      label: m.model,
      total: m.cost_usd,
      parts: [[m.cost_usd, null]],
    }));
  if (!rows.length) {
    figure.append(h("p", { class: "muted", text: "No cost data yet." }));
    return figure;
  }
  barRows(
    figure,
    rows,
    {
      valueText: (row) => fmtUsd(row.cost_usd),
      tipText: (row) => [
        fmtUsd(row.cost_usd),
        row.label,
        plural(row.sessions, "session"),
      ],
    },
    width,
  );
  figure.append(
    tableView(
      ["Model", "Cost", "Sessions"],
      rows.map((r) => [r.label, fmtUsd(r.cost_usd), r.sessions]),
    ),
  );
  return figure;
}

function linesChart(data, width) {
  const figure = chartFigure(
    hourly(data) ? "Lines changed per hour" : "Lines changed per day",
    "Added above the line, removed below",
  );
  columnChart(
    figure,
    data.buckets.map((d) => ({
      label: bucketLabel(data, d.start),
      day: d,
      parts: [
        [d.lines_added, null],
        [-d.lines_removed, null],
      ],
    })),
    {
      width,
      height: 170,
      fmtY: (v) => (v > 0 ? `+${v}` : v < 0 ? `−${-v}` : "0"),
      brush: bucketBrush(data),
      tip: (col, value) => [
        value > 0 ? `+${value} added` : `−${-value} removed`,
        bucketName(data, col.day.start),
      ],
    },
  );
  figure.append(
    tableView(
      ["Day", "Added", "Removed"],
      data.buckets.map((d) => [
        bucketName(data, d.start),
        d.lines_added,
        d.lines_removed,
      ]),
    ),
  );
  return figure;
}

function cpuChart(data, width) {
  const figure = chartFigure(
    "CPU by project",
    "Core-hours used by session process trees",
  );
  const rows = data.projects
    .filter((p) => p.cpu_secs > 0)
    .slice(0, 8)
    .map((p) => ({
      ...p,
      label: place(p.cwd),
      total: p.cpu_secs,
      parts: [[p.cpu_secs, null]],
    }));
  if (!rows.length) {
    figure.append(h("p", { class: "muted", text: "No process data yet." }));
    return figure;
  }
  const fmtCpu = (secs) =>
    secs >= 3600
      ? `${(secs / 3600).toFixed(1)} core-h`
      : `${Math.round(secs / 60)} core-min`;
  barRows(
    figure,
    rows,
    {
      valueText: (row) => fmtCpu(row.cpu_secs),
      tipText: (row) => [
        fmtCpu(row.cpu_secs),
        row.label,
        `peak ${fmtBytes(row.peak_mem_bytes)}`,
      ],
    },
    width,
  );
  figure.append(
    tableView(
      ["Project", "CPU", "Peak memory"],
      rows.map((r) => [
        r.label,
        fmtCpu(r.cpu_secs),
        fmtBytes(r.peak_mem_bytes),
      ]),
    ),
  );
  return figure;
}

// --- chrome -----------------------------------------------------------------

function applyTheme() {
  const theme = localStorage.getItem(THEME_KEY) || "auto";
  if (theme === "auto") delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = theme;
  const button = $("#theme");
  button.textContent = { auto: "◐", light: "☀︎", dark: "☾" }[theme];
  button.setAttribute("aria-label", `Theme: ${theme}`);
}

function cycleTheme() {
  const order = ["auto", "light", "dark"];
  const current = localStorage.getItem(THEME_KEY) || "auto";
  localStorage.setItem(
    THEME_KEY,
    order[(order.indexOf(current) + 1) % order.length],
  );
  applyTheme();
}

function showView(view) {
  localStorage.setItem(VIEW_KEY, view);
  for (const tab of document.querySelectorAll("[data-view]")) {
    tab.setAttribute("aria-selected", String(tab.dataset.view === view));
  }
  $("#view-sessions").hidden = view !== "sessions";
  $("#view-history").hidden = view !== "history";
  $("#view-analytics").hidden = view !== "analytics";
  if (view === "history") loadHistory();
  else if (view === "analytics") loadAnalytics();
  else loadRecent();
}

function start() {
  takeHash();
  addEventListener("hashchange", takeHash);
  applyTheme();
  $("#theme").addEventListener("click", cycleTheme);
  for (const tab of document.querySelectorAll("[data-view]")) {
    tab.addEventListener("click", () => showView(tab.dataset.view));
  }
  state.range = loadRange(HISTORY_RANGE_KEY, state.range);
  state.aRange = loadRange(ANALYTICS_RANGE_KEY, state.aRange);
  renderHistoryToolbar();
  renderAnalyticsToolbar();
  renderScopebar();
  $("#detail").addEventListener("close", () => {
    state.detailId = null;
    state.convo = null;
  });
  $("#detail").addEventListener("click", (event) => {
    if (event.target === event.currentTarget) event.currentTarget.close();
  });
  $("#login-form").addEventListener("submit", () => {
    localStorage.setItem(TOKEN_KEY, $("#login-token").value.trim());
    runStream();
  });

  let resizeTimer = null;
  new ResizeObserver(() => {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(renderHistory, 150);
  }).observe($("#view-history"));
  let analyticsResize = null;
  new ResizeObserver(() => {
    clearTimeout(analyticsResize);
    analyticsResize = setTimeout(renderAnalytics, 150);
  }).observe($("#view-analytics"));

  setInterval(tickClocks, 1000);
  setInterval(scheduleRender, 15_000);
  setInterval(reconnectIfStale, 10_000);
  setInterval(() => {
    if (!$("#view-history").hidden) {
      if (reachesNow(state.range)) loadHistory();
    } else if ($("#view-analytics").hidden) loadRecent();
  }, 60_000);
  setInterval(() => {
    if (!$("#view-analytics").hidden && reachesNow(state.aRange))
      loadAnalytics();
  }, 300_000);
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") reconnectIfStale();
  });

  showView(localStorage.getItem(VIEW_KEY) || "sessions");
  if (!token()) return askToken();
  runStream();
}

start();
