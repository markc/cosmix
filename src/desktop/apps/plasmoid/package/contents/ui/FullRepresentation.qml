import QtQuick
import QtQuick.Layouts

import org.kde.kirigami as Kirigami
import org.kde.plasma.components as PlasmaComponents3

Item {
    id: root

    required property var backend
    property bool visibilityNotified: false

    signal representationShown()
    signal busVisibilityChanged(bool active)
    signal mixVisibilityChanged(bool active)

    function syncVisibility(): void {
        if (root.visible && !root.visibilityNotified) {
            root.visibilityNotified = true;
            root.representationShown();
        } else if (!root.visible) {
            root.visibilityNotified = false;
        }
        root.busVisibilityChanged(root.visible && tabs.currentIndex === 2);
        root.mixVisibilityChanged(root.visible && tabs.currentIndex === 1);
    }

    Layout.minimumWidth: Kirigami.Units.gridUnit * 20
    Layout.minimumHeight: Kirigami.Units.gridUnit * 20
    Layout.preferredWidth: Kirigami.Units.gridUnit * 23
    Layout.preferredHeight: Kirigami.Units.gridUnit * 30
    implicitWidth: Layout.preferredWidth
    implicitHeight: Layout.preferredHeight

    ColumnLayout {
        anchors.fill: parent
        spacing: Kirigami.Units.smallSpacing

        PlasmaComponents3.TabBar {
            id: tabs

            Layout.fillWidth: true
            currentIndex: 0
            onCurrentIndexChanged: root.syncVisibility()

            PlasmaComponents3.TabButton {
                text: "COS"
            }
            PlasmaComponents3.TabButton {
                text: "MIX"
            }
            PlasmaComponents3.TabButton {
                text: "BUS"
            }
            PlasmaComponents3.TabButton {
                text: "SSH"
            }
        }

        StackLayout {
            Layout.fillWidth: true
            Layout.fillHeight: true
            currentIndex: tabs.currentIndex

            CosTab {
                backend: root.backend
            }

            MixTab {
                backend: root.backend
            }

            BusTab {
                backend: root.backend
            }

            SshTab {
                backend: root.backend
            }
        }
    }

    onVisibleChanged: root.syncVisibility()
    Component.onCompleted: root.syncVisibility()
}
