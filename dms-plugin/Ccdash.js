.pragma library

// Pure helpers shared by every surface of the plugin. Session objects are the
// ones ccdash sends over api.sock (see src/model.rs).

const ACTIVITY = {
    permission: { label: "Needs permission", rank: 0 },
    your_turn: { label: "Your turn", rank: 1 },
    working: { label: "Working", rank: 2 },
    idle: { label: "Idle", rank: 3 },
    finished: { label: "Finished", rank: 4 },
    gone: { label: "Ended", rank: 5 }
};

function isAgent(s) {
    return s.kind === "background";
}

/** Background sessions answer to their parent: a done turn is "finished". */
function shown(s) {
    return isAgent(s) && s.activity === "your_turn" ? "finished" : s.activity;
}

function activityLabel(s) {
    return (ACTIVITY[shown(s)] || ACTIVITY.idle).label;
}

function needsYou(s) {
    const a = shown(s);
    return a === "permission" || a === "your_turn";
}

function label(s) {
    return s.title || s.name || s.id.slice(0, 8);
}

function place(cwd) {
    const parts = (cwd || "").split("/").filter(p => p);
    return parts.slice(-2).join("/") || cwd || "";
}

/** Short-lived sessions that never worked (the `--resume` picker). */
function stub(s) {
    return s.activity === "gone" && !s.prompt && s.activity_since - s.started_at < 30000;
}

/** Sessions worth listing, needs-you first, then by how long they have waited. */
function visible(sessions, workspace) {
    return (sessions || [])
        .filter(s => s.activity !== "gone" && !stub(s))
        .filter(s => !workspace || s.workspace === workspace)
        .sort((a, b) => ACTIVITY[shown(a)].rank - ACTIVITY[shown(b)].rank || a.activity_since - b.activity_since);
}

function counts(list) {
    const out = { needs: 0, permission: 0, working: 0, idle: 0, agents: 0 };
    for (const s of list) {
        const a = shown(s);
        if (a === "permission")
            out.permission++;
        if (needsYou(s))
            out.needs++;
        else if (isAgent(s))
            out.agents++;
        else if (a === "working")
            out.working++;
        else
            out.idle++;
    }
    return out;
}

/** The session that has waited longest, permission prompts first. */
function longestWaiting(list) {
    return list.filter(needsYou).sort((a, b) => ACTIVITY[shown(a)].rank - ACTIVITY[shown(b)].rank || a.activity_since - b.activity_since)[0] || null;
}

function fmtDuration(ms) {
    const secs = Math.max(0, Math.round(ms / 1000));
    if (secs < 60)
        return secs + "s";
    const mins = Math.floor(secs / 60);
    if (mins < 60)
        return mins + "m";
    const hours = Math.floor(mins / 60);
    if (hours < 24)
        return hours + "h " + String(mins % 60).padStart(2, "0") + "m";
    return Math.floor(hours / 24) + "d " + (hours % 24) + "h";
}

function fmtBytes(bytes) {
    const gb = bytes / 1073741824;
    return gb >= 1 ? gb.toFixed(gb >= 10 ? 0 : 1) + " GB" : Math.round(bytes / 1048576) + " MB";
}

/** Rough plain text of markdown, for one-line previews. */
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
    return mcp ? mcp[1].split("_").pop() + ": " + mcp[2] : name;
}

function toolLabel(tool) {
    return tool.summary ? toolName(tool.name) + ": " + tool.summary : toolName(tool.name);
}

/**
 * What matters most right now, as `{kind, text}`: the pending request, the
 * running tool, or what Claude last said (`kind` is "wait", "tool", "claude"
 * or "you").
 */
function focus(s) {
    if (!s.hooked && s.activity !== "gone")
        return { kind: "note", text: "No hook data: started before ccdash was installed" };
    if (s.waiting_for)
        return { kind: "wait", text: s.waiting_for };
    if (s.activity === "working" && s.tool)
        return { kind: "tool", text: toolLabel(s.tool) };
    if (s.activity === "working" && s.message)
        return { kind: "claude", text: plain(s.message.text) };
    if (s.activity !== "working" && s.reply)
        return { kind: "claude", text: plain(s.reply.text) };
    return { kind: "you", text: plain(s.prompt ? s.prompt.text : "") };
}

function turnText(s, now) {
    const t = s.turn;
    if (!t)
        return "";
    const running = !t.ended_at && (s.activity === "working" || s.activity === "permission");
    const parts = [];
    if (running)
        parts.push("turn " + fmtDuration(now - t.started_at));
    else if (t.ended_at)
        parts.push("took " + fmtDuration(t.ended_at - t.started_at));
    if (t.tools)
        parts.push(t.tools + (t.tools === 1 ? " tool" : " tools"));
    if (t.failures)
        parts.push(t.failures + " failed");
    if (t.files.length)
        parts.push(t.files.length + (t.files.length === 1 ? " file" : " files"));
    if (s.subagents.length)
        parts.push(s.subagents.length + (s.subagents.length === 1 ? " subagent" : " subagents"));
    return parts.join(" · ");
}

/**
 * Whether a change from `prev` to `next` deserves a desktop notification:
 * "permission", "your_turn" or null. Finished background agents never do;
 * nothing fires while the session's own window has focus.
 */
function notifyKind(prev, next, focusedWindowId, prefs) {
    if (!prev || !next || prev.activity === next.activity)
        return null;
    const kind = shown(next);
    if (kind === "permission" && !prefs.permission)
        return null;
    if (kind === "your_turn" && !prefs.yourTurn)
        return null;
    if (kind !== "permission" && kind !== "your_turn")
        return null;
    if (prefs.unfocusedOnly && next.window && next.window.id === focusedWindowId)
        return null;
    return kind;
}

function notifyText(s, kind) {
    const title = (kind === "permission" ? "Needs permission: " : "Your turn: ") + label(s);
    const f = focus(s);
    const body = f.text.length > 220 ? f.text.slice(0, 219) + "…" : f.text;
    return { title: title, body: body || place(s.cwd) };
}

/** The web UI's state colors (light and dark steps of the same hues). */
function stateColor(activity, light) {
    const colors = {
        working: light ? "#2a78d6" : "#3987e5",
        permission: light ? "#eb6834" : "#d95926",
        your_turn: light ? "#1baf7a" : "#199e70"
    };
    return colors[activity] || (light ? "#c3c2b7" : "#6b6b66");
}

/**
 * Markdown safe for a Qt `Text.MarkdownText`: images become their alt text
 * (no remote fetches) and `<` outside code is escaped, so raw HTML shows as
 * text.
 */
function safeMarkdown(text) {
    return (text || "").split(/(```[\s\S]*?(?:```|$)|`[^`\n]*`)/).map((part, i) => {
        if (i % 2 === 1)
            return part;
        return part.replace(/!\[([^\]]*)\]\([^)]*\)/g, "$1").replace(/</g, "&lt;");
    }).join("");
}

function isWebLink(url) {
    return /^https?:\/\//i.test(url || "");
}
