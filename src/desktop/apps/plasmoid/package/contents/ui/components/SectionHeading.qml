import QtQuick
import QtQuick.Layouts

import org.kde.kirigami as Kirigami

RowLayout {
    id: root

    required property string text

    spacing: Kirigami.Units.smallSpacing

    Kirigami.Heading {
        text: root.text
        level: 3
    }

    Rectangle {
        Layout.fillWidth: true
        implicitHeight: 1
        color: Kirigami.Theme.textColor
        opacity: 0.18
    }
}
