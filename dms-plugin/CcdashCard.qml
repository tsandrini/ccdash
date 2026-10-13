import QtQuick
import qs.Common
import qs.Services
import qs.Widgets
import qs.Modules.Plugins
import "Ccdash.js" as Cc

// Overview card: how many sessions need you, how many work, the 5-hour
// limit. A click opens the tab.
DashCardComponent {
    id: root

    readonly property var shared: PluginService.globalVars[pluginId || "ccdash"] || ({})
    readonly property var daemon: shared.daemon || null
    readonly property string workspace: shared.workspace || ""

    function setWorkspace(name) {
        PluginService.setGlobalVar(pluginId || "ccdash", "workspace", name);
    }

    readonly property var st: shared.state || ({ connected: false, sessions: [], workspaces: [] })
    readonly property var counts: Cc.counts(Cc.visible(st.sessions, root.workspace))
    readonly property var limit: st.rateLimits ? st.rateLimits.five_hour : null

    clickable: true

    Column {
        anchors.verticalCenter: parent.verticalCenter
        width: parent.width
        spacing: Theme.spacingXS

        Row {
            spacing: Theme.spacingS

            StyledText {
                anchors.baseline: needsLabel.baseline
                text: root.st.connected ? root.counts.needs : "–"
                font.pixelSize: Theme.fontSizeXXLarge
                font.weight: Font.DemiBold
                color: root.counts.permission > 0 ? Cc.stateColor("permission", Theme.isLightMode) : root.counts.needs > 0 ? root.accentColor : root.contentColor
            }

            StyledText {
                id: needsLabel
                anchors.verticalCenter: parent.verticalCenter
                text: root.st.connected ? (root.counts.needs === 1 ? "session needs you" : "sessions need you") : "ccdash not running"
                font.pixelSize: Theme.fontSizeMedium
                color: root.contentColor
            }
        }

        StyledText {
            width: parent.width
            text: [root.counts.working + " working", root.counts.agents ? root.counts.agents + " background" : "", root.limit ? "5-hour " + Math.round(root.limit.used_pct) + "%" : "", ...Cc.scopedLimits(root.st.rateLimits)].filter(p => p).join(" · ")
            font.pixelSize: Theme.fontSizeSmall
            color: root.mutedColor
            elide: Text.ElideRight
        }

        Rectangle {
            visible: root.limit !== null
            width: parent.width
            height: 4
            radius: 2
            color: root.chipColor

            Rectangle {
                width: parent.width * Math.min(1, (root.limit ? root.limit.used_pct : 0) / 100)
                height: parent.height
                radius: 2
                color: root.limit && root.limit.used_pct >= 90 ? Theme.error : root.limit && root.limit.used_pct >= 75 ? Theme.warning : root.accentColor
            }
        }
    }
}
