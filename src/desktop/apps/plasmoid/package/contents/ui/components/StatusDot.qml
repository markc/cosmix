import QtQuick

import org.kde.kirigami as Kirigami
import org.kde.plasma.components as PlasmaComponents3

// A small filled circle standing in for a status word. A tooltip is a pointer
// affordance only, so the word is also published to accessibility — otherwise a
// screen reader announces the unit and its buttons and never its state.
Rectangle {
    id: root

    required property string status

    implicitWidth: Math.round(Kirigami.Units.iconSizes.small / 2)
    implicitHeight: implicitWidth
    radius: implicitWidth / 2

    Accessible.role: Accessible.StaticText
    Accessible.name: root.status

    // the outline gives the fill an edge against a frame of similar tone; it is
    // not load-bearing, so its alpha stays low
    border.width: 1
    border.color: Qt.alpha(Kirigami.Theme.textColor, 0.4)

    color: {
        if (root.status === "active" || root.status === "ok") {
            return Kirigami.Theme.positiveTextColor;
        }
        if (root.status === "failed") {
            return Kirigami.Theme.negativeTextColor;
        }
        if (root.status === "changing" || root.status === "probing") {
            return Kirigami.Theme.neutralTextColor;
        }
        // NOT disabledTextColor: an eight-pixel dot in the de-emphasised
        // palette is unfindable in some schemes. Plain text colour makes a
        // stopped daemon exactly as legible as the unit name beside it.
        return Kirigami.Theme.textColor;
    }

    HoverHandler {
        id: dotHover
    }

    PlasmaComponents3.ToolTip {
        text: root.status
        visible: dotHover.hovered
    }
}
