pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Layouts

import org.kde.kirigami as Kirigami
import org.kde.plasma.components as PlasmaComponents3

ColumnLayout {
    id: root

    required property var backend
    required property string title
    required property var daemonModel
    required property string emptyText

    spacing: Kirigami.Units.smallSpacing

    SectionHeading {
        Layout.fillWidth: true
        text: root.title
    }

    PlasmaComponents3.Label {
        Layout.fillWidth: true
        visible: root.backend.snapshotReady && root.daemonModel.count === 0
        text: root.emptyText
        opacity: 0.7
    }

    Repeater {
        model: root.daemonModel

        delegate: DaemonRow {
            backend: root.backend
        }
    }
}
