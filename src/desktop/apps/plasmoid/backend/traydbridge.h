#pragma once

#include "listmodels.h"

#include <QDBusConnection>
#include <QHash>
#include <QObject>
#include <QQmlEngine>
#include <QSet>

class QDBusPendingCall;
class QDBusPendingCallWatcher;
class QDBusServiceWatcher;
class QTimer;

namespace Cosmix
{

class COSMIXPLASMOIDBACKEND_EXPORT TraydBridge : public QObject
{
    Q_OBJECT
    QML_ELEMENT

    Q_PROPERTY(QAbstractItemModel *appsModel READ appsModel CONSTANT)
    Q_PROPERTY(QAbstractItemModel *systemDaemonsModel READ systemDaemonsModel CONSTANT)
    Q_PROPERTY(QAbstractItemModel *userDaemonsModel READ userDaemonsModel CONSTANT)
    Q_PROPERTY(QAbstractItemModel *busNodesModel READ busNodesModel CONSTANT)
    Q_PROPERTY(QAbstractItemModel *busTrafficModel READ busTrafficModel CONSTANT)
    Q_PROPERTY(QAbstractItemModel *mixScriptsModel READ mixScriptsModel CONSTANT)
    Q_PROPERTY(QAbstractItemModel *mixTrashModel READ mixTrashModel CONSTANT)
    Q_PROPERTY(QAbstractItemModel *mixRunsModel READ mixRunsModel CONSTANT)
    Q_PROPERTY(QAbstractItemModel *sshHostsModel READ sshHostsModel CONSTANT)
    Q_PROPERTY(QAbstractItemModel *sshTrashModel READ sshTrashModel CONSTANT)
    Q_PROPERTY(QAbstractItemModel *sshKeysModel READ sshKeysModel CONSTANT)
    Q_PROPERTY(quint64 revision READ revision NOTIFY snapshotChanged)
    Q_PROPERTY(bool snapshotReady READ snapshotReady NOTIFY snapshotChanged)
    Q_PROPERTY(bool nodedChecked READ nodedChecked NOTIFY snapshotChanged)
    Q_PROPERTY(bool nodedReachable READ nodedReachable NOTIFY snapshotChanged)
    Q_PROPERTY(QString appsError READ appsError NOTIFY snapshotChanged)
    Q_PROPERTY(QString daemonsError READ daemonsError NOTIFY snapshotChanged)
    Q_PROPERTY(QString refreshError READ refreshError NOTIFY snapshotChanged)
    Q_PROPERTY(QString connectionError READ connectionError NOTIFY connectionErrorChanged)
    Q_PROPERTY(bool busy READ busy NOTIFY busyChanged)
    Q_PROPERTY(quint64 busRevision READ busRevision NOTIFY busChanged)
    Q_PROPERTY(QString busState READ busState NOTIFY busChanged)
    Q_PROPERTY(QString busError READ busError NOTIFY busChanged)
    Q_PROPERTY(bool busObserving READ busObserving NOTIFY busChanged)
    Q_PROPERTY(QStringList busDirections READ busDirections NOTIFY busChanged)
    Q_PROPERTY(QStringList busVerbs READ busVerbs NOTIFY busChanged)
    Q_PROPERTY(QString busBodyMode READ busBodyMode NOTIFY busChanged)
    Q_PROPERTY(QString inventoryPosture READ inventoryPosture NOTIFY busChanged)
    Q_PROPERTY(QStringList localServices READ localServices NOTIFY busChanged)
    Q_PROPERTY(quint64 serverDropped READ serverDropped NOTIFY busChanged)
    Q_PROPERTY(quint64 bridgeDropped READ bridgeDropped NOTIFY busChanged)
    Q_PROPERTY(bool busBusy READ busBusy NOTIFY busBusyChanged)
    Q_PROPERTY(bool busPaused READ busPaused NOTIFY busPausedChanged)
    Q_PROPERTY(bool busSessionOpen READ busSessionOpen NOTIFY busSessionChanged)
    Q_PROPERTY(quint64 mixRevision READ mixRevision NOTIFY mixChanged)
    Q_PROPERTY(QString mixState READ mixState NOTIFY mixChanged)
    Q_PROPERTY(QString mixError READ mixError NOTIFY mixChanged)
    Q_PROPERTY(quint32 mixActiveRuns READ mixActiveRuns NOTIFY mixChanged)
    Q_PROPERTY(bool mixBusy READ mixBusy NOTIFY mixBusyChanged)
    Q_PROPERTY(QString mixSearch READ mixSearch WRITE setMixSearch NOTIFY mixSearchChanged)
    Q_PROPERTY(QString selectedMixRunId READ selectedMixRunId NOTIFY selectedMixRunChanged)
    Q_PROPERTY(QString selectedMixRunName READ selectedMixRunName NOTIFY selectedMixRunChanged)
    Q_PROPERTY(QString selectedMixRunState READ selectedMixRunState NOTIFY selectedMixRunChanged)
    Q_PROPERTY(QString selectedMixRunStdout READ selectedMixRunStdout NOTIFY selectedMixRunChanged)
    Q_PROPERTY(QString selectedMixRunStderr READ selectedMixRunStderr NOTIFY selectedMixRunChanged)
    Q_PROPERTY(bool selectedMixRunActive READ selectedMixRunActive NOTIFY selectedMixRunChanged)
    Q_PROPERTY(bool selectedMixRunHasExitCode READ selectedMixRunHasExitCode
                   NOTIFY selectedMixRunChanged)
    Q_PROPERTY(qint32 selectedMixRunExitCode READ selectedMixRunExitCode
                   NOTIFY selectedMixRunChanged)
    Q_PROPERTY(quint64 selectedMixRunStdoutDropped READ selectedMixRunStdoutDropped
                   NOTIFY selectedMixRunChanged)
    Q_PROPERTY(quint64 selectedMixRunStderrDropped READ selectedMixRunStderrDropped
                   NOTIFY selectedMixRunChanged)
    Q_PROPERTY(quint64 sshRevision READ sshRevision NOTIFY sshChanged)
    Q_PROPERTY(QString sshState READ sshState NOTIFY sshChanged)
    Q_PROPERTY(QString sshError READ sshError NOTIFY sshChanged)
    Q_PROPERTY(quint32 sshActiveProbes READ sshActiveProbes NOTIFY sshChanged)
    Q_PROPERTY(bool sshBusy READ sshBusy NOTIFY sshBusyChanged)

public:
    explicit TraydBridge(QObject *parent = nullptr);
    ~TraydBridge() override;

    QAbstractItemModel *appsModel();
    QAbstractItemModel *systemDaemonsModel();
    QAbstractItemModel *userDaemonsModel();
    QAbstractItemModel *busNodesModel();
    QAbstractItemModel *busTrafficModel();
    QAbstractItemModel *mixScriptsModel();
    QAbstractItemModel *mixTrashModel();
    QAbstractItemModel *mixRunsModel();
    QAbstractItemModel *sshHostsModel();
    QAbstractItemModel *sshTrashModel();
    QAbstractItemModel *sshKeysModel();

    quint64 revision() const;
    bool snapshotReady() const;
    bool nodedChecked() const;
    bool nodedReachable() const;
    QString appsError() const;
    QString daemonsError() const;
    QString refreshError() const;
    QString connectionError() const;
    bool busy() const;
    quint64 busRevision() const;
    QString busState() const;
    QString busError() const;
    bool busObserving() const;
    QStringList busDirections() const;
    QStringList busVerbs() const;
    QString busBodyMode() const;
    QString inventoryPosture() const;
    QStringList localServices() const;
    quint64 serverDropped() const;
    quint64 bridgeDropped() const;
    bool busBusy() const;
    bool busPaused() const;
    bool busSessionOpen() const;
    quint64 mixRevision() const;
    QString mixState() const;
    QString mixError() const;
    quint32 mixActiveRuns() const;
    bool mixBusy() const;
    QString mixSearch() const;
    QString selectedMixRunId() const;
    QString selectedMixRunName() const;
    QString selectedMixRunState() const;
    QString selectedMixRunStdout() const;
    QString selectedMixRunStderr() const;
    bool selectedMixRunActive() const;
    bool selectedMixRunHasExitCode() const;
    qint32 selectedMixRunExitCode() const;
    quint64 selectedMixRunStdoutDropped() const;
    quint64 selectedMixRunStderrDropped() const;
    quint64 sshRevision() const;
    QString sshState() const;
    QString sshError() const;
    quint32 sshActiveProbes() const;
    bool sshBusy() const;

    Q_INVOKABLE void popupOpened();
    Q_INVOKABLE void popupClosed();
    Q_INVOKABLE void refresh();
    Q_INVOKABLE void launchApp(const QString &slug);
    Q_INVOKABLE void controlDaemon(const QString &manager,
                                   const QString &unit,
                                   const QString &verb);
    Q_INVOKABLE void openLogs(const QString &manager, const QString &unit);
    Q_INVOKABLE void setBusVisible(bool visible);
    Q_INVOKABLE void setBusPaused(bool paused);
    Q_INVOKABLE void applyBusFilter(const QString &direction,
                                    const QString &verbGlob,
                                    const QString &bodyMode);
    Q_INVOKABLE void refreshBusRoster();
    Q_INVOKABLE void setMixVisible(bool visible);
    Q_INVOKABLE void setMixSearch(const QString &search);
    Q_INVOKABLE void createMixScript(const QString &name, const QString &description);
    Q_INVOKABLE void updateMixScript(const QString &scriptId,
                                     const QString &name,
                                     const QString &description);
    Q_INVOKABLE void trashMixScript(const QString &scriptId);
    Q_INVOKABLE void restoreMixScript(const QString &scriptId);
    Q_INVOKABLE void purgeMixScript(const QString &scriptId);
    Q_INVOKABLE void editMixScript(const QString &scriptId);
    Q_INVOKABLE void runMixScript(const QString &scriptId);
    Q_INVOKABLE void stopMixRun(const QString &runId);
    Q_INVOKABLE void selectMixRun(const QString &runId);
    Q_INVOKABLE void closeMixOutput();
    Q_INVOKABLE void connectSshHost(const QString &hostId);
    Q_INVOKABLE void probeSshHosts(const QStringList &hostIds);
    Q_INVOKABLE void createSshHost(const QString &name,
                                   const QString &hostname,
                                   quint32 port,
                                   const QString &user,
                                   const QString &keyId);
    Q_INVOKABLE void editSshHost(const QString &hostId);
    Q_INVOKABLE void trashSshHost(const QString &hostId);
    Q_INVOKABLE void restoreSshHost(const QString &hostId);
    Q_INVOKABLE void purgeSshHost(const QString &hostId);

Q_SIGNALS:
    void snapshotChanged();
    void connectionErrorChanged();
    void busyChanged();
    void busChanged();
    void busBusyChanged();
    void busPausedChanged();
    void busSessionChanged();
    void mixChanged();
    void mixBusyChanged();
    void mixSearchChanged();
    void selectedMixRunChanged();
    void sshChanged();
    void sshBusyChanged();

private Q_SLOTS:
    void onChanged(quint64 revision);
    void onServiceRegistered();
    void onServiceUnregistered();
    void onBusChanged(quint64 revision);
    void onBusTrafficBatch(quint64 revision,
                           quint64 filterEpoch,
                           const Cosmix::BusTrafficEntries &events,
                           quint64 serverDropped,
                           quint64 bridgeDropped);
    void onMixChanged(quint64 revision);
    void onMixRunChanged(quint64 revision, const QString &runId);
    void onMixRunOutput(quint64 revision,
                        const QString &runId,
                        const Cosmix::MixOutputChunks &chunks,
                        quint64 stdoutDropped,
                        quint64 stderrDropped);
    void onSshChanged(quint64 revision);

private:
    QDBusPendingCall asyncCall(const QString &method,
                               const QVariantList &arguments = {}) const;
    void callNoReply(const QString &method, const QVariantList &arguments) const;
    void requestSnapshot();
    void callVoidMethod(const QString &method,
                        const QVariantList &arguments,
                        bool refreshAfterSuccess = false);
    void installSnapshot(Snapshot snapshot);
    void setConnectionError(const QString &message);
    void setBusy(bool busy);
    void openBusSession();
    void closeBusSession();
    void keepBusSessionAlive();
    void sendBusFilterUpdate();
    void requestBusSnapshot(bool installTraffic);
    void installBusSnapshot(BusSnapshot snapshot, bool installTraffic);
    void requestMixSnapshot();
    void installMixSnapshot(MixSnapshot snapshot);
    void callMixVoidMethod(const QString &method,
                           const QVariantList &arguments,
                           bool refreshAfterSuccess);
    void setMixActionPending(bool pending);
    void requestSshSnapshot();
    void installSshSnapshot(SshSnapshot snapshot);
    void callSshVoidMethod(const QString &method,
                           const QVariantList &arguments,
                           bool refreshAfterSuccess);
    void setSshActionPending(bool pending);
    const MixRunEntry *selectedMixRun() const;
    QStringList effectiveDirectionArgument() const;
    QString conciseDbusError(QDBusPendingCallWatcher *watcher) const;

    QDBusConnection m_bus;
    QDBusServiceWatcher *m_serviceWatcher = nullptr;
    AppListModel m_apps;
    DaemonListModel m_systemDaemons;
    DaemonListModel m_userDaemons;
    BusNodeListModel m_ampNodes;
    BusTrafficListModel m_ampTraffic;
    MixScriptListModel m_mixScripts;
    MixScriptListModel m_mixTrash;
    MixRunListModel m_mixRuns;
    SshHostListModel m_sshHosts;
    SshHostListModel m_sshTrash;
    SshKeyListModel m_sshKeys;
    Snapshot m_snapshot;
    BusSnapshot m_ampSnapshot;
    MixSnapshot m_mixSnapshot;
    SshSnapshot m_sshSnapshot;
    QString m_connectionError;
    bool m_snapshotPending = false;
    bool m_snapshotFollowUp = false;
    bool m_opened = false;
    QTimer *m_ampKeepalive = nullptr;
    QString m_ampSessionId;
    QString m_ampFilterDirection = QStringLiteral("all");
    QString m_ampFilterVerb = QStringLiteral("*");
    QString m_ampFilterBody = QStringLiteral("none");
    bool m_ampDesired = false;
    bool m_ampOpenPending = false;
    quint64 m_ampOpenCallSerial = 0;
    quint64 m_ampActiveOpenCall = 0;
    bool m_ampClosePending = false;
    QSet<quint64> m_ampSnapshotPending;
    QHash<quint64, bool> m_ampSnapshotFollowUps;
    bool m_ampFilterTransition = false;
    bool m_ampFilterUpdatePending = false;
    bool m_ampFilterReplayPending = false;
    quint64 m_ampFilterCallSerial = 0;
    quint64 m_ampActiveFilterCall = 0;
    bool m_ampPaused = false;
    quint64 m_ampGeneration = 0;
    QString m_mixSearch;
    QString m_selectedMixRunId;
    bool m_mixDesired = false;
    bool m_mixSnapshotPending = false;
    bool m_mixSnapshotFollowUp = false;
    int m_mixPendingActions = 0;
    bool m_sshSnapshotPending = false;
    bool m_sshSnapshotFollowUp = false;
    int m_sshPendingActions = 0;
};

}
