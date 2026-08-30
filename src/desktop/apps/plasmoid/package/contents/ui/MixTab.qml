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
    property bool runsExpanded: false

    function openRename(scriptId: string,
                        scriptName: string,
                        scriptDescription: string): void {
        renameDialog.scriptId = scriptId;
        renameName.text = scriptName;
        renameDescription.text = scriptDescription;
        renameDialog.open();
        renameName.forceActiveFocus();
    }

    function confirmPurge(scriptId: string, scriptName: string): void {
        purgeDialog.scriptId = scriptId;
        purgeDialog.scriptName = scriptName;
        purgeDialog.open();
    }

    ColumnLayout {
        anchors.fill: parent
        spacing: Kirigami.Units.smallSpacing

        RowLayout {
            Layout.fillWidth: true
            spacing: Kirigami.Units.smallSpacing

            PlasmaComponents3.TextField {
                id: searchField

                Layout.fillWidth: true
                placeholderText: "Search Mix scripts"
                clearButtonShown: true
                selectByMouse: true
                onTextChanged: root.backend.setMixSearch(text)
            }

            PlasmaComponents3.Button {
                text: "New"
                icon.name: "list-add"
                enabled: !root.backend.mixBusy
                onClicked: {
                    newName.clear();
                    newDescription.clear();
                    newDialog.open();
                    newName.forceActiveFocus();
                }
            }

            PlasmaComponents3.BusyIndicator {
                running: root.backend.mixBusy
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
            visible: root.backend.mixError.length > 0
            type: Kirigami.MessageType.Warning
            text: root.backend.mixError
        }

        PlasmaComponents3.ScrollView {
            id: scrollView

            Layout.fillWidth: true
            Layout.fillHeight: true
            contentWidth: availableWidth

            ColumnLayout {
                width: scrollView.availableWidth
                spacing: Kirigami.Units.smallSpacing

                RowLayout {
                    Layout.fillWidth: true

                    SectionHeading {
                        Layout.fillWidth: true
                        text: "Scripts · " + root.backend.mixScriptsModel.count
                    }

                    PlasmaComponents3.Label {
                        text: root.backend.mixActiveRuns > 0
                            ? root.backend.mixActiveRuns + " running"
                            : root.backend.mixState
                        color: root.backend.mixActiveRuns > 0
                            ? Kirigami.Theme.positiveTextColor
                            : Kirigami.Theme.textColor
                        opacity: root.backend.mixActiveRuns > 0 ? 1 : 0.65
                        font: Kirigami.Theme.smallFont
                    }
                }

                PlasmaComponents3.Label {
                    Layout.fillWidth: true
                    visible: !root.backend.mixBusy
                        && root.backend.mixScriptsModel.count === 0
                    horizontalAlignment: Text.AlignHCenter
                    wrapMode: Text.Wrap
                    text: searchField.text.length > 0
                        ? "No scripts match this search."
                        : "No Mix scripts yet. Create one to begin."
                    opacity: 0.7
                }

                Repeater {
                    model: root.backend.mixScriptsModel

                    delegate: PlasmaComponents3.Frame {
                        id: scriptDelegate

                        required property string scriptId
                        required property string name
                        required property string description
                        required property string modifiedText

                        Layout.fillWidth: true

                        contentItem: ColumnLayout {
                            spacing: Math.round(Kirigami.Units.smallSpacing / 2)

                            RowLayout {
                                Layout.fillWidth: true

                                ColumnLayout {
                                    Layout.fillWidth: true
                                    spacing: 0

                                    PlasmaComponents3.Label {
                                        Layout.fillWidth: true
                                        text: scriptDelegate.name
                                        font.bold: true
                                        elide: Text.ElideRight
                                    }

                                    PlasmaComponents3.Label {
                                        Layout.fillWidth: true
                                        text: scriptDelegate.description.length > 0
                                            ? scriptDelegate.description
                                            : "Modified " + scriptDelegate.modifiedText
                                        elide: Text.ElideRight
                                        opacity: 0.65
                                        font: Kirigami.Theme.smallFont
                                    }
                                }

                                PlasmaComponents3.ToolButton {
                                    text: "Run"
                                    icon.name: "media-playback-start"
                                    display: PlasmaComponents3.AbstractButton.IconOnly
                                    enabled: !root.backend.mixBusy
                                    onClicked: root.backend.runMixScript(
                                        scriptDelegate.scriptId)

                                    PlasmaComponents3.ToolTip {
                                        text: "Run " + scriptDelegate.name
                                    }
                                }

                                PlasmaComponents3.ToolButton {
                                    text: "Edit"
                                    icon.name: "document-edit"
                                    display: PlasmaComponents3.AbstractButton.IconOnly
                                    enabled: !root.backend.mixBusy
                                    onClicked: root.backend.editMixScript(
                                        scriptDelegate.scriptId)

                                    PlasmaComponents3.ToolTip {
                                        text: "Open in the default editor"
                                    }
                                }

                                PlasmaComponents3.ToolButton {
                                    id: scriptMenuButton

                                    text: "More"
                                    icon.name: "application-menu"
                                    display: PlasmaComponents3.AbstractButton.IconOnly
                                    checked: scriptMenu.status === PlasmaExtras.Menu.Open
                                    onPressed: scriptMenu.openRelative()

                                    PlasmaComponents3.ToolTip {
                                        text: "Script actions"
                                    }
                                }
                            }

                            PlasmaComponents3.Label {
                                Layout.fillWidth: true
                                visible: scriptDelegate.description.length > 0
                                text: "Modified " + scriptDelegate.modifiedText
                                opacity: 0.55
                                font: Kirigami.Theme.smallFont
                            }
                        }

                        PlasmaExtras.Menu {
                            id: scriptMenu

                            visualParent: scriptMenuButton
                            placement: PlasmaExtras.Menu.BottomPosedLeftAlignedPopup

                            PlasmaExtras.MenuItem {
                                text: "Rename"
                                icon: "edit-rename"
                                onClicked: root.openRename(scriptDelegate.scriptId,
                                                           scriptDelegate.name,
                                                           scriptDelegate.description)
                            }

                            PlasmaExtras.MenuItem {
                                text: "Move to Trash"
                                icon: "user-trash"
                                onClicked: root.backend.trashMixScript(
                                    scriptDelegate.scriptId)
                            }
                        }
                    }
                }

                PlasmaComponents3.ToolButton {
                    Layout.fillWidth: true
                    text: "Trash · " + root.backend.mixTrashModel.count
                    icon.name: root.trashExpanded ? "arrow-down" : "arrow-right"
                    display: PlasmaComponents3.AbstractButton.TextBesideIcon
                    onClicked: root.trashExpanded = !root.trashExpanded
                }

                PlasmaComponents3.Label {
                    Layout.fillWidth: true
                    visible: root.trashExpanded
                        && root.backend.mixTrashModel.count === 0
                    horizontalAlignment: Text.AlignHCenter
                    text: searchField.text.length > 0
                        ? "No trashed scripts match this search."
                        : "Trash is empty."
                    opacity: 0.7
                }

                Repeater {
                    model: root.trashExpanded ? root.backend.mixTrashModel : null

                    delegate: PlasmaComponents3.ItemDelegate {
                        id: trashDelegate

                        required property string scriptId
                        required property string name
                        required property string modifiedText

                        Layout.fillWidth: true

                        contentItem: RowLayout {
                            PlasmaComponents3.Label {
                                Layout.fillWidth: true
                                text: trashDelegate.name
                                elide: Text.ElideRight
                            }

                            PlasmaComponents3.ToolButton {
                                text: "Restore"
                                icon.name: "edit-undo"
                                display: PlasmaComponents3.AbstractButton.IconOnly
                                onClicked: root.backend.restoreMixScript(
                                    trashDelegate.scriptId)

                                PlasmaComponents3.ToolTip {
                                    text: "Restore " + trashDelegate.name
                                }
                            }

                            PlasmaComponents3.ToolButton {
                                text: "Delete permanently"
                                icon.name: "edit-delete"
                                display: PlasmaComponents3.AbstractButton.IconOnly
                                onClicked: root.confirmPurge(trashDelegate.scriptId,
                                                            trashDelegate.name)

                                PlasmaComponents3.ToolTip {
                                    text: "Delete permanently"
                                }
                            }
                        }
                    }
                }

                PlasmaComponents3.ToolButton {
                    Layout.fillWidth: true
                    visible: root.backend.mixRunsModel.count > 0
                    text: "Recent runs · " + root.backend.mixRunsModel.count
                    icon.name: root.runsExpanded ? "arrow-down" : "arrow-right"
                    display: PlasmaComponents3.AbstractButton.TextBesideIcon
                    onClicked: root.runsExpanded = !root.runsExpanded
                }

                Repeater {
                    model: root.runsExpanded ? root.backend.mixRunsModel : null

                    delegate: PlasmaComponents3.ItemDelegate {
                        id: runDelegate

                        required property string runId
                        required property string scriptName
                        required property string runState
                        required property string statusIcon

                        Layout.fillWidth: true
                        icon.name: runDelegate.statusIcon
                        text: runDelegate.scriptName + " · " + runDelegate.runState
                        onClicked: root.backend.selectMixRun(runDelegate.runId)
                    }
                }

                Item {
                    Layout.fillWidth: true
                    Layout.preferredHeight: Kirigami.Units.smallSpacing
                }
            }
        }

        PlasmaComponents3.Frame {
            Layout.fillWidth: true
            Layout.maximumHeight: Kirigami.Units.gridUnit * 12
            visible: root.backend.selectedMixRunId.length > 0

            contentItem: ColumnLayout {
                spacing: Kirigami.Units.smallSpacing

                RowLayout {
                    Layout.fillWidth: true

                    ColumnLayout {
                        Layout.fillWidth: true
                        spacing: 0

                        PlasmaComponents3.Label {
                            Layout.fillWidth: true
                            text: root.backend.selectedMixRunName
                            font.bold: true
                            elide: Text.ElideRight
                        }

                        PlasmaComponents3.Label {
                            Layout.fillWidth: true
                            text: root.backend.selectedMixRunState
                                + (root.backend.selectedMixRunHasExitCode
                                    ? " · exit " + root.backend.selectedMixRunExitCode
                                    : "")
                            color: root.backend.selectedMixRunState === "failed"
                                || root.backend.selectedMixRunState === "launch_failed"
                                ? Kirigami.Theme.negativeTextColor
                                : Kirigami.Theme.textColor
                            font: Kirigami.Theme.smallFont
                        }
                    }

                    PlasmaComponents3.ToolButton {
                        text: "Stop"
                        icon.name: "process-stop"
                        display: PlasmaComponents3.AbstractButton.IconOnly
                        visible: root.backend.selectedMixRunActive
                        onClicked: root.backend.stopMixRun(
                            root.backend.selectedMixRunId)

                        PlasmaComponents3.ToolTip {
                            text: "Stop this run"
                        }
                    }

                    PlasmaComponents3.ToolButton {
                        text: "Copy"
                        icon.name: "edit-copy"
                        display: PlasmaComponents3.AbstractButton.IconOnly
                        onClicked: {
                            outputArea.selectAll();
                            outputArea.copy();
                            outputArea.deselect();
                        }

                        PlasmaComponents3.ToolTip {
                            text: "Copy visible output"
                        }
                    }

                    PlasmaComponents3.ToolButton {
                        text: "Close"
                        icon.name: "window-close"
                        display: PlasmaComponents3.AbstractButton.IconOnly
                        onClicked: root.backend.closeMixOutput()

                        PlasmaComponents3.ToolTip {
                            text: "Close output"
                        }
                    }
                }

                Kirigami.InlineMessage {
                    Layout.fillWidth: true
                    visible: outputTabs.currentIndex === 0
                        ? root.backend.selectedMixRunStdoutDropped > 0
                        : root.backend.selectedMixRunStderrDropped > 0
                    type: Kirigami.MessageType.Warning
                    text: "Earlier output was dropped from the bounded tail."
                }

                PlasmaComponents3.TabBar {
                    id: outputTabs

                    Layout.fillWidth: true

                    PlasmaComponents3.TabButton {
                        text: "stdout"
                    }

                    PlasmaComponents3.TabButton {
                        text: "stderr"
                    }
                }

                PlasmaComponents3.ScrollView {
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    Layout.minimumHeight: Kirigami.Units.gridUnit * 4

                    PlasmaComponents3.TextArea {
                        id: outputArea

                        readOnly: true
                        selectByMouse: true
                        wrapMode: TextEdit.WrapAnywhere
                        font.family: "monospace"
                        text: outputTabs.currentIndex === 0
                            ? root.backend.selectedMixRunStdout
                            : root.backend.selectedMixRunStderr
                    }
                }
            }
        }
    }

    Kirigami.Dialog {
        id: newDialog

        title: "New Mix script"
        preferredWidth: Kirigami.Units.gridUnit * 20
        standardButtons: Kirigami.Dialog.Cancel

        property Kirigami.Action createAction: Kirigami.Action {
            text: "Create and Edit"
            icon.name: "list-add"
            enabled: newName.text.trim().length > 0
            onTriggered: {
                root.backend.createMixScript(newName.text, newDescription.text);
                newDialog.close();
            }
        }

        customFooterActions: [createAction]

        Kirigami.FormLayout {
            PlasmaComponents3.TextField {
                id: newName

                Kirigami.FormData.label: "Name:"
                placeholderText: "My script"
                selectByMouse: true
                onAccepted: {
                    if (newDialog.createAction.enabled) {
                        newDialog.createAction.trigger();
                    }
                }
            }

            PlasmaComponents3.TextField {
                id: newDescription

                Kirigami.FormData.label: "Description:"
                placeholderText: "Optional"
                selectByMouse: true
            }

            PlasmaComponents3.Label {
                Kirigami.FormData.label: "Next:"
                Layout.fillWidth: true
                text: "A safe template opens in your default editor."
                wrapMode: Text.Wrap
                opacity: 0.7
            }
        }
    }

    Kirigami.Dialog {
        id: renameDialog

        property string scriptId

        title: "Rename Mix script"
        preferredWidth: Kirigami.Units.gridUnit * 20
        standardButtons: Kirigami.Dialog.Cancel

        property Kirigami.Action renameAction: Kirigami.Action {
            text: "Save"
            icon.name: "document-save"
            enabled: renameName.text.trim().length > 0
            onTriggered: {
                root.backend.updateMixScript(renameDialog.scriptId,
                                             renameName.text,
                                             renameDescription.text);
                renameDialog.close();
            }
        }

        customFooterActions: [renameAction]

        Kirigami.FormLayout {
            PlasmaComponents3.TextField {
                id: renameName

                Kirigami.FormData.label: "Name:"
                selectByMouse: true
                onAccepted: {
                    if (renameDialog.renameAction.enabled) {
                        renameDialog.renameAction.trigger();
                    }
                }
            }

            PlasmaComponents3.TextField {
                id: renameDescription

                Kirigami.FormData.label: "Description:"
                selectByMouse: true
            }
        }
    }

    Kirigami.PromptDialog {
        id: purgeDialog

        property string scriptId
        property string scriptName

        title: "Move beyond recovery?"
        subtitle: "“" + scriptName
            + "” is already in Trash. Continue to the final permanent-delete check?"
        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        onAccepted: {
            close();
            purgeFinalDialog.scriptId = scriptId;
            purgeFinalDialog.scriptName = scriptName;
            purgeFinalDialog.open();
        }
    }

    Kirigami.PromptDialog {
        id: purgeFinalDialog

        property string scriptId
        property string scriptName

        title: "Permanently delete now?"
        subtitle: "Final confirmation: “" + scriptName
            + "” and its source file will be removed. This cannot be undone."
        standardButtons: Kirigami.Dialog.Ok | Kirigami.Dialog.Cancel
        onAccepted: {
            root.backend.purgeMixScript(scriptId);
            close();
        }
    }
}
