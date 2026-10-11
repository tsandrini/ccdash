import QtQuick
import qs.Common
import qs.Widgets
import "Ccdash.js" as Cc

// One session as a list row: state, title and age; where it runs; what
// matters now. Used by the popout and the dash tab.
Rectangle {
    id: row

    required property var session
    property real now: Date.now()
    property bool selected: false
    property bool compact: false

    signal activated
    signal hovered

    readonly property string activity: Cc.shown(session)
    readonly property var focusLine: Cc.focus(session)
    readonly property bool hot: mouse.containsMouse || selected

    implicitHeight: column.implicitHeight + Theme.spacingS * 2
    radius: Theme.cornerRadius
    color: selected ? Theme.withAlpha(Theme.primary, 0.14) : mouse.containsMouse ? Theme.surfaceTextHover : "transparent"

    Rectangle {
        id: dot
        x: Theme.spacingM
        y: Theme.spacingS + (title.height - height) / 2
        width: 9
        height: 9
        radius: 4.5
        color: Cc.stateColor(row.activity, Theme.isLightMode)
        border.width: row.activity === "idle" || row.activity === "finished" ? 1 : 0
        border.color: Theme.outline

        SequentialAnimation on opacity {
            running: row.activity === "working"
            loops: Animation.Infinite
            alwaysRunToEnd: true
            NumberAnimation { to: 0.35; duration: 800; easing.type: Easing.InOutSine }
            NumberAnimation { to: 1; duration: 800; easing.type: Easing.InOutSine }
        }
    }

    Column {
        id: column
        anchors.left: dot.right
        anchors.leftMargin: Theme.spacingS
        anchors.right: parent.right
        anchors.rightMargin: Theme.spacingM
        y: Theme.spacingS
        spacing: 2

        Item {
            width: parent.width
            height: title.height

            StyledText {
                id: title
                anchors.left: parent.left
                anchors.right: age.left
                anchors.rightMargin: Theme.spacingS
                text: Cc.label(row.session)
                font.pixelSize: Theme.fontSizeMedium
                font.weight: Font.DemiBold
                color: Theme.surfaceText
                wrapMode: Text.NoWrap
                elide: Text.ElideRight
            }

            StyledText {
                id: age
                anchors.right: parent.right
                anchors.verticalCenter: title.verticalCenter
                text: Cc.activityLabel(row.session) + " · " + Cc.fmtDuration(row.now - row.session.activity_since)
                font.pixelSize: Theme.fontSizeSmall
                color: Theme.surfaceVariantText
                wrapMode: Text.NoWrap
            }
        }

        StyledText {
            width: parent.width
            visible: !row.compact
            text: [Cc.place(row.session.cwd), row.session.model, Cc.isAgent(row.session) ? "background" : ""].filter(p => p).join(" · ")
            font.pixelSize: Theme.fontSizeSmall
            color: Theme.surfaceVariantText
            wrapMode: Text.NoWrap
            elide: Text.ElideRight
        }

        StyledText {
            width: parent.width
            visible: row.focusLine.text !== ""
            text: (row.focusLine.kind === "claude" ? "Claude: " : row.focusLine.kind === "you" ? "You: " : "") + row.focusLine.text
            isMonospace: row.focusLine.kind === "tool" || row.focusLine.kind === "wait"
            font.pixelSize: Theme.fontSizeSmall
            color: row.focusLine.kind === "wait" ? Cc.stateColor("permission", Theme.isLightMode) : row.focusLine.kind === "note" ? Theme.surfaceVariantText : Theme.surfaceText
            wrapMode: Text.Wrap
            maximumLineCount: 2
            elide: Text.ElideRight
        }
    }

    MouseArea {
        id: mouse
        anchors.fill: parent
        hoverEnabled: true
        cursorShape: Qt.PointingHandCursor
        onClicked: row.activated()
        onEntered: row.hovered()
    }
}
