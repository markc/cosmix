pragma ComponentBehavior: Bound

import QtQuick

import org.kde.plasma.core as PlasmaCore
import org.kde.plasma.plasmoid

import "CosmixBackend" as CosmixBackend

PlasmoidItem {
    id: root

    property bool openNotified: false

    function notifyOpened(): void {
        if (!root.openNotified) {
            root.openNotified = true;
            backend.popupOpened();
        }
    }

    function syncOpenState(): void {
        if (root.expanded) {
            root.notifyOpened();
        } else {
            root.openNotified = false;
            backend.popupClosed();
        }
    }

    Plasmoid.backgroundHints: PlasmaCore.Types.DefaultBackground
    Plasmoid.icon: "dev.cosmix.tray"
    Plasmoid.status: PlasmaCore.Types.ActiveStatus
    Plasmoid.title: "CosMix"

    toolTipMainText: "CosMix"
    toolTipSubText: backend.snapshotReady
        ? (backend.nodedReachable ? "Local noded reachable" : "Local noded unreachable")
        : "Applications, daemons, Bus, Mix and SSH"

    CosmixBackend.TraydBridge {
        id: backend
    }

    compactRepresentation: CompactRepresentation {
        onActivated: root.expanded = !root.expanded
    }

    fullRepresentation: FullRepresentation {
        backend: backend
        onRepresentationShown: root.notifyOpened()
        onBusVisibilityChanged: active => backend.setBusVisible(active)
        onMixVisibilityChanged: active => backend.setMixVisible(active)
    }

    onExpandedChanged: root.syncOpenState()
    Component.onCompleted: root.syncOpenState()
}
