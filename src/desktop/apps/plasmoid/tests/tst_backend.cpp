#include "dbustypes.h"
#include "listmodels.h"
#include "traydbridge.h"

#include <QDBusConnection>
#include <QDBusContext>
#include <QDBusInterface>
#include <QDBusMessage>
#include <QDBusMetaType>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QDBusVirtualObject>
#include <QElapsedTimer>
#include <QEventLoop>
#include <QSignalSpy>
#include <QTest>
#include <QThread>
#include <QTimer>

#include <functional>
#include <utility>

using namespace Cosmix;

class MockTrayd final : public QObject, protected QDBusContext
{
    Q_OBJECT
    Q_CLASSINFO("D-Bus Interface", "dev.cosmix.trayd")

public:
    Snapshot snapshot;
    BusSnapshot busSnapshot;
    MixSnapshot mixSnapshot;
    SshSnapshot sshSnapshot;
    QString lastSlug;
    QString lastManager;
    QString lastUnit;
    QString lastVerb;
    int refreshCalls = 0;
    int logCalls = 0;
    int busOpenCalls = 0;
    int busUpdateCalls = 0;
    int busKeepaliveCalls = 0;
    int busCloseCalls = 0;
    int busRosterRefreshCalls = 0;
    QString busSession = QStringLiteral("mock-session");
    QStringList lastDirections;
    QString lastVerbGlob;
    QString lastBodyMode;
    QList<QStringList> busOpenDirections;
    QStringList busOpenVerbs;
    QStringList busOpenBodies;
    QList<QStringList> busUpdateDirections;
    QStringList busUpdateVerbs;
    QStringList busUpdateBodies;
    QString lastMixScriptId;
    QString lastMixName;
    QString lastMixDescription;
    QString lastMixRunId;
    int mixCreateCalls = 0;
    int mixUpdateCalls = 0;
    int mixTrashCalls = 0;
    int mixRestoreCalls = 0;
    int mixPurgeCalls = 0;
    int mixEditCalls = 0;
    int mixRunCalls = 0;
    int mixStopCalls = 0;
    QString lastSshHostId;
    QStringList lastSshProbeIds;
    QString lastSshName;
    QString lastSshHostname;
    quint32 lastSshPort = 0;
    QString lastSshUser;
    QString lastSshKeyId;
    int sshConnectCalls = 0;
    int sshProbeCalls = 0;
    int sshCreateCalls = 0;
    int sshEditCalls = 0;
    int sshTrashCalls = 0;
    int sshRestoreCalls = 0;
    int sshPurgeCalls = 0;
    bool delayNextBusSnapshot = false;
    bool delayNextBusClose = false;
    bool failNextBusOpen = false;
    QString nextBusOpenErrorName =
        QStringLiteral("dev.cosmix.trayd.Error.BusUnavailable");
    QDBusMessage delayedBusSnapshotCall;
    QDBusMessage delayedBusCloseCall;
    std::function<void()> duringNextBusOpen;
    int busOpenCallsBeforeInterleavedReturn = 0;
    std::function<void()> duringNextBusUpdate;
    int busUpdateCallsBeforeInterleavedReturn = 0;

public Q_SLOTS:
    Snapshot GetSnapshot()
    {
        return snapshot;
    }

    void Refresh()
    {
        ++refreshCalls;
    }

    void LaunchApp(const QString &slug)
    {
        lastSlug = slug;
    }

    void ControlDaemon(const QString &manager, const QString &unit, const QString &verb)
    {
        lastManager = manager;
        lastUnit = unit;
        lastVerb = verb;
    }

    void OpenLogs(const QString &manager, const QString &unit)
    {
        lastManager = manager;
        lastUnit = unit;
        ++logCalls;
    }

    QString OpenBusSession(const QStringList &directions,
                           const QString &verbGlob,
                           const QString &bodyMode)
    {
        ++busOpenCalls;
        lastDirections = directions;
        lastVerbGlob = verbGlob;
        lastBodyMode = bodyMode;
        busOpenDirections.append(directions);
        busOpenVerbs.append(verbGlob);
        busOpenBodies.append(bodyMode);
        const bool failOpen = std::exchange(failNextBusOpen, false);
        if (duringNextBusOpen) {
            auto interleave = std::move(duringNextBusOpen);
            QEventLoop loop;
            QTimer::singleShot(0, this, std::move(interleave));
            QTimer::singleShot(50, &loop, &QEventLoop::quit);
            loop.exec();
            busOpenCallsBeforeInterleavedReturn = busOpenCalls;
        }
        if (failOpen) {
            sendErrorReply(nextBusOpenErrorName,
                           QStringLiteral("temporary Bus failure"));
            nextBusOpenErrorName =
                QStringLiteral("dev.cosmix.trayd.Error.BusUnavailable");
            return {};
        }
        busSnapshot.effectiveDirections = directions;
        busSnapshot.effectiveVerbs = {verbGlob};
        busSnapshot.bodyMode = bodyMode;
        return busSession;
    }

    void UpdateBusSession(const QString &session,
                          const QStringList &directions,
                          const QString &verbGlob,
                          const QString &bodyMode)
    {
        Q_ASSERT(session == busSession);
        ++busUpdateCalls;
        lastDirections = directions;
        lastVerbGlob = verbGlob;
        lastBodyMode = bodyMode;
        busUpdateDirections.append(directions);
        busUpdateVerbs.append(verbGlob);
        busUpdateBodies.append(bodyMode);
        busSnapshot.filterEpoch += 1;
        busSnapshot.revision += 1;
        busSnapshot.effectiveDirections = directions;
        busSnapshot.effectiveVerbs = {verbGlob};
        busSnapshot.bodyMode = bodyMode;
        busSnapshot.traffic.clear();
        if (duringNextBusUpdate) {
            auto interleave = std::move(duringNextBusUpdate);
            QEventLoop loop;
            QTimer::singleShot(0, this, std::move(interleave));
            QTimer::singleShot(50, &loop, &QEventLoop::quit);
            loop.exec();
            busUpdateCallsBeforeInterleavedReturn = busUpdateCalls;
        }
    }

    void KeepBusSessionAlive(const QString &session)
    {
        QCOMPARE(session, busSession);
        ++busKeepaliveCalls;
    }

    void CloseBusSession(const QString &session)
    {
        QCOMPARE(session, busSession);
        ++busCloseCalls;
        if (delayNextBusClose) {
            delayNextBusClose = false;
            setDelayedReply(true);
            delayedBusCloseCall = message();
        }
    }

    void releaseDelayedBusClose()
    {
        QVERIFY(delayedBusCloseCall.type() == QDBusMessage::MethodCallMessage);
        QVERIFY(QDBusConnection::sessionBus().send(
            delayedBusCloseCall.createReply()));
        delayedBusCloseCall = {};
    }

    void RefreshBusRoster(const QString &session)
    {
        QCOMPARE(session, busSession);
        ++busRosterRefreshCalls;
    }

    BusSnapshot GetBusSnapshot()
    {
        if (delayNextBusSnapshot) {
            delayNextBusSnapshot = false;
            setDelayedReply(true);
            delayedBusSnapshotCall = message();
            return {};
        }
        return busSnapshot;
    }

    void releaseDelayedBusSnapshot()
    {
        QVERIFY(delayedBusSnapshotCall.type() == QDBusMessage::MethodCallMessage);
        const auto reply = delayedBusSnapshotCall.createReply(
            {QVariant::fromValue(busSnapshot)});
        QVERIFY(QDBusConnection::sessionBus().send(reply));
        delayedBusSnapshotCall = {};
    }

    MixSnapshot GetMixSnapshot()
    {
        return mixSnapshot;
    }

    QString CreateMixScript(const QString &name, const QString &description)
    {
        ++mixCreateCalls;
        lastMixName = name;
        lastMixDescription = description;
        lastMixScriptId = QStringLiteral("mock-created-script");
        mixSnapshot.revision += 1;
        mixSnapshot.scripts.append(MixScriptEntry{
            lastMixScriptId,
            name,
            description,
            false,
            1,
            2,
        });
        return lastMixScriptId;
    }

    void UpdateMixScript(const QString &scriptId,
                         const QString &name,
                         const QString &description)
    {
        ++mixUpdateCalls;
        lastMixScriptId = scriptId;
        lastMixName = name;
        lastMixDescription = description;
    }

    void TrashMixScript(const QString &scriptId)
    {
        ++mixTrashCalls;
        lastMixScriptId = scriptId;
    }

    void RestoreMixScript(const QString &scriptId)
    {
        ++mixRestoreCalls;
        lastMixScriptId = scriptId;
    }

    void PurgeMixScript(const QString &scriptId)
    {
        ++mixPurgeCalls;
        lastMixScriptId = scriptId;
    }

    void EditMixScript(const QString &scriptId)
    {
        ++mixEditCalls;
        lastMixScriptId = scriptId;
    }

    QString RunMixScript(const QString &scriptId)
    {
        ++mixRunCalls;
        lastMixScriptId = scriptId;
        lastMixRunId = QStringLiteral("mock-created-run");
        mixSnapshot.revision += 1;
        mixSnapshot.runs.prepend(MixRunEntry{
            lastMixRunId,
            scriptId,
            QStringLiteral("Created script"),
            QStringLiteral("running"),
            3,
            0,
            false,
            0,
            {},
            {},
            0,
            0,
            1,
        });
        mixSnapshot.activeRuns = 1;
        return lastMixRunId;
    }

    void StopMixRun(const QString &runId)
    {
        ++mixStopCalls;
        lastMixRunId = runId;
    }

    SshSnapshot GetSshSnapshot()
    {
        return sshSnapshot;
    }

    void ConnectSshHost(const QString &hostId)
    {
        ++sshConnectCalls;
        lastSshHostId = hostId;
    }

    void ProbeSshHosts(const QStringList &hostIds)
    {
        ++sshProbeCalls;
        lastSshProbeIds = hostIds;
    }

    void CreateSshHost(const QString &name,
                       const QString &hostname,
                       quint32 port,
                       const QString &user,
                       const QString &keyId)
    {
        ++sshCreateCalls;
        lastSshName = name;
        lastSshHostname = hostname;
        lastSshPort = port;
        lastSshUser = user;
        lastSshKeyId = keyId;
    }

    void EditSshHost(const QString &hostId)
    {
        ++sshEditCalls;
        lastSshHostId = hostId;
    }

    void TrashSshHost(const QString &hostId)
    {
        ++sshTrashCalls;
        lastSshHostId = hostId;
    }

    void RestoreSshHost(const QString &hostId)
    {
        ++sshRestoreCalls;
        lastSshHostId = hostId;
    }

    void PurgeSshHost(const QString &hostId)
    {
        ++sshPurgeCalls;
        lastSshHostId = hostId;
    }

Q_SIGNALS:
    void Changed(quint64 revision);
    void BusChanged(quint64 revision);
    void BusTrafficBatch(quint64 revision,
                         quint64 filterEpoch,
                         const Cosmix::BusTrafficEntries &events,
                         quint64 serverDropped,
                         quint64 bridgeDropped);
    void MixChanged(quint64 revision);
    void MixRunChanged(quint64 revision, const QString &runId);
    void MixRunOutput(quint64 revision,
                      const QString &runId,
                      const Cosmix::MixOutputChunks &chunks,
                      quint64 stdoutDropped,
                      quint64 stderrDropped);
    void SshChanged(quint64 revision);
};

class SlowIntrospectionObject final : public QDBusVirtualObject
{
public:
    mutable int introspectionCalls = 0;

    QString introspect(const QString &path) const override
    {
        Q_UNUSED(path)
        ++introspectionCalls;
        QThread::msleep(750);
        return QStringLiteral(
            "<node><interface name=\"dev.cosmix.trayd\"/></node>");
    }

    bool handleMessage(const QDBusMessage &message,
                       const QDBusConnection &connection) override
    {
        Q_UNUSED(message)
        Q_UNUSED(connection)
        return false;
    }
};

class DelayedOpenObject final : public QDBusVirtualObject
{
public:
    struct PendingOpen {
        QDBusMessage call;
        QDBusConnection connection;
    };

    QList<PendingOpen> openCalls;
    int closeCalls = 0;

    QString introspect(const QString &path) const override
    {
        Q_UNUSED(path)
        return QStringLiteral(
            "<node><interface name=\"dev.cosmix.trayd\">"
            "<method name=\"OpenBusSession\"/>"
            "<method name=\"CloseBusSession\"/>"
            "</interface></node>");
    }

    bool handleMessage(const QDBusMessage &message,
                       const QDBusConnection &connection) override
    {
        if (message.interface() != QStringLiteral("dev.cosmix.trayd")) {
            return false;
        }
        if (message.member() == QStringLiteral("OpenBusSession")) {
            openCalls.append(PendingOpen{message, connection});
            return true;
        }
        if (message.member() == QStringLiteral("CloseBusSession")) {
            ++closeCalls;
            return connection.send(message.createReply());
        }
        return false;
    }

    void releaseFirstOpen()
    {
        QVERIFY(!openCalls.isEmpty());
        const auto pending = openCalls.takeFirst();
        const auto reply = pending.call.createReply(
            QVariantList{QVariant::fromValue(QStringLiteral("delayed-session"))});
        QVERIFY(pending.connection.send(reply));
    }
};

class BackendTest final : public QObject
{
    Q_OBJECT

private Q_SLOTS:
    void initTestCase()
    {
        registerDbusTypes();
    }

    void dbusTypesHaveContractSignatures()
    {
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<AppEntry>()),
                 QByteArrayLiteral("(sssb)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<AppEntries>()),
                 QByteArrayLiteral("a(sssb)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<DaemonEntry>()),
                 QByteArrayLiteral("(sss)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<DaemonEntries>()),
                 QByteArrayLiteral("a(sss)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<Snapshot>()),
                 QByteArrayLiteral("(tbba(sssb)sa(sss)ss)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<BusNodeEntry>()),
                 QByteArrayLiteral("(ssbs)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<BusNodeEntries>()),
                 QByteArrayLiteral("a(ssbs)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<BusTrafficEntry>()),
                 QByteArrayLiteral("(tssssssssbxttss)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<BusTrafficEntries>()),
                 QByteArrayLiteral("a(tssssssssbxttss)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<BusSnapshot>()),
                 QByteArrayLiteral("(tssbtasasssa(ssbs)asa(tssssssssbxttss)tt)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<MixScriptEntry>()),
                 QByteArrayLiteral("(sssbtt)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<MixScriptEntries>()),
                 QByteArrayLiteral("a(sssbtt)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<MixRunEntry>()),
                 QByteArrayLiteral("(ssssttbissttt)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<MixRunEntries>()),
                 QByteArrayLiteral("a(ssssttbissttt)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<MixOutputChunk>()),
                 QByteArrayLiteral("(tss)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<MixOutputChunks>()),
                 QByteArrayLiteral("a(tss)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<MixSnapshot>()),
                 QByteArrayLiteral("(tssa(sssbtt)a(ssssttbissttt)u)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<SshHostEntry>()),
                 QByteArrayLiteral("(ssssqssbsstt)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<SshHostEntries>()),
                 QByteArrayLiteral("a(ssssqssbsstt)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<SshKeyEntry>()),
                 QByteArrayLiteral("(sss)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<SshKeyEntries>()),
                 QByteArrayLiteral("a(sss)"));
        QCOMPARE(QDBusMetaType::typeToSignature(QMetaType::fromType<SshSnapshot>()),
                 QByteArrayLiteral("(tssa(ssssqssbsstt)a(sss)u)"));
    }

    void sshWireTypesMarshalRoundTrip()
    {
        const SshSnapshot expected{
            42,
            QStringLiteral("watching"),
            QStringLiteral("catalogue warning"),
            {SshHostEntry{QStringLiteral("alpha"),
                          {},
                          QStringLiteral("mode is 0644"),
                          QStringLiteral("alpha.example.com"),
                          2222,
                          QStringLiteral("operator"),
                          QStringLiteral("/home/operator/.ssh/keys/alpha"),
                          false,
                          QStringLiteral("failed"),
                          QStringLiteral("connection refused"),
                          19,
                          1'755'000'000'000}},
            {SshKeyEntry{QStringLiteral("alpha"),
                         QStringLiteral("256 SHA256:example alpha (ED25519)"),
                         {}}},
            3,
        };
        MockTrayd mock;
        mock.sshSnapshot = expected;
        auto bus = QDBusConnection::sessionBus();
        QVERIFY(bus.registerService(QStringLiteral("dev.cosmix.trayd")));
        QVERIFY(bus.registerObject(QStringLiteral("/dev/cosmix/trayd"),
                                   &mock,
                                   QDBusConnection::ExportAllSlots));
        QDBusInterface interface(QStringLiteral("dev.cosmix.trayd"),
                                 QStringLiteral("/dev/cosmix/trayd"),
                                 QStringLiteral("dev.cosmix.trayd"),
                                 bus);
        QDBusPendingCallWatcher watcher(
            interface.asyncCall(QStringLiteral("GetSshSnapshot")));
        QTRY_VERIFY_WITH_TIMEOUT(watcher.isFinished(), 5000);
        const QDBusPendingReply<SshSnapshot> reply = watcher;
        QVERIFY2(!reply.isError(), qPrintable(reply.error().message()));
        QCOMPARE(reply.value(), expected);
        bus.unregisterObject(QStringLiteral("/dev/cosmix/trayd"));
        bus.unregisterService(QStringLiteral("dev.cosmix.trayd"));
    }

    void listModelsExposeStableRoles()
    {
        AppListModel apps;
        QSignalSpy appCountChanged(&apps, &AppListModel::countChanged);
        apps.replace({AppEntry{QStringLiteral("tower"),
                              QStringLiteral("CosMix Tower"),
                              QStringLiteral("dev.cosmix.tower"),
                              true}});
        QCOMPARE(apps.rowCount(), 1);
        QCOMPARE(appCountChanged.count(), 1);
        QCOMPARE(apps.data(apps.index(0), AppListModel::SlugRole).toString(),
                 QStringLiteral("tower"));
        QCOMPARE(apps.data(apps.index(0), AppListModel::LaunchableRole).toBool(), true);
        apps.replace(apps.rows());
        QCOMPARE(appCountChanged.count(), 1);

        DaemonListModel daemons;
        daemons.replace({
            DaemonEntry{QStringLiteral("system"),
                        QStringLiteral("active.service"),
                        QStringLiteral("active")},
            DaemonEntry{QStringLiteral("user"),
                        QStringLiteral("failed.service"),
                        QStringLiteral("failed")},
            DaemonEntry{QStringLiteral("user"),
                        QStringLiteral("changing.service"),
                        QStringLiteral("changing")},
        });
        QCOMPARE(daemons.data(daemons.index(0), DaemonListModel::StartEnabledRole).toBool(),
                 false);
        QCOMPARE(daemons.data(daemons.index(0), DaemonListModel::StopEnabledRole).toBool(),
                 true);
        QCOMPARE(daemons.data(daemons.index(1), DaemonListModel::StartEnabledRole).toBool(),
                 true);
        QCOMPARE(daemons.data(daemons.index(1), DaemonListModel::StopEnabledRole).toBool(),
                 true);
        // A transitional unit keeps BOTH actions live — Stop is what cancels a
        // hung start, so folding "changing" into inactive would remove it.
        QCOMPARE(daemons.data(daemons.index(2), DaemonListModel::StatusRole).toString(),
                 QStringLiteral("changing"));
        QCOMPARE(daemons.data(daemons.index(2), DaemonListModel::StartEnabledRole).toBool(),
                 true);
        QCOMPARE(daemons.data(daemons.index(2), DaemonListModel::StopEnabledRole).toBool(),
                 true);

        BusNodeListModel nodes;
        nodes.replace({
            BusNodeEntry{QStringLiteral("alpha"),
                         QStringLiteral("192.0.2.10"),
                         true,
                         QStringLiteral("active")},
            BusNodeEntry{QStringLiteral("beta"),
                         QStringLiteral("198.51.100.20"),
                         false,
                         QStringLiteral("inactive")},
        });
        QCOMPARE(nodes.rowCount(), 2);
        QCOMPARE(nodes.data(nodes.index(0), BusNodeListModel::StatusIconRole).toString(),
                 QStringLiteral("network-connect"));
        QCOMPARE(nodes.data(nodes.index(1), BusNodeListModel::StatusIconRole).toString(),
                 QStringLiteral("network-disconnect"));

        SshHostListModel hosts;
        hosts.replace({SshHostEntry{QStringLiteral("alpha"),
                                    {},
                                    QStringLiteral("mode is 0644"),
                                    QStringLiteral("alpha.example.com"),
                                    22,
                                    QStringLiteral("root"),
                                    QStringLiteral("/home/root/.ssh/keys/alpha"),
                                    false,
                                    QStringLiteral("probing"),
                                    {},
                                    0,
                                    0}});
        QCOMPARE(hosts.data(hosts.index(0), SshHostListModel::IdRole).toString(),
                 QStringLiteral("alpha"));
        QCOMPARE(hosts.data(hosts.index(0), SshHostListModel::DotStatusRole).toString(),
                 QStringLiteral("probing"));
        QCOMPARE(hosts.data(hosts.index(0), SshHostListModel::ActionableRole).toBool(),
                 true);
        QCOMPARE(hosts.data(hosts.index(0), SshHostListModel::HostWarningRole).toString(),
                 QStringLiteral("mode is 0644"));

        SshKeyListModel keys;
        keys.replace({SshKeyEntry{QStringLiteral("alpha"),
                                  QStringLiteral("SHA256:example"),
                                  {}}});
        QCOMPARE(keys.data(keys.index(0), SshKeyListModel::FingerprintRole).toString(),
                 QStringLiteral("SHA256:example"));
    }

    void trafficModelCapsRowsPayloadAndDuplicateBatches()
    {
        BusTrafficEntries rows;
        for (quint64 sequence = 0; sequence < 140; ++sequence) {
            rows.append(BusTrafficEntry{
                sequence,
                QStringLiteral("2026-07-28T00:00:00Z"),
                QStringLiteral("local"),
                QStringLiteral("delivered"),
                QStringLiteral("request"),
                QStringLiteral("alpha-service"),
                QStringLiteral("noded"),
                QStringLiteral("noded.inventory"),
                QStringLiteral("example-%1").arg(sequence),
                false,
                0,
                128,
                0,
                sequence == 139 ? QString(20 * 1024, QLatin1Char('x')) : QString{},
                {},
            });
        }

        BusTrafficListModel traffic;
        traffic.replace(rows);
        QCOMPARE(traffic.rowCount(), BusTrafficListModel::MaximumRows);
        QCOMPARE(traffic.data(traffic.index(0), BusTrafficListModel::SequenceRole).toULongLong(),
                 quint64(12));
        QCOMPARE(traffic.data(traffic.index(127), BusTrafficListModel::PayloadJsonRole).toString(),
                 QString{});
        QCOMPARE(
            traffic.data(traffic.index(127), BusTrafficListModel::PayloadOmittedRole).toString(),
            QStringLiteral("ui_limit"));

        traffic.appendBatch({rows.last()});
        QCOMPARE(traffic.rowCount(), BusTrafficListModel::MaximumRows);
        auto next = rows.last();
        next.sequence = 140;
        next.correlationId = QStringLiteral("example-140");
        traffic.appendBatch({next});
        QCOMPARE(traffic.rowCount(), BusTrafficListModel::MaximumRows);
        QCOMPARE(traffic.data(traffic.index(0), BusTrafficListModel::SequenceRole).toULongLong(),
                 quint64(13));
    }

    void mixModelsFilterExposeRolesAndBoundOutput()
    {
        MixScriptListModel scripts;
        scripts.replace({
            MixScriptEntry{QStringLiteral("alpha"),
                           QStringLiteral("Daily report"),
                           QStringLiteral("Build the report"),
                           false,
                           1,
                           2},
            MixScriptEntry{QStringLiteral("beta"),
                           QStringLiteral("Mesh check"),
                           QStringLiteral("Inspect alpha services"),
                           false,
                           3,
                           4},
        });
        QCOMPARE(scripts.rowCount(), 2);
        QCOMPARE(scripts.data(scripts.index(0), MixScriptListModel::IdRole).toString(),
                 QStringLiteral("alpha"));
        QVERIFY(!scripts.data(scripts.index(0), MixScriptListModel::ModifiedTextRole)
                     .toString()
                     .isEmpty());
        scripts.setFilter(QStringLiteral("mesh"));
        QCOMPARE(scripts.rowCount(), 1);
        QCOMPARE(scripts.data(scripts.index(0), MixScriptListModel::IdRole).toString(),
                 QStringLiteral("beta"));
        scripts.setFilter(QStringLiteral("report"));
        QCOMPARE(scripts.rowCount(), 1);
        QCOMPARE(scripts.data(scripts.index(0), MixScriptListModel::IdRole).toString(),
                 QStringLiteral("alpha"));

        MixRunListModel runs;
        runs.replace({MixRunEntry{
            QStringLiteral("run-alpha"),
            QStringLiteral("alpha"),
            QStringLiteral("Daily report"),
            QStringLiteral("running"),
            10,
            0,
            false,
            0,
            {},
            {},
            0,
            0,
            5,
        }});
        QCOMPARE(runs.data(runs.index(0), MixRunListModel::ActiveRole).toBool(), true);
        QCOMPARE(runs.data(runs.index(0), MixRunListModel::StatusIconRole).toString(),
                 QStringLiteral("media-playback-start"));
        // Snapshot sequence 4 is already represented by the installed tail.
        // Its delayed signal must be discarded without moving baseline 5.
        QVERIFY(runs.appendOutput(
            QStringLiteral("run-alpha"),
            {MixOutputChunk{4, QStringLiteral("stdout"), QStringLiteral("duplicate\n")}},
            0,
            0));
        QCOMPARE(runs.find(QStringLiteral("run-alpha"))->stdoutText, QString{});
        QVERIFY(runs.appendOutput(
            QStringLiteral("run-alpha"),
            {
                MixOutputChunk{5, QStringLiteral("stdout"), QString(300 * 1024, QLatin1Char('x'))},
                MixOutputChunk{6, QStringLiteral("stderr"), QStringLiteral("warning\n")},
            },
            44,
            55));
        const auto *run = runs.find(QStringLiteral("run-alpha"));
        QVERIFY(run != nullptr);
        QVERIFY(run->stdoutText.toUtf8().size() <= MixRunListModel::MaximumOutputBytes);
        QCOMPARE(run->stderrText, QStringLiteral("warning\n"));
        QCOMPARE(run->stdoutDropped, quint64(44));
        QCOMPARE(run->stderrDropped, quint64(55));
        QVERIFY(!runs.appendOutput(
            QStringLiteral("run-alpha"),
            {MixOutputChunk{8, QStringLiteral("stdout"), QStringLiteral("gap\n")}},
            99,
            55));
        QCOMPARE(runs.find(QStringLiteral("run-alpha"))->stdoutDropped, quint64(44));
        // The rejected forward gap must not advance sequence 7.
        QVERIFY(runs.appendOutput(
            QStringLiteral("run-alpha"),
            {MixOutputChunk{7, QStringLiteral("stderr"), QStringLiteral("recovered\n")}},
            44,
            55));
        QCOMPARE(runs.find(QStringLiteral("run-alpha"))->stderrText,
                 QStringLiteral("warning\nrecovered\n"));
    }

    void bridgeConstructionDoesNotIntrospectTrayd()
    {
        SlowIntrospectionObject slowObject;
        auto bus = QDBusConnection::sessionBus();
        QVERIFY(bus.registerService(QStringLiteral("dev.cosmix.trayd")));
        QVERIFY(bus.registerVirtualObject(QStringLiteral("/dev/cosmix/trayd"),
                                          &slowObject));

        QElapsedTimer elapsed;
        elapsed.start();
        TraydBridge bridge;
        QVERIFY2(elapsed.elapsed() < 250,
                 "TraydBridge construction blocked on D-Bus introspection");
        QCOMPARE(slowObject.introspectionCalls, 0);

        bus.unregisterObject(QStringLiteral("/dev/cosmix/trayd"));
        bus.unregisterService(QStringLiteral("dev.cosmix.trayd"));
    }

    void busOpenFailureWaitsForARecoveryEventBeforeRetrying()
    {
        MockTrayd mock;
        mock.failNextBusOpen = true;
        mock.busSnapshot.state = QStringLiteral("connected");

        auto bus = QDBusConnection::sessionBus();
        QVERIFY(bus.registerService(QStringLiteral("dev.cosmix.trayd")));
        QVERIFY(bus.registerObject(QStringLiteral("/dev/cosmix/trayd"),
                                   &mock,
                                   QDBusConnection::ExportAllSlots
                                       | QDBusConnection::ExportAllSignals));

        TraydBridge bridge;
        bridge.setBusVisible(true);
        QTRY_COMPARE_WITH_TIMEOUT(mock.busOpenCalls, 1, 5000);
        QTest::qWait(100);
        QCOMPARE(mock.busOpenCalls, 1);
        QVERIFY(!bridge.busSessionOpen());

        QVERIFY(bus.unregisterService(QStringLiteral("dev.cosmix.trayd")));
        QTRY_COMPARE_WITH_TIMEOUT(bridge.busState(),
                                  QStringLiteral("unavailable"),
                                  5000);
        QVERIFY(bus.registerService(QStringLiteral("dev.cosmix.trayd")));
        QTRY_COMPARE_WITH_TIMEOUT(mock.busOpenCalls, 2, 5000);
        QTRY_VERIFY_WITH_TIMEOUT(bridge.busSessionOpen(), 5000);

        bridge.setBusVisible(false);
        QTRY_COMPARE_WITH_TIMEOUT(mock.busCloseCalls, 1, 5000);
        QTRY_VERIFY_WITH_TIMEOUT(!bridge.busSessionOpen(), 5000);
        QTRY_VERIFY_WITH_TIMEOUT(!bridge.busBusy(), 5000);
        bus.unregisterObject(QStringLiteral("/dev/cosmix/trayd"));
        bus.unregisterService(QStringLiteral("dev.cosmix.trayd"));
    }

    void staleFailedBusOpenReplaysTheLatestDesiredFilter()
    {
        MockTrayd mock;
        mock.failNextBusOpen = true;
        mock.busSnapshot.state = QStringLiteral("connected");

        auto bus = QDBusConnection::sessionBus();
        QVERIFY(bus.registerService(QStringLiteral("dev.cosmix.trayd")));
        QVERIFY(bus.registerObject(QStringLiteral("/dev/cosmix/trayd"),
                                   &mock,
                                   QDBusConnection::ExportAllSlots
                                       | QDBusConnection::ExportAllSignals));

        {
            TraydBridge bridge;
            mock.duringNextBusOpen = [&bridge] {
                bridge.applyBusFilter(QStringLiteral("mesh_out"),
                                      QStringLiteral("gamma.*"),
                                      QStringLiteral("redacted"));
            };
            bridge.setBusVisible(true);

            QTRY_COMPARE_WITH_TIMEOUT(mock.busOpenCalls, 2, 5000);
            QTRY_VERIFY_WITH_TIMEOUT(bridge.busSessionOpen(), 5000);
            QCOMPARE(mock.busOpenCallsBeforeInterleavedReturn, 1);
            QCOMPARE(mock.busCloseCalls, 0);
            QCOMPARE(mock.busOpenDirections.at(1),
                     QStringList{QStringLiteral("mesh_out")});
            QCOMPARE(mock.busOpenVerbs.at(1), QStringLiteral("gamma.*"));
            QCOMPARE(mock.busOpenBodies.at(1), QStringLiteral("redacted"));

            bridge.setBusVisible(false);
            QTRY_COMPARE_WITH_TIMEOUT(mock.busCloseCalls, 1, 5000);
            QTRY_VERIFY_WITH_TIMEOUT(!bridge.busSessionOpen(), 5000);
            QTRY_VERIFY_WITH_TIMEOUT(!bridge.busBusy(), 5000);
        }

        bus.unregisterObject(QStringLiteral("/dev/cosmix/trayd"));
        bus.unregisterService(QStringLiteral("dev.cosmix.trayd"));
    }

    void staleOpenCompletionCannotClearANewerPendingOpen()
    {
        DelayedOpenObject mock;

        const auto connectionName =
            QStringLiteral("cosmix-delayed-open-test");
        auto bus = QDBusConnection::connectToBus(QDBusConnection::SessionBus,
                                                 connectionName);
        QVERIFY(bus.isConnected());
        QVERIFY(bus.registerService(QStringLiteral("dev.cosmix.trayd")));
        QVERIFY(bus.registerVirtualObject(QStringLiteral("/dev/cosmix/trayd"),
                                          &mock));

        TraydBridge bridge;
        bridge.setBusVisible(true);
        QTRY_COMPARE_WITH_TIMEOUT(mock.openCalls.size(), 1, 5000);

        QVERIFY(QMetaObject::invokeMethod(&bridge,
                                         "onServiceUnregistered",
                                         Qt::DirectConnection));
        QCOMPARE(bridge.busState(), QStringLiteral("unavailable"));
        QVERIFY(QMetaObject::invokeMethod(&bridge,
                                         "onServiceRegistered",
                                         Qt::DirectConnection));
        QTRY_COMPARE_WITH_TIMEOUT(mock.openCalls.size(), 2, 5000);
        QVERIFY(bridge.busBusy());

        mock.releaseFirstOpen();
        QTRY_COMPARE_WITH_TIMEOUT(mock.closeCalls, 1, 5000);
        QCOMPARE(mock.openCalls.size(), 1);
        QVERIFY(bridge.busBusy());
        QVERIFY(!bridge.busSessionOpen());

        QVERIFY(QMetaObject::invokeMethod(&bridge,
                                         "onServiceUnregistered",
                                         Qt::DirectConnection));
        mock.releaseFirstOpen();
        QTRY_COMPARE_WITH_TIMEOUT(mock.closeCalls, 2, 5000);
        QTRY_VERIFY_WITH_TIMEOUT(!bridge.busBusy(), 5000);
        QTRY_VERIFY_WITH_TIMEOUT(!bridge.busSessionOpen(), 5000);
        bus.unregisterObject(QStringLiteral("/dev/cosmix/trayd"));
        bus.unregisterService(QStringLiteral("dev.cosmix.trayd"));
        QDBusConnection::disconnectFromBus(connectionName);
    }

    void filterCorrectionRetriesAStoppedOpen()
    {
        MockTrayd mock;
        mock.failNextBusOpen = true;
        mock.nextBusOpenErrorName =
            QStringLiteral("dev.cosmix.trayd.Error.BadBusFilter");
        mock.busSnapshot.state = QStringLiteral("connected");

        auto bus = QDBusConnection::sessionBus();
        QVERIFY(bus.registerService(QStringLiteral("dev.cosmix.trayd")));
        QVERIFY(bus.registerObject(QStringLiteral("/dev/cosmix/trayd"),
                                   &mock,
                                   QDBusConnection::ExportAllSlots
                                       | QDBusConnection::ExportAllSignals));

        TraydBridge bridge;
        bridge.setBusVisible(true);
        QTRY_COMPARE_WITH_TIMEOUT(mock.busOpenCalls, 1, 5000);
        QTRY_VERIFY_WITH_TIMEOUT(!bridge.busBusy(), 5000);
        QVERIFY(!bridge.busSessionOpen());

        bridge.applyBusFilter(QStringLiteral("mesh_in"),
                              QStringLiteral("alpha.*"),
                              QStringLiteral("redacted"));
        QTRY_COMPARE_WITH_TIMEOUT(mock.busOpenCalls, 2, 5000);
        QTRY_VERIFY_WITH_TIMEOUT(bridge.busSessionOpen(), 5000);
        QCOMPARE(mock.busOpenDirections.at(1), QStringList{QStringLiteral("mesh_in")});
        QCOMPARE(mock.busOpenVerbs.at(1), QStringLiteral("alpha.*"));
        QCOMPARE(mock.busOpenBodies.at(1), QStringLiteral("redacted"));

        bridge.setBusVisible(false);
        QTRY_COMPARE_WITH_TIMEOUT(mock.busCloseCalls, 1, 5000);
        QTRY_VERIFY_WITH_TIMEOUT(!bridge.busSessionOpen(), 5000);
        QTRY_VERIFY_WITH_TIMEOUT(!bridge.busBusy(), 5000);
        bus.unregisterObject(QStringLiteral("/dev/cosmix/trayd"));
        bus.unregisterService(QStringLiteral("dev.cosmix.trayd"));
    }

    void bridgeUsesAsyncIdentityOnlyContract()
    {
        MockTrayd mock;
        mock.snapshot = Snapshot{
            7,
            true,
            true,
            {AppEntry{QStringLiteral("tower"),
                      QStringLiteral("CosMix Tower"),
                      QStringLiteral("dev.cosmix.tower"),
                      true}},
            {},
            {
                DaemonEntry{QStringLiteral("system"),
                            QStringLiteral("cosmix-noded.service"),
                            QStringLiteral("active")},
                DaemonEntry{QStringLiteral("user"),
                            QStringLiteral("cosmix-trayd.service"),
                            QStringLiteral("inactive")},
            },
            {},
            {},
        };
        const BusTrafficEntry firstTraffic{
            1,
            QStringLiteral("2026-07-28T00:00:00Z"),
            QStringLiteral("local"),
            QStringLiteral("delivered"),
            QStringLiteral("request"),
            QStringLiteral("alpha-service"),
            QStringLiteral("noded"),
            QStringLiteral("noded.inventory"),
            QStringLiteral("example-1"),
            false,
            0,
            128,
            0,
            {},
            QStringLiteral("disabled"),
        };
        mock.busSnapshot = BusSnapshot{
            3,
            QStringLiteral("connected"),
            {},
            true,
            1,
            {
                QStringLiteral("local"),
                QStringLiteral("mesh_in"),
                QStringLiteral("mesh_out"),
            },
            {QStringLiteral("*")},
            QStringLiteral("none"),
            QStringLiteral("verified"),
            {BusNodeEntry{QStringLiteral("alpha"),
                          QStringLiteral("192.0.2.10"),
                          true,
                          QStringLiteral("active")}},
            {QStringLiteral("noded"), QStringLiteral("tower-bevy-100")},
            {firstTraffic},
            0,
            0,
        };
        mock.mixSnapshot = MixSnapshot{
            9,
            QStringLiteral("watching"),
            {},
            {
                MixScriptEntry{QStringLiteral("script-alpha"),
                               QStringLiteral("Alpha task"),
                               QStringLiteral("An example task"),
                               false,
                               10,
                               20},
                MixScriptEntry{QStringLiteral("script-trash"),
                               QStringLiteral("Old task"),
                               {},
                               true,
                               11,
                               21},
            },
            {
                MixRunEntry{QStringLiteral("run-alpha"),
                            QStringLiteral("script-alpha"),
                            QStringLiteral("Alpha task"),
                            QStringLiteral("succeeded"),
                            12,
                            13,
                            true,
                            0,
                            QStringLiteral("done\n"),
                            {},
                            0,
                            0},
            },
            0,
        };
        mock.sshSnapshot = SshSnapshot{
            4,
            QStringLiteral("watching"),
            {},
            {
                SshHostEntry{QStringLiteral("alpha"),
                             {},
                             QStringLiteral("mode is 0644"),
                             QStringLiteral("alpha.example.com"),
                             22,
                             QStringLiteral("root"),
                             QStringLiteral("/home/root/.ssh/keys/alpha"),
                             false,
                             QStringLiteral("ok"),
                             {},
                             18,
                             1'755'000'000'000},
                SshHostEntry{QStringLiteral("old-alpha"),
                             {},
                             {},
                             QStringLiteral("old.example.com"),
                             2222,
                             QStringLiteral("operator"),
                             QStringLiteral("/home/operator/.ssh/keys/alpha"),
                             true,
                             QStringLiteral("unknown"),
                             {},
                             0,
                             0},
            },
            {SshKeyEntry{QStringLiteral("alpha"),
                         QStringLiteral("256 SHA256:example alpha (ED25519)"),
                         {}}},
            0,
        };

        auto bus = QDBusConnection::sessionBus();
        QVERIFY(bus.registerService(QStringLiteral("dev.cosmix.trayd")));
        QVERIFY(bus.registerObject(QStringLiteral("/dev/cosmix/trayd"),
                                   &mock,
                                   QDBusConnection::ExportAllSlots
                                       | QDBusConnection::ExportAllSignals));

        TraydBridge bridge;
        QSignalSpy snapshotChanged(&bridge, &TraydBridge::snapshotChanged);
        bridge.popupOpened();

        QTRY_COMPARE_WITH_TIMEOUT(bridge.revision(), quint64(7), 5000);
        QCOMPARE(snapshotChanged.count(), 1);
        QCOMPARE(bridge.nodedReachable(), true);
        QCOMPARE(bridge.appsModel()->rowCount(), 1);
        QCOMPARE(bridge.systemDaemonsModel()->rowCount(), 1);
        QCOMPARE(bridge.userDaemonsModel()->rowCount(), 1);
        QTRY_COMPARE_WITH_TIMEOUT(mock.refreshCalls, 1, 5000);
        QTRY_COMPARE_WITH_TIMEOUT(bridge.sshRevision(), quint64(4), 5000);
        QCOMPARE(bridge.sshState(), QStringLiteral("watching"));
        QCOMPARE(bridge.sshHostsModel()->rowCount(), 1);
        QCOMPARE(bridge.sshTrashModel()->rowCount(), 1);
        QCOMPARE(bridge.sshKeysModel()->rowCount(), 1);

        bridge.launchApp(QStringLiteral("tower"));
        QTRY_COMPARE_WITH_TIMEOUT(mock.lastSlug, QStringLiteral("tower"), 5000);

        bridge.controlDaemon(QStringLiteral("system"),
                             QStringLiteral("cosmix-noded.service"),
                             QStringLiteral("restart"));
        QTRY_COMPARE_WITH_TIMEOUT(mock.lastVerb, QStringLiteral("restart"), 5000);
        QCOMPARE(mock.lastManager, QStringLiteral("system"));
        QCOMPARE(mock.lastUnit, QStringLiteral("cosmix-noded.service"));
        QTRY_COMPARE_WITH_TIMEOUT(mock.refreshCalls, 2, 5000);

        bridge.openLogs(QStringLiteral("user"), QStringLiteral("cosmix-trayd.service"));
        QTRY_COMPARE_WITH_TIMEOUT(mock.logCalls, 1, 5000);
        QCOMPARE(mock.lastManager, QStringLiteral("user"));
        QCOMPARE(mock.lastUnit, QStringLiteral("cosmix-trayd.service"));

        mock.snapshot.revision = 8;
        mock.snapshot.nodedReachable = false;
        Q_EMIT mock.Changed(8);
        QTRY_COMPARE_WITH_TIMEOUT(bridge.revision(), quint64(8), 5000);
        QCOMPARE(bridge.nodedReachable(), false);

        bridge.setBusVisible(true);
        QTRY_VERIFY_WITH_TIMEOUT(bridge.busSessionOpen(), 5000);
        QTRY_COMPARE_WITH_TIMEOUT(mock.busOpenCalls, 1, 5000);
        QTRY_COMPARE_WITH_TIMEOUT(bridge.busRevision(), quint64(3), 5000);
        QCOMPARE(bridge.busState(), QStringLiteral("connected"));
        QCOMPARE(bridge.inventoryPosture(), QStringLiteral("verified"));
        QCOMPARE(bridge.busNodesModel()->rowCount(), 1);
        QCOMPARE(bridge.busTrafficModel()->rowCount(), 1);
        QCOMPARE(mock.lastBodyMode, QStringLiteral("none"));

        auto secondTraffic = firstTraffic;
        secondTraffic.sequence = 2;
        secondTraffic.correlationId = QStringLiteral("example-2");
        Q_EMIT mock.BusTrafficBatch(4, 1, {secondTraffic}, 1, 2);
        QTRY_COMPARE_WITH_TIMEOUT(bridge.busTrafficModel()->rowCount(), 2, 5000);
        QCOMPARE(bridge.serverDropped(), quint64(1));
        QCOMPARE(bridge.bridgeDropped(), quint64(2));

        bridge.setBusPaused(true);
        auto thirdTraffic = firstTraffic;
        thirdTraffic.sequence = 3;
        thirdTraffic.correlationId = QStringLiteral("example-3");
        Q_EMIT mock.BusTrafficBatch(5, 1, {thirdTraffic}, 1, 2);
        QTest::qWait(50);
        QCOMPARE(bridge.busTrafficModel()->rowCount(), 2);
        mock.busSnapshot.revision = 5;
        mock.busSnapshot.traffic = {firstTraffic, secondTraffic, thirdTraffic};
        bridge.setBusPaused(false);
        QTRY_COMPARE_WITH_TIMEOUT(bridge.busTrafficModel()->rowCount(), 3, 5000);

        bridge.applyBusFilter(QStringLiteral("mesh_in"),
                              QStringLiteral("maild.*"),
                              QStringLiteral("redacted"));
        QTRY_COMPARE_WITH_TIMEOUT(mock.busUpdateCalls, 1, 5000);
        QCOMPARE(mock.lastDirections, QStringList{QStringLiteral("mesh_in")});
        QCOMPARE(mock.lastVerbGlob, QStringLiteral("maild.*"));
        QCOMPARE(mock.lastBodyMode, QStringLiteral("redacted"));
        QTRY_COMPARE_WITH_TIMEOUT(bridge.busTrafficModel()->rowCount(), 0, 5000);
        // A queued batch from the superseded broad filter must remain fenced
        // even after the new filter snapshot has installed.
        Q_EMIT mock.BusTrafficBatch(7, 1, {thirdTraffic}, 1, 2);
        QTest::qWait(50);
        QCOMPARE(bridge.busTrafficModel()->rowCount(), 0);
        Q_EMIT mock.BusTrafficBatch(8, 2, {thirdTraffic}, 1, 2);
        QTRY_COMPARE_WITH_TIMEOUT(bridge.busTrafficModel()->rowCount(), 1, 5000);

        bridge.refreshBusRoster();
        QTRY_COMPARE_WITH_TIMEOUT(mock.busRosterRefreshCalls, 1, 5000);
        bridge.setBusVisible(false);
        QTRY_COMPARE_WITH_TIMEOUT(mock.busCloseCalls, 1, 5000);
        QTRY_VERIFY_WITH_TIMEOUT(!bridge.busSessionOpen(), 5000);

        bridge.setMixVisible(true);
        QTRY_COMPARE_WITH_TIMEOUT(bridge.mixRevision(), quint64(9), 5000);
        QCOMPARE(bridge.mixState(), QStringLiteral("watching"));
        QCOMPARE(bridge.mixScriptsModel()->rowCount(), 1);
        QCOMPARE(bridge.mixTrashModel()->rowCount(), 1);
        QCOMPARE(bridge.mixRunsModel()->rowCount(), 1);

        bridge.setMixSearch(QStringLiteral("alpha"));
        QCOMPARE(bridge.mixScriptsModel()->rowCount(), 1);
        QCOMPARE(bridge.mixTrashModel()->rowCount(), 0);
        bridge.setMixSearch({});

        bridge.updateMixScript(QStringLiteral("script-alpha"),
                               QStringLiteral("Renamed task"),
                               QStringLiteral("Updated"));
        QTRY_COMPARE_WITH_TIMEOUT(mock.mixUpdateCalls, 1, 5000);
        QCOMPARE(mock.lastMixScriptId, QStringLiteral("script-alpha"));
        QCOMPARE(mock.lastMixName, QStringLiteral("Renamed task"));

        bridge.createMixScript(QStringLiteral("Created script"), QStringLiteral("Starter"));
        QTRY_COMPARE_WITH_TIMEOUT(mock.mixCreateCalls, 1, 5000);
        QTRY_COMPARE_WITH_TIMEOUT(mock.mixEditCalls, 1, 5000);
        QCOMPARE(mock.lastMixScriptId, QStringLiteral("mock-created-script"));

        bridge.runMixScript(QStringLiteral("script-alpha"));
        QTRY_COMPARE_WITH_TIMEOUT(mock.mixRunCalls, 1, 5000);
        QTRY_COMPARE_WITH_TIMEOUT(bridge.selectedMixRunId(),
                                  QStringLiteral("mock-created-run"),
                                  5000);
        QTRY_COMPARE_WITH_TIMEOUT(bridge.selectedMixRunState(),
                                  QStringLiteral("running"),
                                  5000);
        Q_EMIT mock.MixRunOutput(
            12,
            QStringLiteral("mock-created-run"),
            {MixOutputChunk{1, QStringLiteral("stdout"), QStringLiteral("live output\n")}},
            0,
            0);
        QTRY_COMPARE_WITH_TIMEOUT(bridge.selectedMixRunStdout(),
                                  QStringLiteral("live output\n"),
                                  5000);
        bridge.stopMixRun(QStringLiteral("mock-created-run"));
        QTRY_COMPARE_WITH_TIMEOUT(mock.mixStopCalls, 1, 5000);
        QCOMPARE(mock.lastMixRunId, QStringLiteral("mock-created-run"));

        bridge.trashMixScript(QStringLiteral("script-alpha"));
        bridge.restoreMixScript(QStringLiteral("script-trash"));
        bridge.purgeMixScript(QStringLiteral("script-trash"));
        QTRY_COMPARE_WITH_TIMEOUT(mock.mixTrashCalls, 1, 5000);
        QTRY_COMPARE_WITH_TIMEOUT(mock.mixRestoreCalls, 1, 5000);
        QTRY_COMPARE_WITH_TIMEOUT(mock.mixPurgeCalls, 1, 5000);

        bridge.connectSshHost(QStringLiteral("alpha"));
        QTRY_COMPARE_WITH_TIMEOUT(mock.sshConnectCalls, 1, 5000);
        QCOMPARE(mock.lastSshHostId, QStringLiteral("alpha"));

        bridge.probeSshHosts({QStringLiteral("alpha")});
        QTRY_COMPARE_WITH_TIMEOUT(mock.sshProbeCalls, 1, 5000);
        QCOMPARE(mock.lastSshProbeIds, QStringList{QStringLiteral("alpha")});

        bridge.createSshHost(QStringLiteral("beta"),
                             QStringLiteral("beta.example.com"),
                             2222,
                             QStringLiteral("operator"),
                             QStringLiteral("alpha"));
        QTRY_COMPARE_WITH_TIMEOUT(mock.sshCreateCalls, 1, 5000);
        QCOMPARE(mock.lastSshName, QStringLiteral("beta"));
        QCOMPARE(mock.lastSshHostname, QStringLiteral("beta.example.com"));
        QCOMPARE(mock.lastSshPort, quint32(2222));
        QCOMPARE(mock.lastSshUser, QStringLiteral("operator"));
        QCOMPARE(mock.lastSshKeyId, QStringLiteral("alpha"));

        bridge.editSshHost(QStringLiteral("alpha"));
        bridge.trashSshHost(QStringLiteral("alpha"));
        bridge.restoreSshHost(QStringLiteral("old-alpha"));
        bridge.purgeSshHost(QStringLiteral("old-alpha"));
        QTRY_COMPARE_WITH_TIMEOUT(mock.sshEditCalls, 1, 5000);
        QTRY_COMPARE_WITH_TIMEOUT(mock.sshTrashCalls, 1, 5000);
        QTRY_COMPARE_WITH_TIMEOUT(mock.sshRestoreCalls, 1, 5000);
        QTRY_COMPARE_WITH_TIMEOUT(mock.sshPurgeCalls, 1, 5000);

        mock.sshSnapshot.revision = 5;
        mock.sshSnapshot.activeProbes = 1;
        mock.sshSnapshot.hosts[0].probeStatus = QStringLiteral("probing");
        Q_EMIT mock.SshChanged(5);
        QTRY_COMPARE_WITH_TIMEOUT(bridge.sshRevision(), quint64(5), 5000);
        QCOMPARE(bridge.sshActiveProbes(), quint32(1));

        bus.unregisterObject(QStringLiteral("/dev/cosmix/trayd"));
        bus.unregisterService(QStringLiteral("dev.cosmix.trayd"));
    }

    void staleBusSnapshotCompletionCannotStarveTheCurrentGeneration()
    {
        MockTrayd mock;
        mock.busSnapshot.revision = 1;
        mock.busSnapshot.state = QStringLiteral("connected");
        mock.busSnapshot.observing = true;
        mock.busSnapshot.filterEpoch = 1;
        mock.delayNextBusSnapshot = true;

        auto bus = QDBusConnection::sessionBus();
        QVERIFY(bus.registerService(QStringLiteral("dev.cosmix.trayd")));
        QVERIFY(bus.registerObject(QStringLiteral("/dev/cosmix/trayd"),
                                   &mock,
                                   QDBusConnection::ExportAllSlots
                                       | QDBusConnection::ExportAllSignals));

        TraydBridge bridge;
        bridge.setBusVisible(true);
        QTRY_VERIFY_WITH_TIMEOUT(bridge.busSessionOpen(), 5000);
        QTRY_VERIFY_WITH_TIMEOUT(
            mock.delayedBusSnapshotCall.type() == QDBusMessage::MethodCallMessage, 5000);

        bridge.setBusVisible(false);
        QTRY_COMPARE_WITH_TIMEOUT(mock.busCloseCalls, 1, 5000);
        QTRY_VERIFY_WITH_TIMEOUT(!bridge.busSessionOpen(), 5000);
        mock.busSnapshot.revision = 2;
        mock.busSnapshot.filterEpoch = 2;
        bridge.setBusVisible(true);
        QTRY_COMPARE_WITH_TIMEOUT(mock.busOpenCalls, 2, 5000);
        // Generation B must issue and install its own snapshot even while A's
        // delayed callback remains in flight.
        QTRY_COMPARE_WITH_TIMEOUT(bridge.busRevision(), quint64(2), 5000);

        mock.releaseDelayedBusSnapshot();
        QTest::qWait(50);
        QCOMPARE(bridge.busRevision(), quint64(2));

        mock.delayNextBusClose = true;
        bridge.setBusVisible(false);
        QTRY_COMPARE_WITH_TIMEOUT(mock.busCloseCalls, 2, 5000);
        QVERIFY(bridge.busSessionOpen());
        QVERIFY(bridge.busBusy());
        mock.releaseDelayedBusClose();
        QTRY_VERIFY_WITH_TIMEOUT(!bridge.busSessionOpen(), 5000);
        QTRY_VERIFY_WITH_TIMEOUT(!bridge.busBusy(), 5000);
        bus.unregisterObject(QStringLiteral("/dev/cosmix/trayd"));
        bus.unregisterService(QStringLiteral("dev.cosmix.trayd"));
    }

    void rapidBusFilterUpdatesAreSingleFlightAndLatestWins()
    {
        MockTrayd mock;
        mock.busSnapshot.revision = 1;
        mock.busSnapshot.state = QStringLiteral("connected");
        mock.busSnapshot.observing = true;
        mock.busSnapshot.filterEpoch = 1;
        mock.busSnapshot.effectiveDirections = {
            QStringLiteral("local"),
            QStringLiteral("mesh_in"),
            QStringLiteral("mesh_out"),
        };
        mock.busSnapshot.effectiveVerbs = {QStringLiteral("*")};
        mock.busSnapshot.bodyMode = QStringLiteral("none");

        auto bus = QDBusConnection::sessionBus();
        QVERIFY(bus.registerService(QStringLiteral("dev.cosmix.trayd")));
        QVERIFY(bus.registerObject(QStringLiteral("/dev/cosmix/trayd"),
                                   &mock,
                                   QDBusConnection::ExportAllSlots
                                       | QDBusConnection::ExportAllSignals));

        TraydBridge bridge;
        bridge.setBusVisible(true);
        QTRY_COMPARE_WITH_TIMEOUT(bridge.busRevision(), quint64(1), 5000);

        mock.duringNextBusUpdate = [&bridge] {
            bridge.applyBusFilter(QStringLiteral("local"),
                                  QStringLiteral("beta.*"),
                                  QStringLiteral("none"));
            bridge.applyBusFilter(QStringLiteral("mesh_out"),
                                  QStringLiteral("gamma.*"),
                                  QStringLiteral("none"));
        };
        bridge.applyBusFilter(QStringLiteral("mesh_in"),
                              QStringLiteral("alpha.*"),
                              QStringLiteral("redacted"));
        QTRY_COMPARE_WITH_TIMEOUT(mock.busUpdateCalls, 2, 5000);
        QTest::qWait(50);
        // B and C were issued from a nested event loop while A's method call
        // was outstanding. Neither became a concurrent server call.
        QCOMPARE(mock.busUpdateCallsBeforeInterleavedReturn, 1);
        QCOMPARE(mock.busUpdateCalls, 2);
        QCOMPARE(mock.busUpdateDirections.at(0), QStringList{QStringLiteral("mesh_in")});
        QCOMPARE(mock.busUpdateVerbs.at(0), QStringLiteral("alpha.*"));
        QCOMPARE(mock.busUpdateBodies.at(0), QStringLiteral("redacted"));
        // Only the latest desired filter C is replayed after A; B is coalesced.
        QCOMPARE(mock.busUpdateDirections.at(1), QStringList{QStringLiteral("mesh_out")});
        QCOMPARE(mock.busUpdateVerbs.at(1), QStringLiteral("gamma.*"));
        QCOMPARE(mock.busUpdateBodies.at(1), QStringLiteral("none"));
        QTRY_COMPARE_WITH_TIMEOUT(bridge.busDirections(),
                                  QStringList{QStringLiteral("mesh_out")},
                                  5000);
        QTRY_COMPARE_WITH_TIMEOUT(bridge.busVerbs(),
                                  QStringList{QStringLiteral("gamma.*")},
                                  5000);
        QCOMPARE(bridge.busBodyMode(), QStringLiteral("none"));

        mock.delayNextBusClose = true;
        bridge.setBusVisible(false);
        QTRY_COMPARE_WITH_TIMEOUT(mock.busCloseCalls, 1, 5000);
        QVERIFY(bridge.busSessionOpen());
        QVERIFY(bridge.busBusy());
        mock.releaseDelayedBusClose();
        QTRY_VERIFY_WITH_TIMEOUT(!bridge.busSessionOpen(), 5000);
        QTRY_VERIFY_WITH_TIMEOUT(!bridge.busBusy(), 5000);
        bus.unregisterObject(QStringLiteral("/dev/cosmix/trayd"));
        bus.unregisterService(QStringLiteral("dev.cosmix.trayd"));
    }

    void filterChangedDuringPendingOpenFencesAndReopensWithLatestChoice()
    {
        MockTrayd mock;
        mock.busSnapshot.revision = 1;
        mock.busSnapshot.state = QStringLiteral("connected");
        mock.busSnapshot.observing = true;
        mock.busSnapshot.filterEpoch = 1;

        auto bus = QDBusConnection::sessionBus();
        QVERIFY(bus.registerService(QStringLiteral("dev.cosmix.trayd")));
        QVERIFY(bus.registerObject(QStringLiteral("/dev/cosmix/trayd"),
                                   &mock,
                                   QDBusConnection::ExportAllSlots
                                       | QDBusConnection::ExportAllSignals));

        TraydBridge bridge;
        mock.duringNextBusOpen = [&bridge] {
            bridge.applyBusFilter(QStringLiteral("mesh_in"),
                                  QStringLiteral("alpha.*"),
                                  QStringLiteral("redacted"));
            bridge.applyBusFilter(QStringLiteral("mesh_out"),
                                  QStringLiteral("gamma.*"),
                                  QStringLiteral("none"));
        };
        bridge.setBusVisible(true);

        QTRY_COMPARE_WITH_TIMEOUT(mock.busOpenCalls, 2, 5000);
        QTRY_COMPARE_WITH_TIMEOUT(mock.busCloseCalls, 1, 5000);
        QVERIFY(bridge.busSessionOpen());
        QCOMPARE(mock.busOpenCallsBeforeInterleavedReturn, 1);
        QCOMPARE(mock.busOpenDirections.at(0),
                 QStringList({QStringLiteral("local"),
                              QStringLiteral("mesh_in"),
                              QStringLiteral("mesh_out")}));
        QCOMPARE(mock.busOpenVerbs.at(0), QStringLiteral("*"));
        QCOMPARE(mock.busOpenBodies.at(0), QStringLiteral("none"));
        QCOMPARE(mock.busOpenDirections.at(1), QStringList{QStringLiteral("mesh_out")});
        QCOMPARE(mock.busOpenVerbs.at(1), QStringLiteral("gamma.*"));
        QCOMPARE(mock.busOpenBodies.at(1), QStringLiteral("none"));
        QCOMPARE(mock.busUpdateCalls, 0);
        QTRY_COMPARE_WITH_TIMEOUT(bridge.busDirections(),
                                  QStringList{QStringLiteral("mesh_out")},
                                  5000);
        QTRY_COMPARE_WITH_TIMEOUT(bridge.busVerbs(),
                                  QStringList{QStringLiteral("gamma.*")},
                                  5000);
        QCOMPARE(bridge.busBodyMode(), QStringLiteral("none"));

        mock.delayNextBusClose = true;
        bridge.setBusVisible(false);
        QTRY_COMPARE_WITH_TIMEOUT(mock.busCloseCalls, 2, 5000);
        QVERIFY(bridge.busSessionOpen());
        QVERIFY(bridge.busBusy());
        mock.releaseDelayedBusClose();
        QTRY_VERIFY_WITH_TIMEOUT(!bridge.busSessionOpen(), 5000);
        QTRY_VERIFY_WITH_TIMEOUT(!bridge.busBusy(), 5000);
        bus.unregisterObject(QStringLiteral("/dev/cosmix/trayd"));
        bus.unregisterService(QStringLiteral("dev.cosmix.trayd"));
    }
};

QTEST_GUILESS_MAIN(BackendTest)

#include "tst_backend.moc"
