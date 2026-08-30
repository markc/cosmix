pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Layouts

import org.kde.kirigami as Kirigami
import org.kde.plasma.components as PlasmaComponents3
import org.kde.plasma.extras as PlasmaExtras

import "components"

Item {
    id: root

    required property var backend
    property bool trashExpanded: false
    property bool keysExpanded: false

    function confirmPurge(hostId: string): void {
        purgeDialog.hostId = hostId;
        purgeDialog.open();
    }

    ColumnLayout {
        anchors.fill: parent
        spacing: Kirigami.Units.smallSpacing

        RowLayout {
            Layout.fillWidth: true
            spacing: Kirigami.Units.smallSpacing

            SectionHeading {
                Layout.fillWidth: true
                text: "SSH hosts · " + root.backend.sshHostsModel.count
            }

            PlasmaComponents3.Label {
                text: root.backend.sshActiveProbes > 0
                    ? root.backend.sshActiveProbes + " probing"
                    : root.backend.sshState
                color: root.backend.sshActiveProbes > 0
                    ? Kirigami.Theme.neutralTextColor
                    : Kirigami.Theme.textColor
                opacity: root.backend.sshActiveProbes > 0 ? 1 : 0.65
                font: Kirigami.Theme.smallFont
            }

            PlasmaComponents3.Button {
                text: "Test all"
                icon.name: "network-connect"
                enabled: !root.backend.sshBusy
                    && root.backend.sshHostsModel.count > 0
                onClicked: root.backend.probeSshHosts([])
            }

            PlasmaComponents3.Button {
                text: "New"
                icon.name: "list-add"
                enabled: !root.backend.sshBusy
                    && root.backend.sshKeysModel.count > 0
                onClicked: {
                    newName.clear();
                    newHostname.clear();
                    newPort.value = 22;
                    newUser.text = "root";
                    newKey.currentIndex = 0;
                    newDialog.open();
                    newName.forceActiveFocus();
                }
            }

            PlasmaComponents3.BusyIndicator {
                running: root.backend.sshBusy
                visible: running
                Layout.preferredWidth: Kirigami.Units.iconSizes.small
                Layout.preferredHeight: Kirigami.Units.iconSizes.small
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
            visible: root.backend.sshError.length > 0
            type: Kirigami.MessageType.Warning
            text: root.backend.sshError
        }

        PlasmaComponents3.ScrollView {
            id: scrollView

            Layout.fillWidth: true
            Layout.fillHeight: true
            contentWidth: availableWidth

            ColumnLayout {
                width: scrollView.availableWidth
                spacing: Kirigami.Units.smallSpacing

                PlasmaComponents3.Label {
                    Layout.fillWidth: true
                    visible: !root.backend.sshBusy
                        && root.backend.sshHostsModel.count === 0
                    horizontalAlignment: Text.AlignHCenter
                    wrapMode: Text.Wrap
                    text: "No SSH hosts found."
                    opacity: 0.7
                }

                Repeater {
                    model: root.backend.sshHostsModel

                    delegate: PlasmaComponents3.Frame {
                        id: hostDelegate

                        required property string hostId
                        required property string hostError
                        required property string hostWarning
                        required property string hostname
                        required property int port
                        required property string user
                        required property string identity
                        required property string probeStatus
                        required property string probeError
                        required property var probeMs
                        required property string dotStatus
                        required property bool actionable

                        Layout.fillWidth: true

                        contentItem: ColumnLayout {
                            spacing: Math.round(Kirigami.Units.smallSpacing / 2)

                            RowLayout {
                                Layout.fillWidth: true

                                StatusDot {
                                    status: hostDelegate.dotStatus
                                }

                                ColumnLayout {
                                    Layout.fillWidth: true
                                    spacing: 0

                                    PlasmaComponents3.Label {
                                        Layout.fillWidth: true
                                        text: hostDelegate.hostId
                                        font.bold: true
                                        elide: Text.ElideRight
                                    }

                                    PlasmaComponents3.Label {
                                        Layout.fillWidth: true
                                        text: hostDelegate.hostname.length > 0
                                            ? hostDelegate.user + "@"
                                                + hostDelegate.hostname + ":"
                                                + hostDelegate.port
                                            : "Invalid host entry"
                                        elide: Text.ElideMiddle
                                        opacity: 0.7
                                        font: Kirigami.Theme.smallFont
                                    }
                                }

                                PlasmaComponents3.ToolButton {
                                    text: "Connect"
                                    icon.name: "utilities-terminal"
                                    display: PlasmaComponents3.AbstractButton.IconOnly
                                    enabled: hostDelegate.actionable
                                        && !root.backend.sshBusy
                                    onClicked: root.backend.connectSshHost(
                                        hostDelegate.hostId)

                                    PlasmaComponents3.ToolTip {
                                        text: "Connect to " + hostDelegate.hostId
                                    }
                                }

                                PlasmaComponents3.ToolButton {
                                    text: "Test"
                                    icon.name: "network-connect"
                                    display: PlasmaComponents3.AbstractButton.IconOnly
                                    enabled: hostDelegate.actionable
                                        && hostDelegate.probeStatus !== "probing"
                                        && !root.backend.sshBusy
                                    onClicked: root.backend.probeSshHosts(
                                        [hostDelegate.hostId])

                                    PlasmaComponents3.ToolTip {
                                        text: "Test " + hostDelegate.hostId
                                    }
                                }

                                PlasmaComponents3.ToolButton {
                                    text: "Edit"
                                    icon.name: "document-edit"
                                    display: PlasmaComponents3.AbstractButton.IconOnly
                                    enabled: !root.backend.sshBusy
                                    onClicked: root.backend.editSshHost(
                                        hostDelegate.hostId)

                                    PlasmaComponents3.ToolTip {
                                        text: "Open host fragment in the default editor"
                                    }
                                }

                                PlasmaComponents3.ToolButton {
                                    id: hostMenuButton

                                    text: "More"
                                    icon.name: "application-menu"
                                    display: PlasmaComponents3.AbstractButton.IconOnly
                                    checked: hostMenu.status === PlasmaExtras.Menu.Open
                                    onPressed: hostMenu.openRelative()

                                    PlasmaComponents3.ToolTip {
                                        text: "Host actions"
                                    }
                                }
                            }

                            PlasmaComponents3.Label {
                                Layout.fillWidth: true
                                visible: hostDelegate.probeStatus !== "unknown"
                                text: hostDelegate.probeStatus
                                    + (hostDelegate.probeMs > 0
                                        ? " · " + hostDelegate.probeMs + " ms"
                                        : "")
                                color: hostDelegate.probeStatus === "failed"
                                    ? Kirigami.Theme.negativeTextColor
                                    : Kirigami.Theme.textColor
                                font: Kirigami.Theme.smallFont
                            }

                            Kirigami.InlineMessage {
                                Layout.fillWidth: true
                                visible: hostDelegate.hostError.length > 0
                                type: Kirigami.MessageType.Warning
                                text: hostDelegate.hostError
                            }

                            Kirigami.InlineMessage {
                                Layout.fillWidth: true
                                visible: hostDelegate.hostWarning.length > 0
                                type: Kirigami.MessageType.Warning
                                text: hostDelegate.hostWarning
                            }

                            Kirigami.InlineMessage {
                                Layout.fillWidth: true
                                visible: hostDelegate.probeError.length > 0
                                type: Kirigami.MessageType.Error
                                text: hostDelegate.probeError
                            }
                        }

                        PlasmaExtras.Menu {
                            id: hostMenu

                            visualParent: hostMenuButton
                            placement: PlasmaExtras.Menu.BottomPosedLeftAlignedPopup

                            PlasmaExtras.MenuItem {
                                text: "Move to Trash"
                                icon: "user-trash"
                                onClicked: root.backend.trashSshHost(hostDelegate.hostId)
                            }
                        }
                    }
                }

                PlasmaComponents3.ToolButton {
                    Layout.fillWidth: true
                    text: "Trash · " + root.backend.sshTrashModel.count
                    icon.name: root.trashExpanded ? "arrow-down" : "arrow-right"
                    display: PlasmaComponents3.AbstractButton.TextBesideIcon
                    onClicked: root.trashExpanded = !root.trashExpanded
                }

                PlasmaComponents3.Label {
                    Layout.fillWidth: true
                    visible: root.trashExpanded
                        && root.backend.sshTrashModel.count === 0
                    horizontalAlignment: Text.AlignHCenter
                    text: "Trash is empty."
                    opacity: 0.7
                }

                Repeater {
                    model: root.trashExpanded ? root.backend.sshTrashModel : null

                    delegate: PlasmaComponents3.ItemDelegate {
                        id: trashDelegate

                        required property string hostId
                        required property string hostError
                        required property string hostWarning
                        required property string hostname
                        required property int port
                        required property string user
                        required property string probeError

                        Layout.fillWidth: true

                        contentItem: ColumnLayout {
                            spacing: 0

                            RowLayout {
                                Layout.fillWidth: true

                                PlasmaComponents3.Label {
                                    Layout.fillWidth: true
                                    text: trashDelegate.hostId + " · "
                                        + trashDelegate.user + "@"
                                        + trashDelegate.hostname + ":"
                                        + trashDelegate.port
                                    elide: Text.ElideMiddle
                                }

                                PlasmaComponents3.ToolButton {
                                    text: "Restore"
                                    icon.name: "edit-undo"
                                    display: PlasmaComponents3.AbstractButton.IconOnly
                                    enabled: !root.backend.sshBusy
                                    onClicked: root.backend.restoreSshHost(
                                        trashDelegate.hostId)

                                    PlasmaComponents3.ToolTip {
                                        text: "Restore " + trashDelegate.hostId
                                    }
                                }

                                PlasmaComponents3.ToolButton {
                                    text: "Delete permanently"
                                    icon.name: "edit-delete"
                                    display: PlasmaComponents3.AbstractButton.IconOnly
                                    enabled: !root.backend.sshBusy
                                    onClicked: root.confirmPurge(trashDelegate.hostId)

                                    PlasmaComponents3.ToolTip {
                                        text: "Delete permanently"
                                    }
                                }
                            }

                            PlasmaComponents3.Label {
                                Layout.fillWidth: true
                                visible: trashDelegate.hostError.length > 0
                                text: trashDelegate.hostError
                                color: Kirigami.Theme.negativeTextColor
                                wrapMode: Text.Wrap
                                font: Kirigami.Theme.smallFont
                            }

                            PlasmaComponents3.Label {
                                Layout.fillWidth: true
                                visible: trashDelegate.hostWarning.length > 0
                                text: trashDelegate.hostWarning
                                color: Kirigami.Theme.neutralTextColor
                                wrapMode: Text.Wrap
                                font: Kirigami.Theme.smallFont
                            }

                            PlasmaComponents3.Label {
                                Layout.fillWidth: true
                                visible: trashDelegate.probeError.length > 0
                                text: trashDelegate.probeError
                                color: Kirigami.Theme.negativeTextColor
                                wrapMode: Text.Wrap
                                font: Kirigami.Theme.smallFont
                            }
                        }
                    }
                }

                PlasmaComponents3.ToolButton {
                    Layout.fillWidth: true
                    text: "Keys · " + root.backend.sshKeysModel.count
                    icon.name: root.keysExpanded ? "arrow-down" : "arrow-right"
                    display: PlasmaComponents3.AbstractButton.TextBesideIcon
                    onClicked: root.keysExpanded = !root.keysExpanded
                }

                Repeater {
                    model: root.keysExpanded ? root.backend.sshKeysModel : null

                    delegate: PlasmaComponents3.ItemDelegate {
                        id: keyDelegate

                        required property string keyId
                        required property string fingerprint
                        required property string keyError

                        Layout.fillWidth: true

                        contentItem: ColumnLayout {
                            spacing: 0

                            PlasmaComponents3.Label {
                                Layout.fillWidth: true
                                text: keyDelegate.keyId
                                    + (keyDelegate.fingerprint.length > 0
                                        ? " · " + keyDelegate.fingerprint : "")
                                elide: Text.ElideMiddle
                            }

                            PlasmaComponents3.Label {
                                Layout.fillWidth: true
                                visible: keyDelegate.keyError.length > 0
                                text: keyDelegate.keyError
                                color: Kirigami.Theme.negativeTextColor
                                wrapMode: Text.Wrap
                                font: Kirigami.Theme.smallFont
                            }
                        }

                        PlasmaComponents3.ToolTip {
                            text: keyDelegate.keyError.length > 0
                                ? keyDelegate.keyError : keyDelegate.fingerprint
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

    Kirigami.Dialog {
        id: newDialog

        title: "New SSH host"
        preferredWidth: Kirigami.Units.gridUnit * 20
        standardButtons: Kirigami.Dialog.Cancel

        property Kirigami.Action createAction: Kirigami.Action {
            text: "Create"
            icon.name: "list-add"
            enabled: newName.text.trim().length > 0
                && newHostname.text.trim().length > 0
                && newKey.currentIndex >= 0
            onTriggered: {
                root.backend.createSshHost(newName.text,
                                           newHostname.text,
                                           newPort.value,
                                           newUser.text,
                                           newKey.currentValue);
                newDialog.close();
            }
        }

        customFooterActions: [createAction]

        Kirigami.FormLayout {
            PlasmaComponents3.TextField {
                id: newName

                Kirigami.FormData.label: "Name:"
                placeholderText: "server-name"
                selectByMouse: true
            }

            PlasmaComponents3.TextField {
                id: newHostname

                Kirigami.FormData.label: "Hostname:"
                placeholderText: "server.example.com"
                selectByMouse: true
            }

            PlasmaComponents3.SpinBox {
                id: newPort

                Kirigami.FormData.label: "Port:"
                from: 1
                to: 65535
                value: 22
                editable: true
            }

            PlasmaComponents3.TextField {
                id: newUser

                Kirigami.FormData.label: "User:"
                text: "root"
                selectByMouse: true
            }

            PlasmaComponents3.ComboBox {
                id: newKey

                Kirigami.FormData.label: "Key:"
                Layout.fillWidth: true
                model: root.backend.sshKeysModel
                textRole: "keyId"
                valueRole: "keyId"
            }
        }
    }

    Kirigami.PromptDialog {
        id: purgeDialog

        property string hostId

        title: "Move beyond recovery?"
        subtitle: "“" + hostId
            + "” is already in Trash. Continue to the final permanent-delete check?"
        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        onAccepted: {
            close();
            purgeFinalDialog.hostId = hostId;
            purgeFinalDialog.open();
        }
    }

    Kirigami.PromptDialog {
        id: purgeFinalDialog

        property string hostId

        title: "Permanently delete now?"
        subtitle: "Final confirmation: “" + hostId
            + "” will be removed. This cannot be undone."
        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        onAccepted: {
            root.backend.purgeSshHost(hostId);
            close();
        }
    }
}
