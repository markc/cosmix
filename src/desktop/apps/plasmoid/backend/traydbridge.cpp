#include "traydbridge.h"

#include "dbustypes.h"

#include <QDBusError>
#include <QDBusMessage>
#include <QDBusPendingCall>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QDBusServiceWatcher>
#include <QLoggingCategory>
#include <QTimer>

#include <algorithm>
#include <utility>

namespace
{

constexpr auto serviceName = "dev.cosmix.trayd";
constexpr auto objectPath = "/dev/cosmix/trayd";
constexpr auto interfaceName = "dev.cosmix.trayd";

}

namespace Cosmix
{

TraydBridge::TraydBridge(QObject *parent)
    : QObject(parent)
    , m_bus(QDBusConnection::sessionBus())
    , m_serviceWatcher(new QDBusServiceWatcher(QString::fromLatin1(serviceName),
                                              m_bus,
                                              QDBusServiceWatcher::WatchForRegistration
                                                  | QDBusServiceWatcher::WatchForUnregistration,
                                              this))
    , m_apps(this)
    , m_systemDaemons(this)
    , m_userDaemons(this)
    , m_ampNodes(this)
    , m_ampTraffic(this)
    , m_mixScripts(this)
    , m_mixTrash(this)
    , m_mixRuns(this)
    , m_sshHosts(this)
    , m_sshTrash(this)
    , m_sshKeys(this)
    , m_ampKeepalive(new QTimer(this))
{
    registerDbusTypes();

    m_bus.connect(QString::fromLatin1(serviceName),
                  QString::fromLatin1(objectPath),
                  QString::fromLatin1(interfaceName),
                  QStringLiteral("Changed"),
                  this,
                  SLOT(onChanged(quint64)));
    m_bus.connect(QString::fromLatin1(serviceName),
                  QString::fromLatin1(objectPath),
                  QString::fromLatin1(interfaceName),
                  QStringLiteral("BusChanged"),
                  this,
                  SLOT(onBusChanged(quint64)));
    m_bus.connect(QString::fromLatin1(serviceName),
                  QString::fromLatin1(objectPath),
                  QString::fromLatin1(interfaceName),
                  QStringLiteral("BusTrafficBatch"),
                  this,
                  SLOT(onBusTrafficBatch(quint64,quint64,Cosmix::BusTrafficEntries,quint64,quint64)));
    m_bus.connect(QString::fromLatin1(serviceName),
                  QString::fromLatin1(objectPath),
                  QString::fromLatin1(interfaceName),
                  QStringLiteral("MixChanged"),
                  this,
                  SLOT(onMixChanged(quint64)));
    m_bus.connect(QString::fromLatin1(serviceName),
                  QString::fromLatin1(objectPath),
                  QString::fromLatin1(interfaceName),
                  QStringLiteral("MixRunChanged"),
                  this,
                  SLOT(onMixRunChanged(quint64,QString)));
    m_bus.connect(
        QString::fromLatin1(serviceName),
        QString::fromLatin1(objectPath),
        QString::fromLatin1(interfaceName),
        QStringLiteral("MixRunOutput"),
        this,
        SLOT(onMixRunOutput(quint64,QString,Cosmix::MixOutputChunks,quint64,quint64)));
    m_bus.connect(QString::fromLatin1(serviceName),
                  QString::fromLatin1(objectPath),
                  QString::fromLatin1(interfaceName),
                  QStringLiteral("SshChanged"),
                  this,
                  SLOT(onSshChanged(quint64)));
    connect(m_serviceWatcher,
            &QDBusServiceWatcher::serviceRegistered,
            this,
            &TraydBridge::onServiceRegistered);
    connect(m_serviceWatcher,
            &QDBusServiceWatcher::serviceUnregistered,
            this,
            &TraydBridge::onServiceUnregistered);

    m_ampKeepalive->setInterval(5 * 60 * 1000);
    m_ampKeepalive->setTimerType(Qt::VeryCoarseTimer);
    connect(m_ampKeepalive, &QTimer::timeout, this, &TraydBridge::keepBusSessionAlive);
}

TraydBridge::~TraydBridge()
{
    if (!m_ampSessionId.isEmpty()) {
        callNoReply(QStringLiteral("CloseBusSession"), {m_ampSessionId});
    }
}

QAbstractItemModel *TraydBridge::appsModel()
{
    return &m_apps;
}

QAbstractItemModel *TraydBridge::systemDaemonsModel()
{
    return &m_systemDaemons;
}

QAbstractItemModel *TraydBridge::userDaemonsModel()
{
    return &m_userDaemons;
}

QAbstractItemModel *TraydBridge::busNodesModel()
{
    return &m_ampNodes;
}

QAbstractItemModel *TraydBridge::busTrafficModel()
{
    return &m_ampTraffic;
}

QAbstractItemModel *TraydBridge::mixScriptsModel()
{
    return &m_mixScripts;
}

QAbstractItemModel *TraydBridge::mixTrashModel()
{
    return &m_mixTrash;
}

QAbstractItemModel *TraydBridge::mixRunsModel()
{
    return &m_mixRuns;
}

QAbstractItemModel *TraydBridge::sshHostsModel()
{
    return &m_sshHosts;
}

QAbstractItemModel *TraydBridge::sshTrashModel()
{
    return &m_sshTrash;
}

QAbstractItemModel *TraydBridge::sshKeysModel()
{
    return &m_sshKeys;
}

quint64 TraydBridge::revision() const
{
    return m_snapshot.revision;
}

bool TraydBridge::snapshotReady() const
{
    return m_snapshot.revision != 0;
}

bool TraydBridge::nodedChecked() const
{
    return m_snapshot.nodedChecked;
}

bool TraydBridge::nodedReachable() const
{
    return m_snapshot.nodedReachable;
}

QString TraydBridge::appsError() const
{
    return m_snapshot.appsError;
}

QString TraydBridge::daemonsError() const
{
    return m_snapshot.daemonsError;
}

QString TraydBridge::refreshError() const
{
    return m_snapshot.refreshError;
}

QString TraydBridge::connectionError() const
{
    return m_connectionError;
}

bool TraydBridge::busy() const
{
    return m_snapshotPending;
}

quint64 TraydBridge::busRevision() const
{
    return m_ampSnapshot.revision;
}

QString TraydBridge::busState() const
{
    return m_ampSnapshot.state.isEmpty() ? QStringLiteral("idle") : m_ampSnapshot.state;
}

QString TraydBridge::busError() const
{
    return m_ampSnapshot.error;
}

bool TraydBridge::busObserving() const
{
    return m_ampSnapshot.observing;
}

QStringList TraydBridge::busDirections() const
{
    return m_ampSnapshot.effectiveDirections;
}

QStringList TraydBridge::busVerbs() const
{
    return m_ampSnapshot.effectiveVerbs;
}

QString TraydBridge::busBodyMode() const
{
    return m_ampSnapshot.bodyMode;
}

QString TraydBridge::inventoryPosture() const
{
    return m_ampSnapshot.inventoryPosture;
}

QStringList TraydBridge::localServices() const
{
    return m_ampSnapshot.localServices;
}

quint64 TraydBridge::serverDropped() const
{
    return m_ampSnapshot.serverDropped;
}

quint64 TraydBridge::bridgeDropped() const
{
    return m_ampSnapshot.bridgeDropped;
}

bool TraydBridge::busBusy() const
{
    return m_ampOpenPending || m_ampClosePending || m_ampFilterUpdatePending
        || m_ampSnapshotPending.contains(m_ampGeneration);
}

bool TraydBridge::busPaused() const
{
    return m_ampPaused;
}

bool TraydBridge::busSessionOpen() const
{
    return !m_ampSessionId.isEmpty();
}

quint64 TraydBridge::mixRevision() const
{
    return m_mixSnapshot.revision;
}

QString TraydBridge::mixState() const
{
    return m_mixSnapshot.state.isEmpty() ? QStringLiteral("absent") : m_mixSnapshot.state;
}

QString TraydBridge::mixError() const
{
    return m_mixSnapshot.error;
}

quint32 TraydBridge::mixActiveRuns() const
{
    return m_mixSnapshot.activeRuns;
}

bool TraydBridge::mixBusy() const
{
    return m_mixSnapshotPending || m_mixPendingActions > 0;
}

QString TraydBridge::mixSearch() const
{
    return m_mixSearch;
}

QString TraydBridge::selectedMixRunId() const
{
    return m_selectedMixRunId;
}

QString TraydBridge::selectedMixRunName() const
{
    const auto *run = selectedMixRun();
    return run == nullptr ? QString{} : run->scriptName;
}

QString TraydBridge::selectedMixRunState() const
{
    const auto *run = selectedMixRun();
    return run == nullptr ? QString{} : run->state;
}

QString TraydBridge::selectedMixRunStdout() const
{
    const auto *run = selectedMixRun();
    return run == nullptr ? QString{} : run->stdoutText;
}

QString TraydBridge::selectedMixRunStderr() const
{
    const auto *run = selectedMixRun();
    return run == nullptr ? QString{} : run->stderrText;
}

bool TraydBridge::selectedMixRunActive() const
{
    const auto state = selectedMixRunState();
    return state == QStringLiteral("starting") || state == QStringLiteral("running")
        || state == QStringLiteral("stopping");
}

bool TraydBridge::selectedMixRunHasExitCode() const
{
    const auto *run = selectedMixRun();
    return run != nullptr && run->hasExitCode;
}

qint32 TraydBridge::selectedMixRunExitCode() const
{
    const auto *run = selectedMixRun();
    return run == nullptr ? 0 : run->exitCode;
}

quint64 TraydBridge::selectedMixRunStdoutDropped() const
{
    const auto *run = selectedMixRun();
    return run == nullptr ? 0 : run->stdoutDropped;
}

quint64 TraydBridge::selectedMixRunStderrDropped() const
{
    const auto *run = selectedMixRun();
    return run == nullptr ? 0 : run->stderrDropped;
}

quint64 TraydBridge::sshRevision() const
{
    return m_sshSnapshot.revision;
}

QString TraydBridge::sshState() const
{
    return m_sshSnapshot.state.isEmpty() ? QStringLiteral("absent") : m_sshSnapshot.state;
}

QString TraydBridge::sshError() const
{
    return m_sshSnapshot.error;
}

quint32 TraydBridge::sshActiveProbes() const
{
    return m_sshSnapshot.activeProbes;
}

bool TraydBridge::sshBusy() const
{
    return m_sshSnapshotPending || m_sshPendingActions > 0;
}

void TraydBridge::popupOpened()
{
    m_opened = true;
    requestSnapshot();
    requestSshSnapshot();
    refresh();
}

void TraydBridge::popupClosed()
{
    m_opened = false;
    setBusVisible(false);
    setMixVisible(false);
}

void TraydBridge::refresh()
{
    callVoidMethod(QStringLiteral("Refresh"), {});
}

void TraydBridge::launchApp(const QString &slug)
{
    if (slug.isEmpty()) {
        setConnectionError(QStringLiteral("Cannot launch an application without a slug."));
        return;
    }
    callVoidMethod(QStringLiteral("LaunchApp"), {slug});
}

void TraydBridge::controlDaemon(const QString &manager,
                                const QString &unit,
                                const QString &verb)
{
    if (manager.isEmpty() || unit.isEmpty()
        || (verb != QStringLiteral("start") && verb != QStringLiteral("stop")
            && verb != QStringLiteral("restart"))) {
        setConnectionError(QStringLiteral("Invalid daemon control identity."));
        return;
    }
    callVoidMethod(QStringLiteral("ControlDaemon"), {manager, unit, verb}, true);
}

void TraydBridge::openLogs(const QString &manager, const QString &unit)
{
    if (manager.isEmpty() || unit.isEmpty()) {
        setConnectionError(QStringLiteral("Invalid daemon log identity."));
        return;
    }
    callVoidMethod(QStringLiteral("OpenLogs"), {manager, unit});
}

void TraydBridge::setBusVisible(bool visible)
{
    if (m_ampDesired == visible) {
        return;
    }
    m_ampDesired = visible;
    ++m_ampGeneration;
    if (visible) {
        openBusSession();
    } else {
        closeBusSession();
    }
}

void TraydBridge::setBusPaused(bool paused)
{
    if (m_ampPaused == paused) {
        return;
    }
    m_ampPaused = paused;
    Q_EMIT busPausedChanged();
    if (!paused && !m_ampSessionId.isEmpty()) {
        requestBusSnapshot(true);
    }
}

void TraydBridge::applyBusFilter(const QString &direction,
                                 const QString &verbGlob,
                                 const QString &bodyMode)
{
    const QStringList allowedDirections = {
        QStringLiteral("all"),
        QStringLiteral("local"),
        QStringLiteral("mesh_in"),
        QStringLiteral("mesh_out"),
    };
    if (!allowedDirections.contains(direction)) {
        setConnectionError(QStringLiteral("Invalid Bus direction."));
        return;
    }
    const auto verb = verbGlob.trimmed();
    if (verb.isEmpty()) {
        setConnectionError(QStringLiteral("Bus verb filter cannot be empty."));
        return;
    }
    if (bodyMode != QStringLiteral("none") && bodyMode != QStringLiteral("redacted")) {
        setConnectionError(QStringLiteral("Invalid Bus body mode."));
        return;
    }

    if (m_ampFilterDirection == direction && m_ampFilterVerb == verb
        && m_ampFilterBody == bodyMode) {
        return;
    }
    m_ampFilterDirection = direction;
    m_ampFilterVerb = verb;
    m_ampFilterBody = bodyMode;
    m_ampTraffic.clear();
    ++m_ampGeneration;
    m_ampFilterTransition = true;
    if (m_ampSessionId.isEmpty()) {
        // OpenBusSession captures its filter generation. If it is currently
        // pending, this advance fences that reply: the stale lease is closed
        // and openBusSession() immediately retries with the latest desired
        // filter. With no call pending, an explicit correction is the bounded
        // recovery edge after a rejected open.
        if (!m_ampOpenPending && m_ampDesired && !m_ampClosePending) {
            openBusSession();
        }
        return;
    }

    if (m_ampFilterUpdatePending) {
        // Controls remain editable while an update is in flight. Coalesce
        // every intermediate selection and replay only the latest desired
        // filter after the current D-Bus call completes.
        m_ampFilterReplayPending = true;
        return;
    }
    sendBusFilterUpdate();
}

void TraydBridge::sendBusFilterUpdate()
{
    if (m_ampSessionId.isEmpty() || !m_ampDesired) {
        return;
    }
    m_ampFilterReplayPending = false;
    m_ampFilterUpdatePending = true;
    const auto callSerial = ++m_ampFilterCallSerial;
    m_ampActiveFilterCall = callSerial;
    const auto generation = m_ampGeneration;
    const auto session = m_ampSessionId;
    const auto directions = effectiveDirectionArgument();
    const auto verb = m_ampFilterVerb;
    const auto body = m_ampFilterBody;
    Q_EMIT busBusyChanged();
    auto *watcher = new QDBusPendingCallWatcher(
        asyncCall(QStringLiteral("UpdateBusSession"),
                  {session, directions, verb, body}),
        this);
    connect(watcher,
            &QDBusPendingCallWatcher::finished,
            this,
            [this, watcher, callSerial, generation, session] {
        const QDBusPendingReply<> reply = *watcher;
        if (m_ampActiveFilterCall != callSerial) {
            watcher->deleteLater();
            return;
        }
        m_ampActiveFilterCall = 0;
        const bool sameSession = m_ampDesired && session == m_ampSessionId;
        const bool replay = sameSession
            && (m_ampFilterReplayPending || generation != m_ampGeneration);
        m_ampFilterUpdatePending = false;
        if (replay) {
            sendBusFilterUpdate();
        } else {
            m_ampFilterReplayPending = false;
            Q_EMIT busBusyChanged();
        }
        if (!sameSession || replay) {
            watcher->deleteLater();
            return;
        }
        if (reply.isError()) {
            setConnectionError(conciseDbusError(watcher));
            requestBusSnapshot(true);
        } else {
            setConnectionError({});
            requestBusSnapshot(true);
        }
        watcher->deleteLater();
    });
}

void TraydBridge::refreshBusRoster()
{
    if (m_ampSessionId.isEmpty()) {
        return;
    }
    callVoidMethod(QStringLiteral("RefreshBusRoster"), {m_ampSessionId});
}

void TraydBridge::setMixVisible(bool visible)
{
    if (m_mixDesired == visible) {
        return;
    }
    m_mixDesired = visible;
    if (visible) {
        requestMixSnapshot();
    }
}

void TraydBridge::setMixSearch(const QString &search)
{
    const auto normalised = search.trimmed();
    if (m_mixSearch == normalised) {
        return;
    }
    m_mixSearch = normalised;
    m_mixScripts.setFilter(normalised);
    m_mixTrash.setFilter(normalised);
    Q_EMIT mixSearchChanged();
}

void TraydBridge::createMixScript(const QString &name, const QString &description)
{
    const auto trimmedName = name.trimmed();
    if (trimmedName.isEmpty()) {
        setConnectionError(QStringLiteral("A Mix script needs a name."));
        return;
    }
    setMixActionPending(true);
    auto *watcher = new QDBusPendingCallWatcher(
        asyncCall(QStringLiteral("CreateMixScript"),
                  {trimmedName, description.trimmed()}),
        this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, watcher] {
        const QDBusPendingReply<QString> reply = *watcher;
        if (reply.isError()) {
            setConnectionError(conciseDbusError(watcher));
        } else {
            setConnectionError({});
            requestMixSnapshot();
            editMixScript(reply.value());
        }
        setMixActionPending(false);
        watcher->deleteLater();
    });
}

void TraydBridge::updateMixScript(const QString &scriptId,
                                  const QString &name,
                                  const QString &description)
{
    if (scriptId.isEmpty() || name.trimmed().isEmpty()) {
        setConnectionError(QStringLiteral("Invalid Mix script identity or name."));
        return;
    }
    callMixVoidMethod(QStringLiteral("UpdateMixScript"),
                      {scriptId, name.trimmed(), description.trimmed()},
                      true);
}

void TraydBridge::trashMixScript(const QString &scriptId)
{
    if (scriptId.isEmpty()) {
        setConnectionError(QStringLiteral("Invalid Mix script identity."));
        return;
    }
    callMixVoidMethod(QStringLiteral("TrashMixScript"), {scriptId}, true);
}

void TraydBridge::restoreMixScript(const QString &scriptId)
{
    if (scriptId.isEmpty()) {
        setConnectionError(QStringLiteral("Invalid Mix script identity."));
        return;
    }
    callMixVoidMethod(QStringLiteral("RestoreMixScript"), {scriptId}, true);
}

void TraydBridge::purgeMixScript(const QString &scriptId)
{
    if (scriptId.isEmpty()) {
        setConnectionError(QStringLiteral("Invalid Mix script identity."));
        return;
    }
    callMixVoidMethod(QStringLiteral("PurgeMixScript"), {scriptId}, true);
}

void TraydBridge::editMixScript(const QString &scriptId)
{
    if (scriptId.isEmpty()) {
        setConnectionError(QStringLiteral("Invalid Mix script identity."));
        return;
    }
    callMixVoidMethod(QStringLiteral("EditMixScript"), {scriptId}, false);
}

void TraydBridge::runMixScript(const QString &scriptId)
{
    if (scriptId.isEmpty()) {
        setConnectionError(QStringLiteral("Invalid Mix script identity."));
        return;
    }
    setMixActionPending(true);
    auto *watcher = new QDBusPendingCallWatcher(
        asyncCall(QStringLiteral("RunMixScript"), {scriptId}), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, watcher] {
        const QDBusPendingReply<QString> reply = *watcher;
        if (reply.isError()) {
            setConnectionError(conciseDbusError(watcher));
        } else {
            setConnectionError({});
            m_selectedMixRunId = reply.value();
            Q_EMIT selectedMixRunChanged();
            requestMixSnapshot();
        }
        setMixActionPending(false);
        watcher->deleteLater();
    });
}

void TraydBridge::stopMixRun(const QString &runId)
{
    if (runId.isEmpty()) {
        setConnectionError(QStringLiteral("Invalid Mix run identity."));
        return;
    }
    callMixVoidMethod(QStringLiteral("StopMixRun"), {runId}, true);
}

void TraydBridge::selectMixRun(const QString &runId)
{
    if (m_selectedMixRunId == runId) {
        return;
    }
    m_selectedMixRunId = runId;
    Q_EMIT selectedMixRunChanged();
}

void TraydBridge::closeMixOutput()
{
    selectMixRun({});
}

void TraydBridge::connectSshHost(const QString &hostId)
{
    if (hostId.isEmpty()) {
        setConnectionError(QStringLiteral("Invalid SSH host identity."));
        return;
    }
    callSshVoidMethod(QStringLiteral("ConnectSshHost"), {hostId}, false);
}

void TraydBridge::probeSshHosts(const QStringList &hostIds)
{
    callSshVoidMethod(QStringLiteral("ProbeSshHosts"), {hostIds}, true);
}

void TraydBridge::createSshHost(const QString &name,
                                const QString &hostname,
                                quint32 port,
                                const QString &user,
                                const QString &keyId)
{
    if (name.trimmed().isEmpty() || hostname.trimmed().isEmpty()
        || keyId.trimmed().isEmpty()) {
        setConnectionError(QStringLiteral("A new SSH host needs a name, hostname and key."));
        return;
    }
    callSshVoidMethod(QStringLiteral("CreateSshHost"),
                      {name.trimmed(),
                       hostname.trimmed(),
                       port,
                       user.trimmed(),
                       keyId.trimmed()},
                      true);
}

void TraydBridge::editSshHost(const QString &hostId)
{
    if (hostId.isEmpty()) {
        setConnectionError(QStringLiteral("Invalid SSH host identity."));
        return;
    }
    callSshVoidMethod(QStringLiteral("EditSshHost"), {hostId}, false);
}

void TraydBridge::trashSshHost(const QString &hostId)
{
    if (hostId.isEmpty()) {
        setConnectionError(QStringLiteral("Invalid SSH host identity."));
        return;
    }
    callSshVoidMethod(QStringLiteral("TrashSshHost"), {hostId}, true);
}

void TraydBridge::restoreSshHost(const QString &hostId)
{
    if (hostId.isEmpty()) {
        setConnectionError(QStringLiteral("Invalid SSH host identity."));
        return;
    }
    callSshVoidMethod(QStringLiteral("RestoreSshHost"), {hostId}, true);
}

void TraydBridge::purgeSshHost(const QString &hostId)
{
    if (hostId.isEmpty()) {
        setConnectionError(QStringLiteral("Invalid SSH host identity."));
        return;
    }
    callSshVoidMethod(QStringLiteral("PurgeSshHost"), {hostId}, true);
}

void TraydBridge::onChanged(quint64 revision)
{
    if (!m_opened || revision == m_snapshot.revision) {
        return;
    }
    requestSnapshot();
}

void TraydBridge::onServiceRegistered()
{
    if (m_opened) {
        requestSnapshot();
    }
    if (m_ampDesired) {
        openBusSession();
    }
    if (m_mixDesired) {
        requestMixSnapshot();
    }
    if (m_opened) {
        requestSshSnapshot();
    }
}

void TraydBridge::onServiceUnregistered()
{
    m_ampKeepalive->stop();
    ++m_ampGeneration;
    m_ampSessionId.clear();
    m_ampOpenPending = false;
    m_ampActiveOpenCall = 0;
    ++m_ampOpenCallSerial;
    m_ampClosePending = false;
    m_ampSnapshotPending.clear();
    m_ampSnapshotFollowUps.clear();
    m_ampFilterTransition = false;
    m_ampFilterUpdatePending = false;
    m_ampFilterReplayPending = false;
    m_ampActiveFilterCall = 0;
    ++m_ampFilterCallSerial;
    m_ampSnapshot = {};
    m_ampSnapshot.state = QStringLiteral("unavailable");
    m_ampTraffic.clear();
    m_ampNodes.replace(BusNodeEntries{});
    m_mixSnapshotPending = false;
    m_mixSnapshotFollowUp = false;
    m_mixSnapshot = {};
    m_mixSnapshot.state = QStringLiteral("absent");
    m_mixScripts.replace(MixScriptEntries{});
    m_mixTrash.replace(MixScriptEntries{});
    m_mixRuns.replace(MixRunEntries{});
    m_selectedMixRunId.clear();
    m_sshSnapshotPending = false;
    m_sshSnapshotFollowUp = false;
    m_sshSnapshot = {};
    m_sshSnapshot.state = QStringLiteral("absent");
    m_sshHosts.replace(SshHostEntries{});
    m_sshTrash.replace(SshHostEntries{});
    m_sshKeys.replace(SshKeyEntries{});
    Q_EMIT busBusyChanged();
    Q_EMIT busSessionChanged();
    Q_EMIT busChanged();
    Q_EMIT mixBusyChanged();
    Q_EMIT mixChanged();
    Q_EMIT selectedMixRunChanged();
    Q_EMIT sshBusyChanged();
    Q_EMIT sshChanged();
    setConnectionError(QStringLiteral("CosMix tray daemon is unavailable."));
}

void TraydBridge::onBusChanged(quint64 revision)
{
    if (m_ampSessionId.isEmpty() || revision == m_ampSnapshot.revision) {
        return;
    }
    requestBusSnapshot(!m_ampPaused);
}

void TraydBridge::onBusTrafficBatch(quint64 revision,
                                    quint64 filterEpoch,
                                    const BusTrafficEntries &events,
                                    quint64 serverDropped,
                                    quint64 bridgeDropped)
{
    if (m_ampSessionId.isEmpty() || m_ampFilterTransition
        || filterEpoch != m_ampSnapshot.filterEpoch) {
        if (!m_ampSessionId.isEmpty() && filterEpoch > m_ampSnapshot.filterEpoch) {
            requestBusSnapshot(!m_ampPaused);
        }
        return;
    }
    m_ampSnapshot.revision = std::max(m_ampSnapshot.revision, revision);
    m_ampSnapshot.serverDropped = serverDropped;
    m_ampSnapshot.bridgeDropped = bridgeDropped;
    if (!m_ampPaused) {
        m_ampTraffic.appendBatch(events);
    }
    Q_EMIT busChanged();
}

void TraydBridge::onMixChanged(quint64 revision)
{
    if (!m_mixDesired || revision == m_mixSnapshot.revision) {
        return;
    }
    requestMixSnapshot();
}

void TraydBridge::onMixRunChanged(quint64 revision, const QString &runId)
{
    Q_UNUSED(revision)
    Q_UNUSED(runId)
    if (m_mixDesired) {
        requestMixSnapshot();
    }
}

void TraydBridge::onMixRunOutput(quint64 revision,
                                 const QString &runId,
                                 const MixOutputChunks &chunks,
                                 quint64 stdoutDropped,
                                 quint64 stderrDropped)
{
    if (!m_mixDesired) {
        return;
    }
    m_mixSnapshot.revision = std::max(m_mixSnapshot.revision, revision);
    if (!m_mixRuns.appendOutput(runId, chunks, stdoutDropped, stderrDropped)) {
        requestMixSnapshot();
    }
    if (runId == m_selectedMixRunId) {
        Q_EMIT selectedMixRunChanged();
    }
    Q_EMIT mixChanged();
}

void TraydBridge::onSshChanged(quint64 revision)
{
    if (revision == m_sshSnapshot.revision) {
        return;
    }
    requestSshSnapshot();
}

void TraydBridge::requestSnapshot()
{
    if (m_snapshotPending) {
        m_snapshotFollowUp = true;
        return;
    }

    setBusy(true);
    auto *watcher = new QDBusPendingCallWatcher(
        asyncCall(QStringLiteral("GetSnapshot")), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, watcher] {
        const QDBusPendingReply<Snapshot> reply = *watcher;
        if (reply.isError()) {
            setConnectionError(conciseDbusError(watcher));
        } else {
            installSnapshot(reply.value());
            setConnectionError({});
        }
        setBusy(false);
        watcher->deleteLater();
        if (std::exchange(m_snapshotFollowUp, false)) {
            requestSnapshot();
        }
    });
}

void TraydBridge::callVoidMethod(const QString &method,
                                 const QVariantList &arguments,
                                 bool refreshAfterSuccess)
{
    auto *watcher = new QDBusPendingCallWatcher(
        asyncCall(method, arguments), this);
    connect(watcher,
            &QDBusPendingCallWatcher::finished,
            this,
            [this, watcher, refreshAfterSuccess] {
                const QDBusPendingReply<> reply = *watcher;
                if (reply.isError()) {
                    setConnectionError(conciseDbusError(watcher));
                } else {
                    setConnectionError({});
                    if (refreshAfterSuccess) {
                        refresh();
                    }
                }
                watcher->deleteLater();
            });
}

void TraydBridge::installSnapshot(Snapshot snapshot)
{
    AppEntries apps = snapshot.apps;
    DaemonEntries systemDaemons;
    DaemonEntries userDaemons;
    systemDaemons.reserve(snapshot.daemons.size());
    userDaemons.reserve(snapshot.daemons.size());

    for (const auto &daemon : snapshot.daemons) {
        if (daemon.manager == QStringLiteral("system")) {
            systemDaemons.append(daemon);
        } else if (daemon.manager == QStringLiteral("user")) {
            userDaemons.append(daemon);
        }
    }

    m_apps.replace(std::move(apps));
    m_systemDaemons.replace(std::move(systemDaemons));
    m_userDaemons.replace(std::move(userDaemons));
    m_snapshot = std::move(snapshot);
    Q_EMIT snapshotChanged();
}

void TraydBridge::setConnectionError(const QString &message)
{
    if (m_connectionError == message) {
        return;
    }
    m_connectionError = message;
    Q_EMIT connectionErrorChanged();
}

void TraydBridge::setBusy(bool busy)
{
    if (m_snapshotPending == busy) {
        return;
    }
    m_snapshotPending = busy;
    Q_EMIT busyChanged();
}

QString TraydBridge::conciseDbusError(QDBusPendingCallWatcher *watcher) const
{
    const QDBusError error = watcher->error();
    if (!error.message().isEmpty()) {
        return error.message();
    }
    return QStringLiteral("CosMix tray daemon call failed.");
}

QDBusPendingCall TraydBridge::asyncCall(const QString &method,
                                        const QVariantList &arguments) const
{
    auto message = QDBusMessage::createMethodCall(QString::fromLatin1(serviceName),
                                                  QString::fromLatin1(objectPath),
                                                  QString::fromLatin1(interfaceName),
                                                  method);
    message.setArguments(arguments);
    return m_bus.asyncCall(message);
}

void TraydBridge::callNoReply(const QString &method,
                              const QVariantList &arguments) const
{
    auto message = QDBusMessage::createMethodCall(QString::fromLatin1(serviceName),
                                                  QString::fromLatin1(objectPath),
                                                  QString::fromLatin1(interfaceName),
                                                  method);
    message.setArguments(arguments);
    m_bus.call(message, QDBus::NoBlock);
}

void TraydBridge::openBusSession()
{
    if (!m_ampDesired || m_ampOpenPending || m_ampClosePending
        || !m_ampSessionId.isEmpty()) {
        return;
    }
    m_ampOpenPending = true;
    const auto callSerial = ++m_ampOpenCallSerial;
    m_ampActiveOpenCall = callSerial;
    Q_EMIT busBusyChanged();
    const auto generation = m_ampGeneration;
    auto *watcher = new QDBusPendingCallWatcher(
        asyncCall(QStringLiteral("OpenBusSession"),
                  {effectiveDirectionArgument(), m_ampFilterVerb, m_ampFilterBody}),
        this);
    connect(watcher,
            &QDBusPendingCallWatcher::finished,
            this,
            [this, watcher, callSerial, generation] {
        const QDBusPendingReply<QString> reply = *watcher;
        if (m_ampActiveOpenCall != callSerial) {
            if (!reply.isError()) {
                asyncCall(QStringLiteral("CloseBusSession"), {reply.value()});
            }
            watcher->deleteLater();
            return;
        }
        m_ampActiveOpenCall = 0;
        m_ampOpenPending = false;
        const bool current = generation == m_ampGeneration && m_ampDesired
            && m_ampSessionId.isEmpty();
        const bool replayStaleCompletion =
            !current && m_ampDesired && m_ampSessionId.isEmpty();
        if (reply.isError() && current) {
            setConnectionError(conciseDbusError(watcher));
        } else if (!reply.isError() && current) {
            m_ampSessionId = reply.value();
            m_ampFilterTransition = true;
            m_ampKeepalive->start();
            setConnectionError({});
            Q_EMIT busSessionChanged();
            requestBusSnapshot(true);
        } else if (!reply.isError()) {
            // A hidden/reopened popup fences the reply, but the sender-bound
            // lease still exists server-side. Close it without installing it.
            asyncCall(QStringLiteral("CloseBusSession"), {reply.value()});
        }
        Q_EMIT busBusyChanged();
        watcher->deleteLater();
        // A stale failure has no lease to close, but it consumed the filter or
        // service-recovery edge that fenced it. Replay once at the current
        // generation; a current-generation failure stops here.
        if (replayStaleCompletion && m_ampSessionId.isEmpty() && !m_ampOpenPending
            && !m_ampClosePending) {
            openBusSession();
        }
    });
}

void TraydBridge::closeBusSession()
{
    m_ampKeepalive->stop();
    m_ampSnapshotFollowUps.remove(m_ampGeneration);
    m_ampFilterTransition = false;
    m_ampFilterUpdatePending = false;
    m_ampFilterReplayPending = false;
    m_ampActiveFilterCall = 0;
    ++m_ampFilterCallSerial;
    if (m_ampOpenPending || m_ampClosePending || m_ampSessionId.isEmpty()) {
        return;
    }
    m_ampClosePending = true;
    Q_EMIT busBusyChanged();
    const auto closingSession = m_ampSessionId;
    auto *watcher = new QDBusPendingCallWatcher(
        asyncCall(QStringLiteral("CloseBusSession"), {closingSession}), this);
    connect(watcher,
            &QDBusPendingCallWatcher::finished,
            this,
            [this, watcher, closingSession] {
                const QDBusPendingReply<> reply = *watcher;
                if (reply.isError()
                    && watcher->error().name()
                        != QStringLiteral("dev.cosmix.trayd.Error.UnknownBusSession")) {
                    setConnectionError(conciseDbusError(watcher));
                } else {
                    setConnectionError({});
                }
                if (m_ampSessionId == closingSession) {
                    m_ampSessionId.clear();
                    Q_EMIT busSessionChanged();
                }
                m_ampClosePending = false;
                if (!m_ampDesired) {
                    m_ampSnapshot.observing = false;
                    m_ampSnapshot.state = QStringLiteral("idle");
                }
                Q_EMIT busBusyChanged();
                Q_EMIT busChanged();
                watcher->deleteLater();
                if (m_ampDesired) {
                    openBusSession();
                }
            });
}

void TraydBridge::keepBusSessionAlive()
{
    if (m_ampSessionId.isEmpty() || !m_ampDesired) {
        return;
    }
    callVoidMethod(QStringLiteral("KeepBusSessionAlive"), {m_ampSessionId});
}

void TraydBridge::requestBusSnapshot(bool installTraffic)
{
    if (m_ampSessionId.isEmpty()) {
        return;
    }
    const auto generation = m_ampGeneration;
    if (m_ampFilterUpdatePending) {
        m_ampSnapshotFollowUps.insert(
            generation,
            m_ampSnapshotFollowUps.value(generation, false) || installTraffic);
        return;
    }
    if (m_ampSnapshotPending.contains(generation)) {
        m_ampSnapshotFollowUps.insert(
            generation,
            m_ampSnapshotFollowUps.value(generation, false) || installTraffic);
        return;
    }
    m_ampSnapshotPending.insert(generation);
    Q_EMIT busBusyChanged();
    const auto session = m_ampSessionId;
    auto *watcher = new QDBusPendingCallWatcher(
        asyncCall(QStringLiteral("GetBusSnapshot")), this);
    connect(watcher,
            &QDBusPendingCallWatcher::finished,
            this,
            [this, watcher, installTraffic, generation, session] {
                const QDBusPendingReply<BusSnapshot> reply = *watcher;
                const bool current = generation == m_ampGeneration && m_ampDesired
                    && !session.isEmpty() && session == m_ampSessionId;
                if (!current) {
                    // A close, replacement lease, or hidden popup fences this reply.
                } else if (reply.isError()) {
                    setConnectionError(conciseDbusError(watcher));
                } else if (reply.value().revision >= m_ampSnapshot.revision) {
                    installBusSnapshot(reply.value(), installTraffic && !m_ampPaused);
                    setConnectionError({});
                }
                m_ampSnapshotPending.remove(generation);
                const bool followUp = m_ampSnapshotFollowUps.contains(generation);
                const bool followUpTraffic = m_ampSnapshotFollowUps.take(generation);
                Q_EMIT busBusyChanged();
                watcher->deleteLater();
                if (current && followUp) {
                    requestBusSnapshot(followUpTraffic);
                } else if (!current) {
                    // A stale completion owns only its captured generation.
                    // If the current session has no request, schedule it now.
                    requestBusSnapshot(!m_ampPaused);
                }
            });
}

void TraydBridge::installBusSnapshot(BusSnapshot snapshot, bool installTraffic)
{
    if (snapshot.inventoryPosture == QStringLiteral("verified")) {
        m_ampNodes.replace(snapshot.nodes);
    } else {
        m_ampNodes.replace(BusNodeEntries{});
    }
    if (installTraffic) {
        m_ampTraffic.replace(snapshot.traffic);
    }
    m_ampFilterTransition = false;
    m_ampSnapshot = std::move(snapshot);
    Q_EMIT busChanged();
}

void TraydBridge::requestMixSnapshot()
{
    if (!m_mixDesired) {
        return;
    }
    if (m_mixSnapshotPending) {
        m_mixSnapshotFollowUp = true;
        return;
    }
    m_mixSnapshotPending = true;
    Q_EMIT mixBusyChanged();
    auto *watcher = new QDBusPendingCallWatcher(
        asyncCall(QStringLiteral("GetMixSnapshot")), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, watcher] {
        const QDBusPendingReply<MixSnapshot> reply = *watcher;
        if (reply.isError()) {
            setConnectionError(conciseDbusError(watcher));
        } else if (reply.value().revision >= m_mixSnapshot.revision) {
            installMixSnapshot(reply.value());
            setConnectionError({});
        }
        m_mixSnapshotPending = false;
        Q_EMIT mixBusyChanged();
        watcher->deleteLater();
        if (std::exchange(m_mixSnapshotFollowUp, false)) {
            requestMixSnapshot();
        }
    });
}

void TraydBridge::installMixSnapshot(MixSnapshot snapshot)
{
    MixScriptEntries active;
    MixScriptEntries trash;
    active.reserve(snapshot.scripts.size());
    trash.reserve(snapshot.scripts.size());
    for (auto &script : snapshot.scripts) {
        if (script.trashed) {
            trash.append(std::move(script));
        } else {
            active.append(std::move(script));
        }
    }
    m_mixScripts.replace(std::move(active));
    m_mixTrash.replace(std::move(trash));
    m_mixRuns.replace(snapshot.runs);
    if (!m_selectedMixRunId.isEmpty()
        && m_mixRuns.find(m_selectedMixRunId) == nullptr) {
        m_selectedMixRunId.clear();
    }
    m_mixSnapshot = std::move(snapshot);
    Q_EMIT mixChanged();
    Q_EMIT selectedMixRunChanged();
}

void TraydBridge::callMixVoidMethod(const QString &method,
                                    const QVariantList &arguments,
                                    bool refreshAfterSuccess)
{
    setMixActionPending(true);
    auto *watcher = new QDBusPendingCallWatcher(
        asyncCall(method, arguments), this);
    connect(watcher,
            &QDBusPendingCallWatcher::finished,
            this,
            [this, watcher, refreshAfterSuccess] {
                const QDBusPendingReply<> reply = *watcher;
                if (reply.isError()) {
                    setConnectionError(conciseDbusError(watcher));
                } else {
                    setConnectionError({});
                    if (refreshAfterSuccess) {
                        requestMixSnapshot();
                    }
                }
                setMixActionPending(false);
                watcher->deleteLater();
            });
}

void TraydBridge::setMixActionPending(bool pending)
{
    const bool wasBusy = mixBusy();
    m_mixPendingActions = std::max(0, m_mixPendingActions + (pending ? 1 : -1));
    if (wasBusy != mixBusy()) {
        Q_EMIT mixBusyChanged();
    }
}

void TraydBridge::requestSshSnapshot()
{
    if (m_sshSnapshotPending) {
        m_sshSnapshotFollowUp = true;
        return;
    }
    m_sshSnapshotPending = true;
    Q_EMIT sshBusyChanged();
    auto *watcher = new QDBusPendingCallWatcher(
        asyncCall(QStringLiteral("GetSshSnapshot")), this);
    connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, watcher] {
        const QDBusPendingReply<SshSnapshot> reply = *watcher;
        if (reply.isError()) {
            setConnectionError(conciseDbusError(watcher));
        } else if (reply.value().revision >= m_sshSnapshot.revision) {
            installSshSnapshot(reply.value());
            setConnectionError({});
        }
        m_sshSnapshotPending = false;
        Q_EMIT sshBusyChanged();
        watcher->deleteLater();
        if (std::exchange(m_sshSnapshotFollowUp, false)) {
            requestSshSnapshot();
        }
    });
}

void TraydBridge::installSshSnapshot(SshSnapshot snapshot)
{
    SshHostEntries active;
    SshHostEntries trash;
    active.reserve(snapshot.hosts.size());
    trash.reserve(snapshot.hosts.size());
    for (auto &host : snapshot.hosts) {
        if (host.trashed) {
            trash.append(std::move(host));
        } else {
            active.append(std::move(host));
        }
    }
    m_sshHosts.replace(std::move(active));
    m_sshTrash.replace(std::move(trash));
    m_sshKeys.replace(snapshot.keys);
    m_sshSnapshot = std::move(snapshot);
    Q_EMIT sshChanged();
}

void TraydBridge::callSshVoidMethod(const QString &method,
                                    const QVariantList &arguments,
                                    bool refreshAfterSuccess)
{
    setSshActionPending(true);
    auto *watcher = new QDBusPendingCallWatcher(asyncCall(method, arguments), this);
    connect(watcher,
            &QDBusPendingCallWatcher::finished,
            this,
            [this, watcher, refreshAfterSuccess] {
                const QDBusPendingReply<> reply = *watcher;
                if (reply.isError()) {
                    setConnectionError(conciseDbusError(watcher));
                } else {
                    setConnectionError({});
                    if (refreshAfterSuccess) {
                        requestSshSnapshot();
                    }
                }
                setSshActionPending(false);
                watcher->deleteLater();
            });
}

void TraydBridge::setSshActionPending(bool pending)
{
    const bool wasBusy = sshBusy();
    m_sshPendingActions = std::max(0, m_sshPendingActions + (pending ? 1 : -1));
    if (wasBusy != sshBusy()) {
        Q_EMIT sshBusyChanged();
    }
}

const MixRunEntry *TraydBridge::selectedMixRun() const
{
    return m_mixRuns.find(m_selectedMixRunId);
}

QStringList TraydBridge::effectiveDirectionArgument() const
{
    if (m_ampFilterDirection == QStringLiteral("all")) {
        return {
            QStringLiteral("local"),
            QStringLiteral("mesh_in"),
            QStringLiteral("mesh_out"),
        };
    }
    return {m_ampFilterDirection};
}

}
