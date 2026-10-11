import QtQuick
import Quickshell
import qs.Common
import qs.Services
import qs.Widgets
import qs.Modules.Plugins
import "Ccdash.js" as Cc

// Bar pill: how many sessions need you (highlighted), how many work. Left
// click lists them, right click opens the dash tab.
PluginComponent {
    id: root

    property var popoutService: null

    readonly property var shared: PluginService.globalVars[pluginId || "ccdash"] || ({})
    readonly property var daemon: shared.daemon || null
    readonly property string workspace: shared.workspace || ""

    function setWorkspace(name) {
        PluginService.setGlobalVar(pluginId || "ccdash", "workspace", name);
    }

    readonly property var st: shared.state || ({ connected: false, sessions: [], workspaces: [] })
    readonly property var list: Cc.visible(st.sessions, root.workspace)
    readonly property var counts: Cc.counts(list)
    readonly property bool showWorking: pluginData.pillShowsWorking ?? true
    readonly property bool animate: pluginData.animatePill ?? true
    readonly property color accent: counts.permission > 0 ? Cc.stateColor("permission", Theme.isLightMode) : Theme.primary
    property real now: Date.now()

    Timer {
        interval: 1000
        repeat: true
        running: root.interactionActive
        onTriggered: root.now = Date.now()
    }

    pillRightClickAction: (x, y, width, section, screen) => popoutService?.toggleDankDash("plugin_" + pluginId, x, y, width, section, screen)

    horizontalBarPill: Component {
        Row {
            spacing: Theme.spacingXS

            DankIcon {
                name: root.st.connected ? "asterisk" : "link_off"
                size: root.iconSize
                color: root.counts.needs > 0 ? root.accent : Theme.widgetIconColor
                opacity: root.st.connected ? 1 : 0.5

                SequentialAnimation on opacity {
                    running: root.animate && root.st.connected && root.counts.working > 0 && root.counts.needs === 0
                    loops: Animation.Infinite
                    alwaysRunToEnd: true
                    NumberAnimation { to: 0.55; duration: 900; easing.type: Easing.InOutSine }
                    NumberAnimation { to: 1; duration: 900; easing.type: Easing.InOutSine }
                }
                anchors.verticalCenter: parent.verticalCenter
            }

            StyledText {
                anchors.verticalCenter: parent.verticalCenter
                visible: root.counts.needs > 0
                text: root.counts.needs
                font.pixelSize: root.textSize
                font.weight: Font.Bold
                color: root.accent
            }

            StyledText {
                anchors.verticalCenter: parent.verticalCenter
                visible: root.showWorking && root.counts.working > 0
                text: (root.counts.needs > 0 ? "· " : "") + root.counts.working + " working"
                font.pixelSize: root.textSize
                color: Theme.widgetTextColor
            }
        }
    }

    verticalBarPill: Component {
        Column {
            spacing: Theme.spacingXXS

            DankIcon {
                name: root.st.connected ? "asterisk" : "link_off"
                size: root.iconSize
                color: root.counts.needs > 0 ? root.accent : Theme.widgetIconColor
                opacity: root.st.connected ? 1 : 0.5

                SequentialAnimation on opacity {
                    running: root.animate && root.st.connected && root.counts.working > 0 && root.counts.needs === 0
                    loops: Animation.Infinite
                    alwaysRunToEnd: true
                    NumberAnimation { to: 0.55; duration: 900; easing.type: Easing.InOutSine }
                    NumberAnimation { to: 1; duration: 900; easing.type: Easing.InOutSine }
                }
                anchors.horizontalCenter: parent.horizontalCenter
            }

            StyledText {
                anchors.horizontalCenter: parent.horizontalCenter
                visible: root.counts.needs > 0
                text: root.counts.needs
                font.pixelSize: root.textSize
                font.weight: Font.Bold
                color: root.accent
            }
        }
    }

    popoutContent: Component {
        PopoutComponent {
            id: popout

            headerText: "Claude Code"
            detailsText: !root.st.connected ? "ccdash is not running" : [root.counts.needs + " need you", root.counts.working + " working", root.counts.agents ? root.counts.agents + " background" : "", root.st.rateLimits?.five_hour ? "5-hour limit " + Math.round(root.st.rateLimits.five_hour.used_pct) + "%" : ""].filter(p => p).join(" · ")

            Item {
                width: parent.width
                implicitHeight: root.popoutHeight - popout.headerHeight - popout.detailsHeight - Theme.spacingXL

                StyledText {
                    anchors.centerIn: parent
                    width: parent.width - Theme.spacingXL * 2
                    visible: !root.st.connected || root.list.length === 0
                    horizontalAlignment: Text.AlignHCenter
                    wrapMode: Text.WordWrap
                    text: root.st.connected ? "No Claude Code sessions are running." : "Start it with: systemctl --user start ccdash"
                    color: Theme.surfaceVariantText
                }

                DankListView {
                    id: listView
                    anchors.top: parent.top
                    anchors.bottom: footer.top
                    anchors.bottomMargin: Theme.spacingS
                    width: parent.width
                    clip: true
                    spacing: 2
                    model: root.list

                    delegate: SessionRow {
                        required property var modelData
                        width: listView.width
                        session: modelData
                        now: root.now
                        onActivated: {
                            root.daemon?.open(modelData.id);
                            popout.closePopout();
                        }
                    }
                }

                Row {
                    id: footer
                    anchors.bottom: parent.bottom
                    anchors.right: parent.right
                    spacing: Theme.spacingS

                    DankButton {
                        text: "Dashboard"
                        iconName: "dashboard"
                        backgroundColor: Theme.secondaryContainer
                        textColor: Theme.onSecondaryContainer
                        onClicked: {
                            popout.closePopout();
                            Quickshell.execDetached(["dms", "ipc", "call", "dash", "open", "plugin_" + root.pluginId]);
                        }
                    }

                    DankButton {
                        text: "Web UI"
                        iconName: "open_in_new"
                        onClicked: {
                            root.daemon?.openWeb("");
                            popout.closePopout();
                        }
                    }
                }
            }
        }
    }

    popoutWidth: 460
    popoutHeight: Math.min(640, 190 + Math.max(1, list.length) * 74)
}
