pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Layouts

import org.kde.kirigami as Kirigami
import org.kde.plasma.components as PlasmaComponents3

import "components"

Item {
    id: root

    required property var backend

    PlasmaComponents3.ScrollView {
        id: scrollView

        anchors.fill: parent
        contentWidth: availableWidth

        ColumnLayout {
            width: scrollView.availableWidth
            spacing: Kirigami.Units.smallSpacing

            RowLayout {
                Layout.fillWidth: true

                Kirigami.Heading {
                    Layout.fillWidth: true
                    text: "Local CosMix"
                    level: 2
                }

                PlasmaComponents3.BusyIndicator {
                    running: root.backend.busy
                    visible: running
                    Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                    Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
                }

                // noded reachability lives in the heading rather than a banner
                // row of its own — the popup is short on vertical space and the
                // healthy case is the common one.
                Kirigami.Icon {
                    id: nodedIcon

                    readonly property string label: !root.backend.nodedChecked
                        ? "Local noded reachability has not been checked"
                        : (root.backend.nodedReachable
                            ? "Local noded is reachable"
                            : "Local noded is unreachable")

                    Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                    Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium

                    // the InlineMessage this replaced was an alert, and a
                    // reachability flip is worth announcing when it happens —
                    // StaticText would only be found by traversal
                    Accessible.role: Accessible.AlertMessage
                    Accessible.name: nodedIcon.label

                    source: !root.backend.nodedChecked
                        ? "dialog-information"
                        : (root.backend.nodedReachable ? "dialog-ok" : "dialog-error")
                    // Breeze draws dialog-ok from ColorScheme-Text, so without a
                    // tint the healthy state is a plain white tick; dialog-error
                    // and dialog-information carry their own colour and ignore
                    // this, which is why the mapping is stated in full anyway
                    color: !root.backend.nodedChecked
                        ? Kirigami.Theme.neutralTextColor
                        : (root.backend.nodedReachable
                            ? Kirigami.Theme.positiveTextColor
                            : Kirigami.Theme.negativeTextColor)

                    HoverHandler {
                        id: nodedHover
                    }

                    PlasmaComponents3.ToolTip {
                        text: nodedIcon.label
                        visible: nodedHover.hovered
                    }
                }

                PlasmaComponents3.ToolButton {
                    icon.name: "view-refresh"
                    text: "Refresh"
                    display: PlasmaComponents3.AbstractButton.IconOnly
                    onClicked: root.backend.refresh()

                    PlasmaComponents3.ToolTip {
                        text: "Refresh discovery"
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
                visible: root.backend.refreshError.length > 0
                type: Kirigami.MessageType.Warning
                text: root.backend.refreshError
            }

            SectionHeading {
                Layout.fillWidth: true
                text: "Applications"
            }

            Kirigami.InlineMessage {
                Layout.fillWidth: true
                visible: root.backend.appsError.length > 0
                type: Kirigami.MessageType.Warning
                text: root.backend.appsError
            }

            PlasmaComponents3.Label {
                Layout.fillWidth: true
                visible: root.backend.snapshotReady
                    && root.backend.appsModel.count === 0
                    && root.backend.appsError.length === 0
                text: "No CosMix applications discovered."
                opacity: 0.7
            }

            Repeater {
                model: root.backend.appsModel

                delegate: PlasmaComponents3.ItemDelegate {
                    id: appDelegate

                    required property string slug
                    required property string label
                    required property string iconName
                    required property bool launchable

                    Layout.fillWidth: true
                    text: appDelegate.label
                    icon.name: appDelegate.launchable
                        ? (appDelegate.iconName.length > 0
                            ? appDelegate.iconName
                            : "application-x-executable")
                        : "dialog-warning"
                    enabled: appDelegate.launchable
                    onClicked: root.backend.launchApp(appDelegate.slug)
                }
            }

            DaemonSection {
                Layout.fillWidth: true
                backend: root.backend
                title: "System daemons"
                daemonModel: root.backend.systemDaemonsModel
                emptyText: "No system daemons discovered."
            }

            DaemonSection {
                Layout.fillWidth: true
                backend: root.backend
                title: "User daemons"
                daemonModel: root.backend.userDaemonsModel
                emptyText: "No user daemons discovered."
            }

            Kirigami.InlineMessage {
                Layout.fillWidth: true
                visible: root.backend.daemonsError.length > 0
                type: Kirigami.MessageType.Warning
                text: root.backend.daemonsError
            }

            Item {
                Layout.fillWidth: true
                Layout.preferredHeight: Kirigami.Units.smallSpacing
            }
        }
    }
}
