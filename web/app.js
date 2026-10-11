const TOKEN_KEY = "ccdash.token";
const THEME_KEY = "ccdash.theme";
const VIEW_KEY = "ccdash.view";
const STALE_STREAM_MS = 45_000;
const STRIP_MS = 60 * 60_000;
const PAGE = 10;

const ACTIVITY = {
  permission: { label: "Needs permission", rank: 0 },
  your_turn: { label: "Your turn", rank: 1 },
  working: { label: "Working", rank: 2 },
  idle: { label: "Idle", rank: 3 },
  gone: { label: "Ended", rank: 4 },
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
const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;
const known = (v) => v !== null && v !== undefined;

function place(cwd) {
  const parts = (cwd || "").split("/").filter(Boolean);
  return parts.slice(-2).join("/") || cwd || "";
}

const label = (s) => s.title || s.name || s.id.slice(0, 8);

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
  loadRecent();
  if (state.detailId) scheduleDetailRefresh();
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

const clock = (since) =>
  h("span", {
    class: "since",
    "data-since": since,
    text: fmtDuration(now() - since),
  });

function card(s) {
  const where = [
    place(s.cwd),
    s.model,
    s.kind === "background" ? `bg ${s.short_id}` : null,
  ]
    .filter(Boolean)
    .join(" · ");
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
      clock(s.activity_since),
    ),
    h("span", { class: "session-title", text: label(s) }),
    h("span", { class: "session-where", title: s.cwd, text: where }),
    s.hooked || s.activity === "gone"
      ? [focusLine(s), taskBar(s.tasks), cardProgress(s), cardMeta(s)]
      : h("span", {
          class: "session-note",
          text: "Started before ccdash was installed, so only busy and idle are known. Resume it to see details.",
        }),
    strip(s),
  );
}

/** The one line that matters most for the session's state. */
function focusLine(s) {
  if (s.waiting_for)
    return h("span", { class: "session-focus mono", text: s.waiting_for });
  if (s.activity === "working" && s.tool)
    return h(
      "span",
      { class: "session-tool" },
      h("span", { class: "mono", text: toolLabel(s.tool) }),
      clock(s.tool.started_at),
    );
  const [who, text] =
    s.activity === "working" || !s.reply
      ? ["You", s.prompt?.text]
      : ["Claude", s.reply.text];
  const preview = plain(text);
  if (!preview) return null;
  return h(
    "span",
    { class: "session-detail" },
    h("span", { class: "who", text: who }),
    " ",
    preview,
  );
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

function cardProgress(s) {
  const items = [
    ...(s.activity === "gone" ? [] : turnFacts(s)),
    s.subagents.length ? plural(s.subagents.length, "subagent") : null,
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
    if (last && last[2] === s.activity) last[1] = t1;
    else segs.push([from, t1, s.activity]);
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
  state.convo = { id, exchanges: [], more: false, stats: null, loaded: false };
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
      badge(s.activity),
      clock(s.activity_since),
    ),
    h("h2", { id: "detail-title", text: label(s) }),
    h("div", { class: "session-where", title: s.cwd, text: s.cwd }),
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
  }
  for (const d of body.querySelectorAll("details[data-key]")) {
    if (openKeys.has(d.dataset.key)) d.open = true;
  }
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
    h(
      "ol",
      { class: "steps" },
      omitted
        ? h("li", {
            class: "muted",
            text: `${plural(omitted, "earlier tool call")} not shown`,
          })
        : null,
      items.map(step),
    ),
  );
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
    state.history = await getJson(
      `history?since=${Math.round(rangeSince(state.range))}`,
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
  const steps = [15, 30, 60, 120, 180, 360, 720, 1440].map((m) => m * 60_000);
  const maxTicks = Math.max(2, Math.floor(width / 110));
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
function barRows(figure, rows, { valueText, tipText }) {
  const width = chartWidth();
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
    svg.append(
      s("text", {
        class: "lane-label",
        x: pad.left,
        y: top + 13,
        text: ellipsize(`${lane.label} · ${place(lane.cwd)}`, maxChars),
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
  else loadRecent();
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

  setInterval(tickClocks, 1000);
  setInterval(scheduleRender, 15_000);
  setInterval(reconnectIfStale, 10_000);
  setInterval(() => {
    if (!$("#view-history").hidden) loadHistory();
    else loadRecent();
  }, 60_000);
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") reconnectIfStale();
  });

  showView(localStorage.getItem(VIEW_KEY) || "sessions");
  if (!token()) return askToken();
  runStream();
}

start();
