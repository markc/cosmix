#include "dbustypes.h"

#include <QCoreApplication>
#include <QDBusConnection>
#include <QDBusInterface>
#include <QDBusPendingCallWatcher>
#include <QDBusPendingReply>
#include <QSocketNotifier>
#include <QTextStream>

#include <cstdio>

using namespace Cosmix;

namespace
{

constexpr auto serviceName = "dev.cosmix.trayd";
constexpr auto objectPath = "/dev/cosmix/trayd";
constexpr auto interfaceName = "dev.cosmix.trayd";

}

class LiveBusProbe final : public QObject
{
    Q_OBJECT

public:
    explicit LiveBusProbe(QObject *parent = nullptr)
        : QObject(parent)
        , m_interface(QString::fromLatin1(serviceName),
                      QString::fromLatin1(objectPath),
                      QString::fromLatin1(interfaceName),
                      QDBusConnection::sessionBus())
        , m_stdin(fileno(stdin), QSocketNotifier::Read, this)
    {
        registerDbusTypes();
        auto bus = QDBusConnection::sessionBus();
        bus.connect(QString::fromLatin1(serviceName),
                    QString::fromLatin1(objectPath),
                    QString::fromLatin1(interfaceName),
                    QStringLiteral("BusChanged"),
                    this,
                    SLOT(onBusChanged(quint64)));
        bus.connect(QString::fromLatin1(serviceName),
                    QString::fromLatin1(objectPath),
                    QString::fromLatin1(interfaceName),
                    QStringLiteral("BusTrafficBatch"),
                    this,
                    SLOT(onBusTrafficBatch(quint64,quint64,Cosmix::BusTrafficEntries,quint64,quint64)));
        connect(&m_stdin, &QSocketNotifier::activated, this, &LiveBusProbe::readCommand);
    }

    void start()
    {
        auto *watcher = new QDBusPendingCallWatcher(
            m_interface.asyncCall(QStringLiteral("OpenBusSession"),
                                  QStringList{
                                      QStringLiteral("local"),
                                      QStringLiteral("mesh_in"),
                                      QStringLiteral("mesh_out"),
                                  },
                                  QStringLiteral("*"),
                                  QStringLiteral("none")),
            this);
        connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, watcher] {
            const QDBusPendingReply<QString> reply = *watcher;
            if (reply.isError()) {
                fail(QStringLiteral("OpenBusSession failed: %1").arg(reply.error().message()));
            } else {
                m_sessionId = reply.value();
                QTextStream(stdout) << "LIVE_OPEN session=" << m_sessionId << Qt::endl;
                requestSnapshot();
            }
            watcher->deleteLater();
        });
    }

private Q_SLOTS:
    void onBusChanged(quint64)
    {
        if (!m_sessionId.isEmpty() && !m_closing) {
            requestSnapshot();
        }
    }

    void onBusTrafficBatch(quint64 revision,
                           quint64 filterEpoch,
                           const BusTrafficEntries &events,
                           quint64 serverDropped,
                           quint64 bridgeDropped)
    {
        if (events.isEmpty() || m_sawBatch) {
            return;
        }
        for (const auto &event : events) {
            if (!event.payloadJson.isEmpty()) {
                fail(QStringLiteral("metadata-only observation carried a payload"));
                return;
            }
        }
        m_sawBatch = true;
        QTextStream(stdout) << "LIVE_BATCH revision=" << revision << " events=" << events.size()
                            << " filter_epoch=" << filterEpoch
                            << " server_dropped=" << serverDropped
                            << " bridge_dropped=" << bridgeDropped << Qt::endl
                            << "LIVE_GATE_READY" << Qt::endl;
    }

private:
    void requestSnapshot()
    {
        if (m_snapshotPending) {
            return;
        }
        m_snapshotPending = true;
        auto *watcher = new QDBusPendingCallWatcher(
            m_interface.asyncCall(QStringLiteral("GetBusSnapshot")), this);
        connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, watcher] {
            m_snapshotPending = false;
            const QDBusPendingReply<BusSnapshot> reply = *watcher;
            if (reply.isError()) {
                fail(QStringLiteral("GetBusSnapshot failed: %1").arg(reply.error().message()));
            } else if (reply.value().observing && !m_refreshed) {
                m_refreshed = true;
                m_interface.asyncCall(QStringLiteral("RefreshBusRoster"), m_sessionId);
            }
            watcher->deleteLater();
        });
    }

    void readCommand()
    {
        const auto command = QTextStream(stdin).readLine().trimmed();
        if (command == QStringLiteral("close")) {
            closeLease();
        } else if (command == QStringLiteral("exit") && m_closed) {
            QCoreApplication::quit();
        }
    }

    void closeLease()
    {
        if (m_closing || m_sessionId.isEmpty()) {
            return;
        }
        m_closing = true;
        auto *watcher = new QDBusPendingCallWatcher(
            m_interface.asyncCall(QStringLiteral("CloseBusSession"), m_sessionId), this);
        connect(watcher, &QDBusPendingCallWatcher::finished, this, [this, watcher] {
            const QDBusPendingReply<> reply = *watcher;
            if (reply.isError()) {
                fail(QStringLiteral("CloseBusSession failed: %1").arg(reply.error().message()));
                watcher->deleteLater();
                return;
            }
            m_sessionId.clear();
            auto *snapshotWatcher = new QDBusPendingCallWatcher(
                m_interface.asyncCall(QStringLiteral("GetBusSnapshot")), this);
            connect(snapshotWatcher,
                    &QDBusPendingCallWatcher::finished,
                    this,
                    [this, snapshotWatcher] {
                        const QDBusPendingReply<BusSnapshot> snapshotReply = *snapshotWatcher;
                        if (snapshotReply.isError()) {
                            fail(QStringLiteral("post-close snapshot failed: %1")
                                     .arg(snapshotReply.error().message()));
                        } else {
                            const auto snapshot = snapshotReply.value();
                            m_closed = !snapshot.observing;
                            QTextStream(stdout)
                                << "LIVE_CLOSED observing="
                                << (snapshot.observing ? "true" : "false")
                                << " state=" << snapshot.state << Qt::endl
                                << "AFTER_CLOSE_READY" << Qt::endl;
                        }
                        snapshotWatcher->deleteLater();
                    });
            watcher->deleteLater();
        });
    }

    void fail(const QString &message)
    {
        QTextStream(stderr) << "LIVE_GATE_ERROR " << message << Qt::endl;
        QCoreApplication::exit(1);
    }

    QDBusInterface m_interface;
    QSocketNotifier m_stdin;
    QString m_sessionId;
    bool m_snapshotPending = false;
    bool m_refreshed = false;
    bool m_sawBatch = false;
    bool m_closing = false;
    bool m_closed = false;
};

int main(int argc, char **argv)
{
    QCoreApplication app(argc, argv);
    LiveBusProbe probe;
    QMetaObject::invokeMethod(&probe, &LiveBusProbe::start, Qt::QueuedConnection);
    return app.exec();
}

#include "live_bus_probe.moc"
