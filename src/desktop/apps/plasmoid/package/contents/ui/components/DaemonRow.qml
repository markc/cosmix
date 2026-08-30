import QtQuick
import QtQuick.Layouts

import org.kde.kirigami as Kirigami
import org.kde.plasma.components as PlasmaComponents3

PlasmaComponents3.Frame {
    id: root

    required property var backend
    required property string manager
    required property string unit
    required property string status
    required property bool startEnabled
    required property bool stopEnabled

    Layout.fillWidth: true

    contentItem: RowLayout {
        spacing: Kirigami.Units.smallSpacing

        StatusDot {
            Layout.alignment: Qt.AlignVCenter
            status: root.status
        }

        PlasmaComponents3.Label {
            Layout.fillWidth: true
            Layout.alignment: Qt.AlignVCenter
            text: root.unit
            elide: Text.ElideMiddle
        }

        PlasmaComponents3.ToolButton {
            text: "Start"
            icon.name: "media-playback-start"
            display: PlasmaComponents3.AbstractButton.IconOnly
            enabled: root.startEnabled
            onClicked: root.backend.controlDaemon(root.manager, root.unit, "start")

            PlasmaComponents3.ToolTip {
                text: "Start"
            }
        }

        PlasmaComponents3.ToolButton {
            text: "Stop"
            icon.name: "media-playback-stop"
            display: PlasmaComponents3.AbstractButton.IconOnly
            enabled: root.stopEnabled
            onClicked: root.backend.controlDaemon(root.manager, root.unit, "stop")

            PlasmaComponents3.ToolTip {
                text: "Stop"
            }
        }

        PlasmaComponents3.ToolButton {
            text: "Restart"
            icon.name: "view-refresh"
            display: PlasmaComponents3.AbstractButton.IconOnly
            onClicked: root.backend.controlDaemon(root.manager, root.unit, "restart")

            PlasmaComponents3.ToolTip {
                text: "Restart"
            }
        }

        PlasmaComponents3.ToolButton {
            text: "Logs"
            icon.name: "utilities-log-viewer"
            display: PlasmaComponents3.AbstractButton.IconOnly
            onClicked: root.backend.openLogs(root.manager, root.unit)

            PlasmaComponents3.ToolTip {
                text: "Open logs"
            }
        }
    }
}
