import QtQuick
import qs.Common
import qs.Widgets
import qs.Modules.Plugins

PluginSettings {
    id: root
    pluginId: "ccdash"

    StyledText {
        width: parent.width
        text: "Notifications"
        font.pixelSize: Theme.fontSizeLarge
        font.weight: Font.Medium
        color: Theme.surfaceText
    }

    ToggleSetting {
        settingKey: "notifyPermission"
        label: "Permission requests"
        description: "Notify when a session waits for your permission"
        defaultValue: true
    }

    ToggleSetting {
        settingKey: "notifyYourTurn"
        label: "Finished turns"
        description: "Notify when a session finishes its turn and waits for you"
        defaultValue: true
    }

    ToggleSetting {
        settingKey: "notifyUnfocusedOnly"
        label: "Only for unfocused sessions"
        description: "Stay quiet while the session's terminal has focus"
        defaultValue: true
    }

    StyledText {
        width: parent.width
        text: "Bar"
        font.pixelSize: Theme.fontSizeLarge
        font.weight: Font.Medium
        color: Theme.surfaceText
    }

    ToggleSetting {
        settingKey: "pillShowsWorking"
        label: "Show working sessions"
        description: "Add \"N working\" next to the count of sessions that need you"
        defaultValue: true
    }

    ToggleSetting {
        settingKey: "animatePill"
        label: "Pulse while working"
        description: "Pulse the icon while sessions work and none needs you"
        defaultValue: true
    }

    StyledText {
        width: parent.width
        text: "Connection"
        font.pixelSize: Theme.fontSizeLarge
        font.weight: Font.Medium
        color: Theme.surfaceText
    }

    StringSetting {
        settingKey: "webUrl"
        label: "Web UI"
        description: "Opened by Conversation and Web UI"
        placeholder: "http://127.0.0.1:7837/"
        defaultValue: ""
    }

    StringSetting {
        settingKey: "socketPath"
        label: "API socket"
        description: "Leave empty for $XDG_RUNTIME_DIR/ccdash/api.sock"
        placeholder: "/run/user/1000/ccdash/api.sock"
        defaultValue: ""
    }

    StringSetting {
        settingKey: "command"
        label: "ccdash command"
        description: "Used to focus sessions (`ccdash open <id>`)"
        placeholder: "ccdash"
        defaultValue: ""
    }
}
