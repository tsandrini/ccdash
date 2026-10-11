import QtQuick
import qs.Common
import qs.Widgets
import "Ccdash.js" as Cc

// Everything about one session that fits without the conversation: what it
// waits for or runs, what Claude last said, tasks, subagents, the turn and
// resources, plus the actions.
Column {
    id: root

    property var session: null
    property real now: Date.now()

    signal focusRequested
    signal webRequested
    signal remoteRequested
    signal renameRequested(string title)

    property bool editing: false

    function startRename() {
        if (!session || session.activity === "gone")
            return;
        editing = true;
        titleField.text = Cc.label(session);
        titleField.forceActiveFocus();
    }

    onSessionChanged: if (!session) editing = false

    readonly property var s: session || ({})
    readonly property string activity: session ? Cc.shown(session) : "idle"
    readonly property string said: !session ? "" : session.activity === "working" && session.message ? session.message.text : session.reply ? session.reply.text : ""

    spacing: Theme.spacingM
    visible: session !== null

    Column {
        width: parent.width
        spacing: 2

        Row {
            spacing: Theme.spacingS

            Rectangle {
                anchors.verticalCenter: parent.verticalCenter
                width: 9
                height: 9
                radius: 4.5
                color: Cc.stateColor(root.activity, Theme.isLightMode)
            }

            StyledText {
                text: root.session ? Cc.activityLabel(root.session) + " · " + Cc.fmtDuration(root.now - root.s.activity_since) : ""
                font.pixelSize: Theme.fontSizeSmall
                font.weight: Font.DemiBold
                color: Theme.surfaceText
            }
        }

        Item {
            width: parent.width
            height: root.editing ? titleField.height : title.height
            visible: !root.editing

            StyledText {
                id: title
                width: Math.min(implicitWidth, parent.width - pencil.width - Theme.spacingS)
                text: root.session ? Cc.label(root.session) : ""
                font.pixelSize: Theme.fontSizeXLarge
                font.weight: Font.DemiBold
                color: Theme.surfaceText
                wrapMode: Text.WordWrap
                maximumLineCount: 2
                elide: Text.ElideRight
            }

            DankIcon {
                id: pencil
                anchors.left: title.right
                anchors.leftMargin: Theme.spacingS
                anchors.verticalCenter: title.verticalCenter
                visible: root.s.activity !== "gone"
                name: "edit"
                size: Theme.iconSizeSmall
                color: pencilMouse.containsMouse ? Theme.primary : Theme.surfaceVariantText

                MouseArea {
                    id: pencilMouse
                    anchors.fill: parent
                    anchors.margins: -6
                    hoverEnabled: true
                    cursorShape: Qt.PointingHandCursor
                    onClicked: root.startRename()
                }
            }
        }

        DankTextField {
            id: titleField
            width: parent.width
            visible: root.editing
            maximumLength: 100
            placeholderText: "Session title"
            onAccepted: {
                root.editing = false;
                root.renameRequested(text);
            }
            Keys.onEscapePressed: root.editing = false
        }

        StyledText {
            width: parent.width
            visible: !!root.s.renaming
            text: "Renamed here; Claude Code adopts the name at the session's next prompt."
            font.pixelSize: Theme.fontSizeSmall
            color: Theme.surfaceVariantText
            wrapMode: Text.WordWrap
        }

        StyledText {
            width: parent.width
            text: [root.s.cwd, root.s.model, root.s.effort ? root.s.effort + " effort" : ""].filter(p => p).join(" · ")
            font.pixelSize: Theme.fontSizeSmall
            color: Theme.surfaceVariantText
            wrapMode: Text.NoWrap
            elide: Text.ElideMiddle
        }
    }

    Row {
        spacing: Theme.spacingS

        DankButton {
            text: root.s.window || Cc.isAgent(root.s) ? "Focus" : "Focus (no window)"
            iconName: "select_window"
            enabled: !!root.s.window || Cc.isAgent(root.s)
            onClicked: root.focusRequested()
        }

        DankButton {
            text: "Conversation"
            iconName: "forum"
            backgroundColor: Theme.secondaryContainer
            textColor: Theme.onSecondaryContainer
            onClicked: root.webRequested()
        }

        DankButton {
            visible: !!root.s.remote_url
            text: "Open in Claude"
            iconName: "open_in_new"
            backgroundColor: Theme.secondaryContainer
            textColor: Theme.onSecondaryContainer
            onClicked: root.remoteRequested()
        }
    }

    StyledRect {
        width: parent.width
        visible: !!root.s.waiting_for || (root.s.activity === "working" && !!root.s.tool)
        height: focusText.implicitHeight + Theme.spacingS * 2
        radius: Theme.cornerRadius
        color: Theme.surfaceContainerHigh

        StyledText {
            id: focusText
            x: Theme.spacingM
            y: Theme.spacingS
            width: parent.width - Theme.spacingM * 2
            text: root.s.waiting_for ? root.s.waiting_for : root.s.tool ? Cc.toolLabel(root.s.tool) + "  ·  " + Cc.fmtDuration(root.now - root.s.tool.started_at) : ""
            isMonospace: true
            font.pixelSize: Theme.fontSizeSmall
            color: root.s.waiting_for ? Cc.stateColor("permission", Theme.isLightMode) : Theme.surfaceText
            wrapMode: Text.WrapAnywhere
            maximumLineCount: 4
            elide: Text.ElideRight
        }
    }

    StyledText {
        width: parent.width
        visible: text !== ""
        text: root.session ? [Cc.turnText(root.session, root.now), root.s.context_pct != null ? "context " + Math.round(root.s.context_pct) + "%" : "", root.s.cost_usd ? "$" + root.s.cost_usd.toFixed(2) : "", root.s.resources ? root.s.resources.cpu_pct + "% CPU · " + Cc.fmtBytes(root.s.resources.mem_bytes) : ""].filter(p => p).join("  ·  ") : ""
        font.pixelSize: Theme.fontSizeSmall
        color: Theme.surfaceVariantText
        wrapMode: Text.WordWrap
    }

    Column {
        width: parent.width
        spacing: 2
        visible: (root.s.tasks || []).length > 0

        StyledText {
            text: "TASKS  " + (root.s.tasks || []).filter(t => t.status === "completed").length + "/" + (root.s.tasks || []).length
            font.pixelSize: Theme.fontSizeSmall
            font.weight: Font.DemiBold
            color: Theme.surfaceVariantText
        }

        Repeater {
            model: root.s.tasks || []

            StyledText {
                required property var modelData
                width: root.width
                text: (modelData.status === "completed" ? "✓  " : modelData.status === "in_progress" ? "◐  " : "○  ") + (modelData.status === "in_progress" && modelData.active_form ? modelData.active_form : modelData.subject)
                font.pixelSize: Theme.fontSizeSmall
                font.weight: modelData.status === "in_progress" ? Font.DemiBold : Font.Normal
                color: modelData.status === "completed" ? Theme.surfaceVariantText : Theme.surfaceText
                wrapMode: Text.NoWrap
                elide: Text.ElideRight
            }
        }
    }

    Column {
        width: parent.width
        spacing: 2
        visible: (root.s.subagents || []).length > 0

        StyledText {
            text: "SUBAGENTS"
            font.pixelSize: Theme.fontSizeSmall
            font.weight: Font.DemiBold
            color: Theme.surfaceVariantText
        }

        Repeater {
            model: root.s.subagents || []

            StyledText {
                required property var modelData
                width: root.width
                text: modelData.agent_type + (modelData.description ? ": " + modelData.description : "") + "  ·  " + Cc.fmtDuration(root.now - modelData.started_at) + (modelData.tool ? "  ·  " + Cc.toolLabel(modelData.tool) : "")
                font.pixelSize: Theme.fontSizeSmall
                color: Theme.surfaceText
                wrapMode: Text.NoWrap
                elide: Text.ElideRight
            }
        }
    }

    Column {
        width: parent.width
        spacing: Theme.spacingXS
        visible: root.said !== ""

        StyledText {
            text: root.s.activity === "working" ? "CLAUDE, NOW" : "LAST REPLY"
            font.pixelSize: Theme.fontSizeSmall
            font.weight: Font.DemiBold
            color: Theme.surfaceVariantText
        }

        StyledText {
            width: parent.width
            text: Cc.safeMarkdown(root.said)
            textFormat: Text.MarkdownText
            wrapMode: Text.WordWrap
            font.pixelSize: Theme.fontSizeMedium
            color: Theme.surfaceText
            onLinkActivated: link => {
                if (Cc.isWebLink(link))
                    Qt.openUrlExternally(link);
            }
        }
    }
}
