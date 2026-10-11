const TOKEN_KEY = "ccdash.token";
const THEME_KEY = "ccdash.theme";
const VIEW_KEY = "ccdash.view";
const STALE_STREAM_MS = 45_000;

const ACTIVITY = {
  permission: { label: "Needs permission", rank: 0 },
  your_turn: { label: "Your turn", rank: 1 },
  working: { label: "Working", rank: 2 },
  idle: { label: "Idle", rank: 3 },
  gone: { label: "Ended", rank: 4 },
};

const state = {
  host: "",
  skew: 0,
  sessions: new Map(),
  rateLimits: null,
  detailId: null,
  range: "today",
  history: null,
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
  for (const child of children.flat()) {
    if (child === null || child === undefined || child === false) continue;
    el.append(
      child instanceof Node ? child : document.createTextNode(String(child)),
    );
  }
}

const $ = (sel) => document.querySelector(sel);

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
const fmtUsd = (usd) =>
  usd >= 100 ? `$${usd.toFixed(0)}` : `$${usd.toFixed(2)}`;

function place(cwd) {
  const parts = (cwd || "").split("/").filter(Boolean);
  return parts.slice(-2).join("/") || cwd || "";
}

const label = (s) => s.title || s.name || s.id.slice(0, 8);
const firstLine = (text) =>
  (text || "").split("\n").find((line) => line.trim()) || "";

function meterLevel(pct) {
  if (pct >= 90) return "critical";
  if (pct >= 75) return "warning";
  return "normal";
}

// --- API --------------------------------------------------------------------

class Unauthorized extends Error {}

const token = () => localStorage.getItem(TOKEN_KEY) || "";

function takeTokenFromHash() {
  const match = location.hash.match(/token=([^&]+)/);
  if (!match) return;
  localStorage.setItem(TOKEN_KEY, decodeURIComponent(match[1]));
  history.replaceState(null, "", location.pathname + location.search);
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
  scheduleRender();
}

function applyUpdate(update) {
  if (update.type === "session") {
    const previous = state.sessions.get(update.session.id);
    state.sessions.set(update.session.id, update.session);
    if (
      state.detailId === update.session.id &&
      detailChanged(previous, update.session)
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

const detailChanged = (a, b) =>
  !a ||
  a.activity !== b.activity ||
  a.reply?.at !== b.reply?.at ||
  a.prompt?.at !== b.prompt?.at ||
  a.tool?.started_at !== b.tool?.started_at;

// --- sessions view ----------------------------------------------------------

let renderQueued = false;

function scheduleRender() {
  if (renderQueued) return;
  renderQueued = true;
  requestAnimationFrame(() => {
    renderQueued = false;
    renderSessions();
    if (state.detailId) renderDetailHead();
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
  const all = [...state.sessions.values()].sort(byAttention);
  const needs = all.filter(
    (s) => s.activity === "permission" || s.activity === "your_turn",
  );
  const working = all.filter((s) => s.activity === "working");
  const idle = all.filter((s) => s.activity === "idle");
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

  $("#tiles").replaceChildren(
    countTile("Working", working.length),
    countTile("Idle", idle.length),
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
  }
  if (needs.length) groups.push(group("Needs you", needs));
  if (working.length) groups.push(group("Working", working));
  if (idle.length) groups.push(group("Idle", idle));
  if (gone.length) groups.push(group("Ended", gone, true));
  $("#groups").replaceChildren(...groups);
}

function countTile(name, value) {
  return h(
    "div",
    { class: "tile" },
    h("div", { class: "tile-label", text: name }),
    h("div", { class: "tile-value", text: value }),
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

function group(title, sessions, collapsed = false) {
  const cards = h("div", { class: "cards" }, sessions.map(card));
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

function sessionDetail(s) {
  if (s.waiting_for) return s.waiting_for;
  if (s.tool) return `${s.tool.name}: ${s.tool.summary}`.replace(/: $/, "");
  if (s.activity === "your_turn" && s.reply) return firstLine(s.reply.text);
  if (s.prompt) return firstLine(s.prompt.text);
  return "";
}

function card(s) {
  const where = [
    place(s.cwd),
    s.model,
    s.kind === "background" ? `bg ${s.short_id}` : null,
  ]
    .filter(Boolean)
    .join(" · ");
  const meta = [];
  if (s.context_pct !== null && s.context_pct !== undefined) {
    meta.push(
      h(
        "span",
        { class: "meta-item" },
        h(
          "span",
          { class: "meter", "data-level": meterLevel(s.context_pct) },
          meterFill(s.context_pct),
        ),
        `context ${Math.round(s.context_pct)}%`,
      ),
    );
  }
  if (s.cost_usd) meta.push(fmtUsd(s.cost_usd));
  if (s.tool) meta.push(`tool ${fmtDuration(now() - s.tool.started_at)}`);

  return h(
    "button",
    {
      class: "session",
      type: "button",
      "data-activity": s.activity,
      onclick: () => openDetail(s.id),
    },
    h(
      "span",
      { class: "session-top" },
      badge(s.activity),
      h("span", {
        class: "since",
        "data-since": s.activity_since,
        text: fmtDuration(now() - s.activity_since),
      }),
    ),
    h("span", { class: "session-title", text: label(s) }),
    h("span", { class: "session-where", title: s.cwd, text: where }),
    sessionDetail(s)
      ? h("span", { class: "session-detail", text: sessionDetail(s) })
      : null,
    meta.length
      ? h(
          "span",
          { class: "session-meta" },
          ...meta.flatMap((m, i) =>
            i ? [h("span", { "aria-hidden": "true", text: "·" }), m] : [m],
          ),
        )
      : null,
  );
}

function tickClocks() {
  for (const el of document.querySelectorAll("[data-since]")) {
    el.textContent = fmtDuration(now() - Number(el.dataset.since));
  }
}

// --- detail dialog ----------------------------------------------------------

let detailTimer = null;

function scheduleDetailRefresh() {
  clearTimeout(detailTimer);
  detailTimer = setTimeout(
    () => state.detailId && loadDetail(state.detailId),
    400,
  );
}

function openDetail(id) {
  state.detailId = id;
  const dialog = $("#detail");
  dialog.replaceChildren(
    h("div", { class: "detail-head" }),
    h("div", { class: "detail-body" }),
  );
  renderDetailHead();
  if (!dialog.open) dialog.showModal();
  loadDetail(id);
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
      badge(s.activity),
      h("span", {
        class: "since",
        "data-since": s.activity_since,
        text: fmtDuration(now() - s.activity_since),
      }),
    ),
    h("h2", { id: "detail-title", text: label(s) }),
    h("div", { class: "session-where", title: s.cwd, text: s.cwd }),
    actions.length ? h("div", { class: "detail-actions" }, actions) : null,
  );
}

async function loadDetail(id) {
  let detail;
  try {
    detail = await getJson(`sessions/${encodeURIComponent(id)}?limit=80`);
  } catch (e) {
    if (e instanceof Unauthorized) return askToken();
    return;
  }
  if (state.detailId !== id) return;
  const s = detail.session;
  const body = $("#detail .detail-body");
  if (!body) return;
  const sections = [];
  if (s.waiting_for)
    sections.push(
      h("h3", { text: "Waiting for" }),
      h("p", { text: s.waiting_for }),
    );
  if (s.tool && s.activity !== "permission") {
    sections.push(
      h("h3", { text: "Running" }),
      h("p", { class: "mono", text: `${s.tool.name}: ${s.tool.summary}` }),
    );
  }
  if (detail.reply_html)
    sections.push(
      h("h3", { text: `Last reply · ${fmtTime(s.reply.at)}` }),
      prose(detail.reply_html),
    );
  if (detail.prompt_html)
    sections.push(
      h("h3", { text: `Last prompt · ${fmtTime(s.prompt.at)}` }),
      prose(detail.prompt_html),
    );
  sections.push(h("h3", { text: "Details" }), facts(s));
  if (detail.events.length) {
    sections.push(
      h("h3", { text: "Recent activity" }),
      h("ul", { class: "events" }, [...detail.events].reverse().map(eventItem)),
    );
  }
  body.replaceChildren(...sections);
}

function prose(html) {
  const el = h("div", { class: "prose" });
  // NOTE: server-rendered markdown with raw HTML escaped (src/markdown.rs)
  el.innerHTML = html;
  for (const a of el.querySelectorAll("a")) {
    a.target = "_blank";
    a.rel = "noopener noreferrer";
  }
  return el;
}

function facts(s) {
  const rows = [
    ["Model", s.model],
    ["Permission mode", s.permission_mode],
    [
      "Context",
      s.context_pct !== null && s.context_pct !== undefined
        ? `${Math.round(s.context_pct)}%`
        : null,
    ],
    ["Cost", s.cost_usd ? `${fmtUsd(s.cost_usd)} (API-equivalent)` : null],
    [
      "Lines",
      s.lines_added !== null && s.lines_added !== undefined
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
      `${fmtTime(s.started_at)} (${fmtDuration(now() - s.started_at)} ago)`,
    ],
    ["Host", s.host],
    ["Session id", s.id],
  ].filter(([, value]) => value);
  return h(
    "dl",
    { class: "facts" },
    rows.flatMap(([k, v]) => [h("dt", { text: k }), h("dd", { text: v })]),
  );
}

function describeEvent(e) {
  const d = e.data || {};
  switch (e.kind) {
    case "prompt":
      return `Prompt: ${firstLine(d.text)}`;
    case "reply":
      return `Reply: ${firstLine(d.text)}`;
    case "tool":
      return `${d.ok === false ? "✗ " : ""}${d.name}${d.summary ? `: ${d.summary}` : ""}${d.ms !== null && d.ms !== undefined ? ` (${fmtDuration(d.ms)})` : ""}`;
    case "permission":
      return `Asked permission: ${d.tool}${d.summary ? ` ${d.summary}` : ""}`;
    case "activity":
      return `→ ${ACTIVITY[d.to]?.label ?? d.to}`;
    case "notification":
      return d.message || d.type;
    case "start":
      return `Session started (${d.source})`;
    case "end":
      return `Session ended (${d.reason})`;
    case "compact":
      return "Context compacted";
    case "error":
      return `Error: ${d.error}`;
    default:
      return e.kind;
  }
}

function eventItem(e) {
  return h(
    "li",
    {},
    h("time", { text: fmtTime(e.ts) }),
    h("span", { text: describeEvent(e) }),
  );
}

// --- history view -----------------------------------------------------------

function rangeSince(range) {
  const t = now();
  if (range === "24h") return t - 86_400_000;
  if (range === "7d") return t - 7 * 86_400_000;
  const midnight = new Date(t);
  midnight.setHours(0, 0, 0, 0);
  return midnight.getTime();
}

async function loadHistory() {
  const container = $("#history");
  container.classList.add("loading");
  try {
    state.history = await getJson(`history?since=${rangeSince(state.range)}`);
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
    statTile("Sessions", data.lanes.length, null),
    statTile(
      "API-equivalent cost",
      fmtUsd(data.cost_usd),
      "from statusline totals",
    ),
  );
  $("#history").replaceChildren(tiles, usageChart(data), timelineChart(data));
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

function timeTicks(since, until, width) {
  const steps = [15, 30, 60, 120, 180, 360, 720, 1440].map((m) => m * 60_000);
  const maxTicks = Math.max(2, Math.floor(width / 80));
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
        ...lines.map((l) => h("div", { class: "muted", text: l })),
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

function usageChart(data) {
  const figure = h(
    "figure",
    { class: "chart" },
    h(
      "figcaption",
      {},
      h("div", { class: "chart-title", text: "5-hour limit used" }),
      h("div", {
        class: "chart-sub",
        text: `Highest reading per ${fmtDuration(data.bucket_ms)}`,
      }),
    ),
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
    pad.left + ((t - data.since) / (data.now - data.since)) * plotW;
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
  const { ticks, daily } = timeTicks(data.since, data.now, plotW);
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
  figure.append(svg);
  const tip = tooltipFor(figure);
  hit.addEventListener("pointermove", (event) => {
    const box = svg.getBoundingClientRect();
    const t =
      data.since +
      (((event.clientX - box.left) * (width / box.width) - pad.left) / plotW) *
        (data.now - data.since);
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
  const figure = h(
    "figure",
    { class: "chart" },
    h(
      "figcaption",
      {},
      h("div", { class: "chart-title", text: "Session activity" }),
      h("div", {
        class: "chart-sub",
        text: "When each session was working or waiting on you",
      }),
    ),
    h(
      "div",
      { class: "legend" },
      ["working", "permission", "your_turn", "idle"].map((a) =>
        h(
          "span",
          { "data-activity": a },
          h("i", { "aria-hidden": "true" }),
          ACTIVITY[a].label,
        ),
      ),
    ),
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
    ((Math.max(t, data.since) - data.since) / (data.now - data.since)) * plotW;

  const svg = s("svg", {
    viewBox: `0 0 ${width} ${height}`,
    height,
    role: "img",
    "aria-label": "Session activity timeline",
  });
  const { ticks, daily } = timeTicks(data.since, data.now, plotW);
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
    const name = `${lane.label} · ${place(lane.cwd)}`;
    svg.append(
      s("text", {
        class: "lane-label",
        x: pad.left,
        y: top + 13,
        text: name.length > maxChars ? `${name.slice(0, maxChars - 1)}…` : name,
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
  if (view === "history") loadHistory();
}

function setRange(range) {
  state.range = range;
  for (const button of document.querySelectorAll("[data-range]")) {
    button.setAttribute("aria-checked", String(button.dataset.range === range));
  }
  loadHistory();
}

function start() {
  takeTokenFromHash();
  applyTheme();
  $("#theme").addEventListener("click", cycleTheme);
  for (const tab of document.querySelectorAll("[data-view]")) {
    tab.addEventListener("click", () => showView(tab.dataset.view));
  }
  for (const button of document.querySelectorAll("[data-range]")) {
    button.addEventListener("click", () => setRange(button.dataset.range));
  }
  $("#detail").addEventListener("close", () => {
    state.detailId = null;
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

  setInterval(tickClocks, 1000);
  setInterval(scheduleRender, 15_000);
  setInterval(reconnectIfStale, 10_000);
  setInterval(() => !$("#view-history").hidden && loadHistory(), 60_000);
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") reconnectIfStale();
  });

  showView(localStorage.getItem(VIEW_KEY) || "sessions");
  if (!token()) return askToken();
  runStream();
}

start();
