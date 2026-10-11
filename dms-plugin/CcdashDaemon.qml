import QtQuick
import Quickshell
import Quickshell.Io
import qs.Common
import qs.Services
import qs.Widgets
import qs.Modules.Plugins
import "Ccdash.js" as Cc

// The one connection to ccdash: subscribes on api.sock, publishes the live
// state to the other surfaces (global var "state"), sends notifications and
// answers `dms ipc call ccdash …`. Surfaces call its actions through the
// global var "daemon".
PluginComponent {
    id: root

    property var popoutService: null

    readonly property string socketPath: (pluginData.socketPath || "").trim() || Quickshell.env("XDG_RUNTIME_DIR") + "/ccdash/api.sock"
    readonly property string command: (pluginData.command || "").trim() || "ccdash"
    readonly property string webUrl: (pluginData.webUrl || "").trim() || "http://127.0.0.1:7837/"
    readonly property var notifyPrefs: ({
            permission: pluginData.notifyPermission ?? true,
            yourTurn: pluginData.notifyYourTurn ?? true,
            unfocusedOnly: pluginData.notifyUnfocusedOnly ?? true
        })
    readonly property string iconPath: Qt.resolvedUrl("icon.svg").toString().replace("file://", "")

    property var sessions: ({})
    property var rateLimits: null
    property var workspaces: []
    property string host: ""
    property var lastNotified: ({})

    readonly property string pid: pluginId || "ccdash"

    DankSocket {
        id: socket
        path: root.socketPath
        connected: true

        onConnectionStateChanged: {
            if (linkUp)
                send({ cmd: "subscribe" });
            publishTimer.restart();
        }

        parser: SplitParser {
            onRead: line => root.receive(line)
        }
    }

    Timer {
        id: publishTimer
        interval: 120
        onTriggered: root.publish()
    }

    function receive(line) {
        let msg;
        try {
            msg = JSON.parse(line);
        } catch (e) {
            console.warn("ccdash: bad line from api.sock:", e);
            return;
        }
        switch (msg.type) {
        case "snapshot":
            {
                const byId = {};
                for (const s of msg.sessions || [])
                    byId[s.id] = s;
                sessions = byId;
            }
            rateLimits = msg.rate_limits || null;
            workspaces = msg.workspaces || [];
            host = msg.host || "";
            break;
        case "session":
            {
                const prev = sessions[msg.session.id];
                sessions[msg.session.id] = msg.session;
                maybeNotify(prev, msg.session);
                break;
            }
        case "removed":
            delete sessions[msg.id];
            break;
        case "rate_limits":
            rateLimits = msg.rate_limits;
            break;
        default:
            return;
        }
        publishTimer.restart();
    }

    function publish() {
        PluginService.setGlobalVar(pid, "state", {
            connected: socket.linkUp,
            host: host,
            sessions: Object.values(sessions),
            rateLimits: rateLimits,
            workspaces: workspaces,
            updatedAt: Date.now()
        });
    }

    function maybeNotify(prev, next) {
        const kind = Cc.notifyKind(prev, next, NiriService.lastFocusedWindowId, notifyPrefs);
        if (!kind)
            return;
        const key = next.id + ":" + kind;
        if (Date.now() - (lastNotified[key] || 0) < 30000)
            return;
        lastNotified[key] = Date.now();
        const text = Cc.notifyText(next, kind);
        const proc = notifier.createObject(root, {
            sessionId: next.id,
            command: ["notify-send", "--app-name=Claude Code", "--icon=" + iconPath, "--action=default=Focus", "--action=focus=Focus", text.title, text.body]
        });
        proc.running = true;
    }

    Component {
        id: notifier

        Process {
            property string sessionId

            stdout: StdioCollector {
                onStreamFinished: {
                    if (["default", "focus"].includes(text.trim()))
                        root.open(sessionId);
                }
            }

            onExited: destroy()
        }
    }

    // --- actions, shared with the other surfaces ---

    /** Focuses the session's terminal, or attaches to a background session. */
    function open(id) {
        Proc.runCommand("ccdash.open." + id, [command, "open", id], (out, code) => {
            if (code !== 0)
                ToastService.showWarning("ccdash", "No window is known for this session. Open it from the web UI.");
        }, 0);
    }

    function openWeb(id) {
        const base = webUrl.endsWith("/") ? webUrl : webUrl + "/";
        Qt.openUrlExternally(id ? base + "#session=" + encodeURIComponent(id) : base);
    }

    function openRemote(session) {
        if (session && session.remote_url)
            Qt.openUrlExternally(session.remote_url);
    }

    function focusWaiting() {
        const target = Cc.longestWaiting(Cc.visible(Object.values(sessions), null));
        if (!target)
            return "nothing is waiting on you";
        open(target.id);
        return "opening " + Cc.label(target);
    }

    IpcHandler {
        target: "ccdash"

        function focusWaiting(): string {
            return root.focusWaiting();
        }

        function open(id: string): string {
            root.open(id);
            return "opening " + id;
        }

        function status(): string {
            const c = Cc.counts(Cc.visible(Object.values(root.sessions), null));
            return JSON.stringify({ connected: socket.linkUp, needs_you: c.needs, working: c.working, agents: c.agents, idle: c.idle });
        }
    }

    onPidChanged: {
        PluginService.setGlobalVar(pid, "daemon", root);
        publish();
    }

    Component.onCompleted: {
        PluginService.setGlobalVar(pid, "daemon", root);
        publish();
    }

    Component.onDestruction: PluginService.setGlobalVar(pid, "daemon", null)
}
