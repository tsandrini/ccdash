import QtQuick
import Quickshell
import qs.Common
import qs.Services
import qs.Widgets
import qs.Modules.Plugins
import qs.Modules.DankDash
import qs.Modules.DankDash.Overview
import "Ccdash.js" as Cc

// The dash tab: sessions on the left, the selected one on the right.
// Keys: ↑/↓ or j/k select, Enter focuses its window, e (or F2) renames it,
// w opens it in the web UI, r opens it in Claude, [ and ] switch workspace.
DashTabComponent {
    id: root

    property int rowBudget: 6

    readonly property var shared: PluginService.globalVars[pluginId || "ccdash"] || ({})
    readonly property var daemon: shared.daemon || null
    readonly property string workspace: shared.workspace || ""

    function setWorkspace(name) {
        PluginService.setGlobalVar(pluginId || "ccdash", "workspace", name);
    }

    readonly property var st: shared.state || ({ connected: false, sessions: [], workspaces: [] })
    readonly property var list: Cc.visible(st.sessions, root.workspace)
    readonly property var rows: grouped(list)
    readonly property var selected: list.find(s => s.id === selectedId) || list[0] || null
    property string selectedId: ""
    readonly property bool editingTitle: detailColumn.editing
    property real now: Date.now()

    implicitHeight: Math.max(DashMetrics.tabMinHeight, DashMetrics.heightForRows(Math.min(Math.max(rowBudget, 4), 7)))
    focusTarget: sessionList

    Timer {
        interval: 1000
        repeat: true
        triggeredOnStart: true
        running: root.live
        onTriggered: root.now = Date.now()
    }

    function grouped(sessions) {
        const groups = [["Needs you", s => Cc.needsYou(s)], ["Working", s => !Cc.isAgent(s) && s.activity === "working"], ["Idle", s => !Cc.isAgent(s) && !Cc.needsYou(s) && s.activity !== "working"], ["Background agents", s => Cc.isAgent(s) && !Cc.needsYou(s)]];
        const out = [];
        for (const [title, test] of groups) {
            const members = sessions.filter(test);
            if (!members.length)
                continue;
            out.push({ header: title, count: members.length });
            for (const s of members)
                out.push({ session: s });
        }
        return out;
    }

    function move(step) {
        if (!list.length)
            return;
        const order = rows.filter(r => r.session).map(r => r.session.id);
        const at = Math.max(0, order.indexOf(selected ? selected.id : ""));
        selectedId = order[Math.min(order.length - 1, Math.max(0, at + step))];
        const index = rows.findIndex(r => r.session && r.session.id === selectedId);
        sessionList.positionViewAtIndex(index, ListView.Contain);
    }

    function cycleWorkspace(step) {
        const names = [""].concat(st.workspaces || []);
        const at = Math.max(0, names.indexOf(root.workspace));
        root.setWorkspace(names[(at + step + names.length) % names.length]);
    }

    function activate(session) {
        if (!session)
            return;
        root.daemon?.open(session.id);
        popoutService?.closeDankDash();
    }

    function restoreFocus() {
        sessionList.forceActiveFocus(Qt.OtherFocusReason);
    }

    blocksTabNavigation: detailColumn.editing

    function handleKeyEvent(event) {
        if (detailColumn.editing || event.modifiers & (Qt.ControlModifier | Qt.AltModifier))
            return false;
        switch (event.key) {
        case Qt.Key_Down:
        case Qt.Key_J:
            move(1);
            return true;
        case Qt.Key_Up:
        case Qt.Key_K:
            move(-1);
            return true;
        case Qt.Key_Home:
            move(-list.length);
            return true;
        case Qt.Key_End:
            move(list.length);
            return true;
        case Qt.Key_Return:
        case Qt.Key_Enter:
            activate(selected);
            return true;
        case Qt.Key_E:
        case Qt.Key_F2:
            detailColumn.startRename();
            return true;
        case Qt.Key_W:
            root.daemon?.openWeb(selected ? selected.id : "");
            return true;
        case Qt.Key_R:
            root.daemon?.openRemote(selected);
            return true;
        case Qt.Key_BracketLeft:
            cycleWorkspace(-1);
            return true;
        case Qt.Key_BracketRight:
            cycleWorkspace(1);
            return true;
        }
        return false;
    }

    Column {
        id: header
        width: parent.width
        spacing: Theme.spacingS

        Row {
            width: parent.width
            spacing: Theme.spacingS

            Repeater {
                model: (root.st.workspaces || []).length ? [""].concat(root.st.workspaces) : []

                StyledRect {
                    required property string modelData
                    readonly property bool current: root.workspace === modelData
                    width: chipText.implicitWidth + Theme.spacingL * 2
                    height: Theme.buttonHeightXS ?? 28
                    radius: height / 2
                    color: current ? Theme.secondaryContainer : "transparent"
                    border.width: current ? 0 : 1
                    border.color: Theme.outline

                    StyledText {
                        id: chipText
                        anchors.centerIn: parent
                        text: parent.modelData || "All"
                        font.pixelSize: Theme.fontSizeSmall
                        font.weight: parent.current ? Font.DemiBold : Font.Normal
                        color: parent.current ? Theme.onSecondaryContainer : Theme.surfaceText
                    }

                    MouseArea {
                        anchors.fill: parent
                        cursorShape: Qt.PointingHandCursor
                        onClicked: root.setWorkspace(parent.modelData)
                    }
                }
            }

            DankButton {
                text: "Connect phone"
                iconName: "qr_code_2"
                buttonHeight: Theme.buttonHeightXS ?? 28
                backgroundColor: Theme.secondaryContainer
                textColor: Theme.onSecondaryContainer
                onClicked: root.daemon?.openPair()
            }
        }
    }

    Item {
        anchors.top: header.bottom
        anchors.topMargin: header.visible ? Theme.spacingM : 0
        anchors.bottom: parent.bottom
        width: parent.width

        StyledText {
            anchors.centerIn: parent
            visible: !root.st.connected || root.list.length === 0
            text: root.st.connected ? "No Claude Code sessions here." : "ccdash is not running (systemctl --user start ccdash)."
            color: Theme.surfaceVariantText
        }

        DankListView {
            id: sessionList
            visible: root.list.length > 0
            width: Math.max(300, Math.min(parent.width * 0.42, 460))
            height: parent.height
            clip: true
            spacing: 2
            focus: true
            model: root.rows

            delegate: Loader {
                required property var modelData
                width: sessionList.width
                sourceComponent: modelData.header ? groupHeader : sessionRow

                Component {
                    id: groupHeader

                    StyledText {
                        topPadding: Theme.spacingS
                        bottomPadding: 2
                        leftPadding: Theme.spacingM
                        text: modelData.header.toUpperCase() + "  " + modelData.count
                        font.pixelSize: Theme.fontSizeSmall
                        font.weight: Font.DemiBold
                        color: Theme.surfaceVariantText
                    }
                }

                Component {
                    id: sessionRow

                    SessionRow {
                        session: modelData.session
                        now: root.now
                        selected: root.selected && root.selected.id === modelData.session.id
                        onActivated: {
                            if (selected)
                                root.activate(modelData.session);
                            else
                                root.selectedId = modelData.session.id;
                        }
                    }
                }
            }
        }

        Card {
            id: detail
            anchors.left: sessionList.right
            anchors.leftMargin: Theme.spacingM
            anchors.right: parent.right
            height: parent.height
            visible: root.selected !== null

            DankFlickable {
                anchors.fill: parent
                contentHeight: detailColumn.implicitHeight
                clip: true

                SessionDetail {
                    id: detailColumn
                    width: parent.width
                    session: root.selected
                    now: root.now
                    onFocusRequested: root.activate(root.selected)
                    onWebRequested: root.daemon?.openWeb(root.selected.id)
                    onRenameRequested: title => {
                        root.daemon?.rename(root.selected.id, title);
                        root.restoreFocus();
                    }
                    onRemoteRequested: root.daemon?.openRemote(root.selected)
                }
            }
        }
    }
}
