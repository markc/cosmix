import QtQuick
import QtQuick.Layouts

import org.kde.kirigami as Kirigami

MouseArea {
    id: root

    signal activated()

    Layout.minimumWidth: Kirigami.Units.iconSizes.small
    Layout.minimumHeight: Kirigami.Units.iconSizes.small
    Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
    Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium

    hoverEnabled: true
    onClicked: root.activated()

    Kirigami.Icon {
        anchors.fill: parent
        anchors.margins: Math.max(0, Math.round(Kirigami.Units.smallSpacing / 2))
        source: Qt.resolvedUrl("../images/cosmix.svg")
        active: root.containsMouse
    }
}
