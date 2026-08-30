#pragma once

#include "cosmixplasmoidbackend_export.h"

#include <QDBusArgument>
#include <QList>
#include <QString>
#include <QStringList>

namespace Cosmix
{

struct AppEntry {
    QString slug;
    QString label;
    QString iconName;
    bool launchable = false;

    bool operator==(const AppEntry &) const = default;
};

struct DaemonEntry {
    QString manager;
    QString unit;
    QString status;

    bool operator==(const DaemonEntry &) const = default;
};

using AppEntries = QList<AppEntry>;
using DaemonEntries = QList<DaemonEntry>;

struct Snapshot {
    quint64 revision = 0;
    bool nodedChecked = false;
    bool nodedReachable = false;
    AppEntries apps;
    QString appsError;
    DaemonEntries daemons;
    QString daemonsError;
    QString refreshError;

    bool operator==(const Snapshot &) const = default;
};

struct BusNodeEntry {
    QString name;
    QString meshIp;
    bool busEnabled = false;
    QString status;

    bool operator==(const BusNodeEntry &) const = default;
};

struct BusTrafficEntry {
    quint64 sequence = 0;
    QString timestamp;
    QString direction;
    QString outcome;
    QString messageType;
    QString source;
    QString target;
    QString verb;
    QString correlationId;
    bool hasReturnCode = false;
    qint64 returnCode = 0;
    quint64 size = 0;
    quint64 brokerDropped = 0;
    QString payloadJson;
    QString payloadOmitted;

    bool operator==(const BusTrafficEntry &) const = default;
};

using BusNodeEntries = QList<BusNodeEntry>;
using BusTrafficEntries = QList<BusTrafficEntry>;

struct BusSnapshot {
    quint64 revision = 0;
    QString state;
    QString error;
    bool observing = false;
    quint64 filterEpoch = 0;
    QStringList effectiveDirections;
    QStringList effectiveVerbs;
    QString bodyMode;
    QString inventoryPosture;
    BusNodeEntries nodes;
    QStringList localServices;
    BusTrafficEntries traffic;
    quint64 serverDropped = 0;
    quint64 bridgeDropped = 0;

    bool operator==(const BusSnapshot &) const = default;
};

struct MixScriptEntry {
    QString id;
    QString name;
    QString description;
    bool trashed = false;
    quint64 createdMs = 0;
    quint64 updatedMs = 0;

    bool operator==(const MixScriptEntry &) const = default;
};

struct MixRunEntry {
    QString id;
    QString scriptId;
    QString scriptName;
    QString state;
    quint64 startedMs = 0;
    quint64 finishedMs = 0;
    bool hasExitCode = false;
    qint32 exitCode = 0;
    QString stdoutText;
    QString stderrText;
    quint64 stdoutDropped = 0;
    quint64 stderrDropped = 0;
    quint64 nextSequence = 1;

    bool operator==(const MixRunEntry &) const = default;
};

struct MixOutputChunk {
    quint64 sequence = 0;
    QString stream;
    QString text;

    bool operator==(const MixOutputChunk &) const = default;
};

using MixScriptEntries = QList<MixScriptEntry>;
using MixRunEntries = QList<MixRunEntry>;
using MixOutputChunks = QList<MixOutputChunk>;

struct MixSnapshot {
    quint64 revision = 0;
    QString state;
    QString error;
    MixScriptEntries scripts;
    MixRunEntries runs;
    quint32 activeRuns = 0;

    bool operator==(const MixSnapshot &) const = default;
};

struct SshHostEntry {
    QString id;
    QString hostError;
    QString hostWarning;
    QString hostname;
    quint16 port = 0;
    QString user;
    QString identity;
    bool trashed = false;
    QString probeStatus;
    QString probeError;
    quint64 probeMs = 0;
    quint64 probeCheckedAt = 0;

    bool operator==(const SshHostEntry &) const = default;
};

struct SshKeyEntry {
    QString id;
    QString fingerprint;
    QString keyError;

    bool operator==(const SshKeyEntry &) const = default;
};

using SshHostEntries = QList<SshHostEntry>;
using SshKeyEntries = QList<SshKeyEntry>;

struct SshSnapshot {
    quint64 revision = 0;
    QString state;
    QString error;
    SshHostEntries hosts;
    SshKeyEntries keys;
    quint32 activeProbes = 0;

    bool operator==(const SshSnapshot &) const = default;
};

COSMIXPLASMOIDBACKEND_EXPORT QDBusArgument &operator<<(QDBusArgument &argument,
                                                       const AppEntry &entry);
COSMIXPLASMOIDBACKEND_EXPORT const QDBusArgument &operator>>(const QDBusArgument &argument,
                                                             AppEntry &entry);

COSMIXPLASMOIDBACKEND_EXPORT QDBusArgument &operator<<(QDBusArgument &argument,
                                                       const DaemonEntry &entry);
COSMIXPLASMOIDBACKEND_EXPORT const QDBusArgument &operator>>(const QDBusArgument &argument,
                                                             DaemonEntry &entry);

COSMIXPLASMOIDBACKEND_EXPORT QDBusArgument &operator<<(QDBusArgument &argument,
                                                       const Snapshot &snapshot);
COSMIXPLASMOIDBACKEND_EXPORT const QDBusArgument &operator>>(const QDBusArgument &argument,
                                                             Snapshot &snapshot);

COSMIXPLASMOIDBACKEND_EXPORT QDBusArgument &operator<<(QDBusArgument &argument,
                                                       const BusNodeEntry &entry);
COSMIXPLASMOIDBACKEND_EXPORT const QDBusArgument &operator>>(const QDBusArgument &argument,
                                                             BusNodeEntry &entry);

COSMIXPLASMOIDBACKEND_EXPORT QDBusArgument &operator<<(QDBusArgument &argument,
                                                       const BusTrafficEntry &entry);
COSMIXPLASMOIDBACKEND_EXPORT const QDBusArgument &operator>>(const QDBusArgument &argument,
                                                             BusTrafficEntry &entry);

COSMIXPLASMOIDBACKEND_EXPORT QDBusArgument &operator<<(QDBusArgument &argument,
                                                       const BusSnapshot &snapshot);
COSMIXPLASMOIDBACKEND_EXPORT const QDBusArgument &operator>>(const QDBusArgument &argument,
                                                             BusSnapshot &snapshot);

COSMIXPLASMOIDBACKEND_EXPORT QDBusArgument &operator<<(QDBusArgument &argument,
                                                       const MixScriptEntry &entry);
COSMIXPLASMOIDBACKEND_EXPORT const QDBusArgument &operator>>(const QDBusArgument &argument,
                                                             MixScriptEntry &entry);

COSMIXPLASMOIDBACKEND_EXPORT QDBusArgument &operator<<(QDBusArgument &argument,
                                                       const MixRunEntry &entry);
COSMIXPLASMOIDBACKEND_EXPORT const QDBusArgument &operator>>(const QDBusArgument &argument,
                                                             MixRunEntry &entry);

COSMIXPLASMOIDBACKEND_EXPORT QDBusArgument &operator<<(QDBusArgument &argument,
                                                       const MixOutputChunk &chunk);
COSMIXPLASMOIDBACKEND_EXPORT const QDBusArgument &operator>>(const QDBusArgument &argument,
                                                             MixOutputChunk &chunk);

COSMIXPLASMOIDBACKEND_EXPORT QDBusArgument &operator<<(QDBusArgument &argument,
                                                       const MixSnapshot &snapshot);
COSMIXPLASMOIDBACKEND_EXPORT const QDBusArgument &operator>>(const QDBusArgument &argument,
                                                             MixSnapshot &snapshot);

COSMIXPLASMOIDBACKEND_EXPORT QDBusArgument &operator<<(QDBusArgument &argument,
                                                       const SshHostEntry &entry);
COSMIXPLASMOIDBACKEND_EXPORT const QDBusArgument &operator>>(const QDBusArgument &argument,
                                                             SshHostEntry &entry);

COSMIXPLASMOIDBACKEND_EXPORT QDBusArgument &operator<<(QDBusArgument &argument,
                                                       const SshKeyEntry &entry);
COSMIXPLASMOIDBACKEND_EXPORT const QDBusArgument &operator>>(const QDBusArgument &argument,
                                                             SshKeyEntry &entry);

COSMIXPLASMOIDBACKEND_EXPORT QDBusArgument &operator<<(QDBusArgument &argument,
                                                       const SshSnapshot &snapshot);
COSMIXPLASMOIDBACKEND_EXPORT const QDBusArgument &operator>>(const QDBusArgument &argument,
                                                             SshSnapshot &snapshot);

COSMIXPLASMOIDBACKEND_EXPORT void registerDbusTypes();

}

Q_DECLARE_METATYPE(Cosmix::AppEntry)
Q_DECLARE_METATYPE(Cosmix::AppEntries)
Q_DECLARE_METATYPE(Cosmix::DaemonEntry)
Q_DECLARE_METATYPE(Cosmix::DaemonEntries)
Q_DECLARE_METATYPE(Cosmix::Snapshot)
Q_DECLARE_METATYPE(Cosmix::BusNodeEntry)
Q_DECLARE_METATYPE(Cosmix::BusNodeEntries)
Q_DECLARE_METATYPE(Cosmix::BusTrafficEntry)
Q_DECLARE_METATYPE(Cosmix::BusTrafficEntries)
Q_DECLARE_METATYPE(Cosmix::BusSnapshot)
Q_DECLARE_METATYPE(Cosmix::MixScriptEntry)
Q_DECLARE_METATYPE(Cosmix::MixScriptEntries)
Q_DECLARE_METATYPE(Cosmix::MixRunEntry)
Q_DECLARE_METATYPE(Cosmix::MixRunEntries)
Q_DECLARE_METATYPE(Cosmix::MixOutputChunk)
Q_DECLARE_METATYPE(Cosmix::MixOutputChunks)
Q_DECLARE_METATYPE(Cosmix::MixSnapshot)
Q_DECLARE_METATYPE(Cosmix::SshHostEntry)
Q_DECLARE_METATYPE(Cosmix::SshHostEntries)
Q_DECLARE_METATYPE(Cosmix::SshKeyEntry)
Q_DECLARE_METATYPE(Cosmix::SshKeyEntries)
Q_DECLARE_METATYPE(Cosmix::SshSnapshot)
