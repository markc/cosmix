pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Layouts

import org.kde.kirigami as Kirigami
import org.kde.plasma.components as PlasmaComponents3

import "components"

Item {
    id: root

    required property var backend
    property bool rosterExpanded: true

    function directionValue(index: int): string {
        return ["all", "local", "mesh_in", "mesh_out"][index];
    }

    function applyFilter(): void {
        root.backend.applyBusFilter(
            root.directionValue(directionFilter.currentIndex),
            verbFilter.text,
            bodyFilter.currentIndex === 0 ? "none" : "redacted");
    }

    PlasmaComponents3.ScrollView {
        id: scrollView

        anchors.fill: parent
        contentWidth: availableWidth

        ColumnLayout {
            width: scrollView.availableWidth
            spacing: Kirigami.Units.smallSpacing

            PlasmaComponents3.Frame {
                Layout.fillWidth: true

                contentItem: RowLayout {
                    spacing: Kirigami.Units.smallSpacing

                    Rectangle {
                        Layout.preferredWidth: Kirigami.Units.smallSpacing
                        Layout.preferredHeight: width
                        radius: width / 2
                        color: {
                            if (root.backend.busState === "connected") {
                                return Kirigami.Theme.positiveTextColor;
                            }
                            if (root.backend.busState === "degraded"
                                    || root.backend.busState === "unavailable") {
                                return Kirigami.Theme.negativeTextColor;
                            }
                            return Kirigami.Theme.neutralTextColor;
                        }
                    }

                    ColumnLayout {
                        Layout.fillWidth: true
                        spacing: 0

                        PlasmaComponents3.Label {
                            Layout.fillWidth: true
                            text: root.backend.busObserving
                                ? "Bus observation live"
                                : "Bus " + root.backend.busState
                            font.bold: true
                        }

                        PlasmaComponents3.Label {
                            Layout.fillWidth: true
                            text: root.backend.busSessionOpen
                                ? (root.backend.busPaused
                                    ? "Display paused; capture continues"
                                    : "Metadata stream")
                                : "Opening observation lease"
                            opacity: 0.7
                            font: Kirigami.Theme.smallFont
                        }
                    }

                    PlasmaComponents3.BusyIndicator {
                        running: root.backend.busBusy
                        visible: running
                        Layout.preferredWidth: Kirigami.Units.iconSizes.small
                        Layout.preferredHeight: Kirigami.Units.iconSizes.small
                    }

                    PlasmaComponents3.ToolButton {
                        text: root.backend.busPaused ? "Resume" : "Pause"
                        icon.name: root.backend.busPaused
                            ? "media-playback-start"
                            : "media-playback-pause"
                        display: PlasmaComponents3.AbstractButton.IconOnly
                        enabled: root.backend.busSessionOpen
                        onClicked: root.backend.setBusPaused(!root.backend.busPaused)

                        PlasmaComponents3.ToolTip {
                            text: root.backend.busPaused
                                ? "Resume from the latest bounded snapshot"
                                : "Pause displayed traffic"
                        }
                    }

                    PlasmaComponents3.ToolButton {
                        text: "Refresh roster"
                        icon.name: "view-refresh"
                        display: PlasmaComponents3.AbstractButton.IconOnly
                        enabled: root.backend.busSessionOpen
                        onClicked: root.backend.refreshBusRoster()

                        PlasmaComponents3.ToolTip {
                            text: "Refresh node inventory"
                        }
                    }
                }
            }

            Kirigami.InlineMessage {
                Layout.fillWidth: true
                visible: root.backend.connectionError.length > 0
                type: Kirigami.MessageType.Error
                text: root.backend.connectionError
            }

            Kirigami.InlineMessage {
                Layout.fillWidth: true
                visible: root.backend.busError.length > 0
                type: Kirigami.MessageType.Warning
                text: root.backend.busError
            }

            ColumnLayout {
                Layout.fillWidth: true
                spacing: Kirigami.Units.smallSpacing

                RowLayout {
                    Layout.fillWidth: true

                    PlasmaComponents3.ComboBox {
                        id: directionFilter

                        Layout.preferredWidth: Kirigami.Units.gridUnit * 7
                        model: ["All directions", "Local", "Mesh in", "Mesh out"]
                        currentIndex: 0
                    }

                    PlasmaComponents3.TextField {
                        id: verbFilter

                        Layout.fillWidth: true
                        placeholderText: "Verb glob"
                        text: "*"
                        selectByMouse: true
                        onAccepted: root.applyFilter()
                    }
                }

                RowLayout {
                    Layout.fillWidth: true

                    PlasmaComponents3.ComboBox {
                        id: bodyFilter

                        Layout.fillWidth: true
                        model: ["Metadata only", "Redacted bodies"]
                        currentIndex: 0
                    }

                    PlasmaComponents3.Button {
                        text: "Apply"
                        icon.name: "view-filter"
                        onClicked: root.applyFilter()
                    }
                }
            }

            PlasmaComponents3.ToolButton {
                Layout.fillWidth: true
                text: "Nodes · " + root.backend.inventoryPosture
                    + " · " + root.backend.busNodesModel.count
                icon.name: root.rosterExpanded ? "arrow-down" : "arrow-right"
                display: PlasmaComponents3.AbstractButton.TextBesideIcon
                onClicked: root.rosterExpanded = !root.rosterExpanded
            }

            Kirigami.InlineMessage {
                Layout.fillWidth: true
                visible: root.rosterExpanded
                    && root.backend.inventoryPosture.length > 0
                    && root.backend.inventoryPosture !== "verified"
                type: Kirigami.MessageType.Warning
                text: "Node membership is hidden until noded reports a verified inventory."
            }

            Repeater {
                model: root.rosterExpanded ? root.backend.busNodesModel : null

                delegate: RowLayout {
                    id: nodeDelegate

                    required property string name
                    required property string meshIp
                    required property bool busEnabled
                    required property string status
                    required property string statusIcon

                    Layout.fillWidth: true
                    Layout.leftMargin: Kirigami.Units.smallSpacing
                    Layout.rightMargin: Kirigami.Units.smallSpacing

                    Kirigami.Icon {
                        Layout.preferredWidth: Kirigami.Units.iconSizes.small
                        Layout.preferredHeight: Kirigami.Units.iconSizes.small
                        source: nodeDelegate.statusIcon
                    }

                    PlasmaComponents3.Label {
                        Layout.fillWidth: true
                        text: nodeDelegate.name
                        elide: Text.ElideRight
                    }

                    PlasmaComponents3.Label {
                        text: nodeDelegate.meshIp
                        opacity: 0.65
                        font: Kirigami.Theme.smallFont
                    }
                }
            }

            PlasmaComponents3.Label {
                Layout.fillWidth: true
                Layout.leftMargin: Kirigami.Units.smallSpacing
                Layout.rightMargin: Kirigami.Units.smallSpacing
                visible: root.rosterExpanded && root.backend.localServices.length > 0
                text: "Local: " + root.backend.localServices.join(", ")
                wrapMode: Text.Wrap
                opacity: 0.65
                font: Kirigami.Theme.smallFont
            }

            RowLayout {
                Layout.fillWidth: true

                SectionHeading {
                    Layout.fillWidth: true
                    text: "Traffic · " + root.backend.busTrafficModel.count + "/128"
                }

                PlasmaComponents3.Label {
                    visible: root.backend.serverDropped > 0
                        || root.backend.bridgeDropped > 0
                    text: "Dropped "
                        + (root.backend.serverDropped + root.backend.bridgeDropped)
                    color: Kirigami.Theme.neutralTextColor
                    font: Kirigami.Theme.smallFont
                }
            }

            PlasmaComponents3.Label {
                Layout.fillWidth: true
                visible: root.backend.busSessionOpen
                    && root.backend.busTrafficModel.count === 0
                horizontalAlignment: Text.AlignHCenter
                text: root.backend.busPaused
                    ? "Traffic display paused."
                    : "Waiting for matching Bus traffic…"
                opacity: 0.7
            }

            Repeater {
                model: root.backend.busTrafficModel

                delegate: PlasmaComponents3.ItemDelegate {
                    id: trafficDelegate

                    required property var sequence
                    required property string timestamp
                    required property string direction
                    required property string outcome
                    required property string messageType
                    required property string sourceService
                    required property string targetService
                    required property string verb
                    required property string correlationId
                    required property bool hasReturnCode
                    required property var returnCode
                    required property var messageSize
                    required property var brokerDropped
                    required property string payloadJson
                    required property string payloadOmitted
                    required property string directionIcon
                    property bool expanded: false

                    Layout.fillWidth: true
                    onClicked: trafficDelegate.expanded = !trafficDelegate.expanded

                    contentItem: ColumnLayout {
                        spacing: Math.round(Kirigami.Units.smallSpacing / 2)

                        RowLayout {
                            Layout.fillWidth: true

                            Kirigami.Icon {
                                Layout.preferredWidth: Kirigami.Units.iconSizes.small
                                Layout.preferredHeight: Kirigami.Units.iconSizes.small
                                source: trafficDelegate.directionIcon
                            }

                            PlasmaComponents3.Label {
                                Layout.fillWidth: true
                                text: trafficDelegate.verb.length > 0
                                    ? trafficDelegate.verb
                                    : trafficDelegate.messageType
                                elide: Text.ElideRight
                                font.bold: true
                            }

                            PlasmaComponents3.Label {
                                text: trafficDelegate.timestamp.length >= 19
                                    ? trafficDelegate.timestamp.slice(11, 19)
                                    : trafficDelegate.timestamp
                                opacity: 0.65
                                font: Kirigami.Theme.smallFont
                            }
                        }

                        PlasmaComponents3.Label {
                            Layout.fillWidth: true
                            text: (trafficDelegate.sourceService || "—")
                                + "  →  " + (trafficDelegate.targetService || "—")
                            elide: Text.ElideMiddle
                            opacity: 0.75
                            font: Kirigami.Theme.smallFont
                        }

                        ColumnLayout {
                            Layout.fillWidth: true
                            visible: trafficDelegate.expanded
                            spacing: Math.round(Kirigami.Units.smallSpacing / 2)

                            PlasmaComponents3.Label {
                                Layout.fillWidth: true
                                text: trafficDelegate.direction
                                    + " · " + trafficDelegate.outcome
                                    + " · " + trafficDelegate.messageSize + " B"
                                    + (trafficDelegate.hasReturnCode
                                        ? " · rc " + trafficDelegate.returnCode
                                        : "")
                                wrapMode: Text.Wrap
                                font: Kirigami.Theme.smallFont
                            }

                            PlasmaComponents3.Label {
                                Layout.fillWidth: true
                                visible: trafficDelegate.correlationId.length > 0
                                text: "Correlation: " + trafficDelegate.correlationId
                                elide: Text.ElideMiddle
                                font: Kirigami.Theme.smallFont
                            }

                            PlasmaComponents3.Label {
                                Layout.fillWidth: true
                                visible: trafficDelegate.brokerDropped > 0
                                text: "Broker dropped "
                                    + trafficDelegate.brokerDropped + " before this event"
                                color: Kirigami.Theme.neutralTextColor
                                wrapMode: Text.Wrap
                                font: Kirigami.Theme.smallFont
                            }

                            PlasmaComponents3.Label {
                                Layout.fillWidth: true
                                visible: trafficDelegate.payloadJson.length > 0
                                    || trafficDelegate.payloadOmitted.length > 0
                                text: trafficDelegate.payloadJson.length > 0
                                    ? trafficDelegate.payloadJson
                                    : "Payload omitted: "
                                        + trafficDelegate.payloadOmitted
                                wrapMode: Text.WrapAnywhere
                                font: Kirigami.Theme.smallFont
                            }
                        }
                    }
                }
            }

            Item {
                Layout.fillWidth: true
                Layout.preferredHeight: Kirigami.Units.smallSpacing
            }
        }
    }
}
