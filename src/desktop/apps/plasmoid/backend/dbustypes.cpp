#include "dbustypes.h"

#include <QDBusMetaType>

#include <mutex>

namespace Cosmix
{

QDBusArgument &operator<<(QDBusArgument &argument, const AppEntry &entry)
{
    argument.beginStructure();
    argument << entry.slug << entry.label << entry.iconName << entry.launchable;
    argument.endStructure();
    return argument;
}

const QDBusArgument &operator>>(const QDBusArgument &argument, AppEntry &entry)
{
    argument.beginStructure();
    argument >> entry.slug >> entry.label >> entry.iconName >> entry.launchable;
    argument.endStructure();
    return argument;
}

QDBusArgument &operator<<(QDBusArgument &argument, const DaemonEntry &entry)
{
    argument.beginStructure();
    argument << entry.manager << entry.unit << entry.status;
    argument.endStructure();
    return argument;
}

const QDBusArgument &operator>>(const QDBusArgument &argument, DaemonEntry &entry)
{
    argument.beginStructure();
    argument >> entry.manager >> entry.unit >> entry.status;
    argument.endStructure();
    return argument;
}

QDBusArgument &operator<<(QDBusArgument &argument, const Snapshot &snapshot)
{
    argument.beginStructure();
    argument << snapshot.revision << snapshot.nodedChecked << snapshot.nodedReachable
             << snapshot.apps << snapshot.appsError << snapshot.daemons
             << snapshot.daemonsError << snapshot.refreshError;
    argument.endStructure();
    return argument;
}

const QDBusArgument &operator>>(const QDBusArgument &argument, Snapshot &snapshot)
{
    argument.beginStructure();
    argument >> snapshot.revision >> snapshot.nodedChecked >> snapshot.nodedReachable
             >> snapshot.apps >> snapshot.appsError >> snapshot.daemons
             >> snapshot.daemonsError >> snapshot.refreshError;
    argument.endStructure();
    return argument;
}

QDBusArgument &operator<<(QDBusArgument &argument, const BusNodeEntry &entry)
{
    argument.beginStructure();
    argument << entry.name << entry.meshIp << entry.busEnabled << entry.status;
    argument.endStructure();
    return argument;
}

const QDBusArgument &operator>>(const QDBusArgument &argument, BusNodeEntry &entry)
{
    argument.beginStructure();
    argument >> entry.name >> entry.meshIp >> entry.busEnabled >> entry.status;
    argument.endStructure();
    return argument;
}

QDBusArgument &operator<<(QDBusArgument &argument, const BusTrafficEntry &entry)
{
    argument.beginStructure();
    argument << entry.sequence << entry.timestamp << entry.direction << entry.outcome
             << entry.messageType << entry.source << entry.target << entry.verb
             << entry.correlationId << entry.hasReturnCode << entry.returnCode << entry.size
             << entry.brokerDropped << entry.payloadJson << entry.payloadOmitted;
    argument.endStructure();
    return argument;
}

const QDBusArgument &operator>>(const QDBusArgument &argument, BusTrafficEntry &entry)
{
    argument.beginStructure();
    argument >> entry.sequence >> entry.timestamp >> entry.direction >> entry.outcome
             >> entry.messageType >> entry.source >> entry.target >> entry.verb
             >> entry.correlationId >> entry.hasReturnCode >> entry.returnCode >> entry.size
             >> entry.brokerDropped >> entry.payloadJson >> entry.payloadOmitted;
    argument.endStructure();
    return argument;
}

QDBusArgument &operator<<(QDBusArgument &argument, const BusSnapshot &snapshot)
{
    argument.beginStructure();
    argument << snapshot.revision << snapshot.state << snapshot.error << snapshot.observing
             << snapshot.filterEpoch
             << snapshot.effectiveDirections << snapshot.effectiveVerbs << snapshot.bodyMode
             << snapshot.inventoryPosture << snapshot.nodes << snapshot.localServices
             << snapshot.traffic << snapshot.serverDropped << snapshot.bridgeDropped;
    argument.endStructure();
    return argument;
}

const QDBusArgument &operator>>(const QDBusArgument &argument, BusSnapshot &snapshot)
{
    argument.beginStructure();
    argument >> snapshot.revision >> snapshot.state >> snapshot.error >> snapshot.observing
             >> snapshot.filterEpoch
             >> snapshot.effectiveDirections >> snapshot.effectiveVerbs >> snapshot.bodyMode
             >> snapshot.inventoryPosture >> snapshot.nodes >> snapshot.localServices
             >> snapshot.traffic >> snapshot.serverDropped >> snapshot.bridgeDropped;
    argument.endStructure();
    return argument;
}

QDBusArgument &operator<<(QDBusArgument &argument, const MixScriptEntry &entry)
{
    argument.beginStructure();
    argument << entry.id << entry.name << entry.description << entry.trashed << entry.createdMs
             << entry.updatedMs;
    argument.endStructure();
    return argument;
}

const QDBusArgument &operator>>(const QDBusArgument &argument, MixScriptEntry &entry)
{
    argument.beginStructure();
    argument >> entry.id >> entry.name >> entry.description >> entry.trashed >> entry.createdMs
        >> entry.updatedMs;
    argument.endStructure();
    return argument;
}

QDBusArgument &operator<<(QDBusArgument &argument, const MixRunEntry &entry)
{
    argument.beginStructure();
    argument << entry.id << entry.scriptId << entry.scriptName << entry.state << entry.startedMs
             << entry.finishedMs << entry.hasExitCode << entry.exitCode << entry.stdoutText
             << entry.stderrText << entry.stdoutDropped << entry.stderrDropped;
    argument << entry.nextSequence;
    argument.endStructure();
    return argument;
}

const QDBusArgument &operator>>(const QDBusArgument &argument, MixRunEntry &entry)
{
    argument.beginStructure();
    argument >> entry.id >> entry.scriptId >> entry.scriptName >> entry.state >> entry.startedMs
        >> entry.finishedMs >> entry.hasExitCode >> entry.exitCode >> entry.stdoutText
        >> entry.stderrText >> entry.stdoutDropped >> entry.stderrDropped;
    argument >> entry.nextSequence;
    argument.endStructure();
    return argument;
}

QDBusArgument &operator<<(QDBusArgument &argument, const MixOutputChunk &chunk)
{
    argument.beginStructure();
    argument << chunk.sequence << chunk.stream << chunk.text;
    argument.endStructure();
    return argument;
}

const QDBusArgument &operator>>(const QDBusArgument &argument, MixOutputChunk &chunk)
{
    argument.beginStructure();
    argument >> chunk.sequence >> chunk.stream >> chunk.text;
    argument.endStructure();
    return argument;
}

QDBusArgument &operator<<(QDBusArgument &argument, const MixSnapshot &snapshot)
{
    argument.beginStructure();
    argument << snapshot.revision << snapshot.state << snapshot.error << snapshot.scripts
             << snapshot.runs << snapshot.activeRuns;
    argument.endStructure();
    return argument;
}

const QDBusArgument &operator>>(const QDBusArgument &argument, MixSnapshot &snapshot)
{
    argument.beginStructure();
    argument >> snapshot.revision >> snapshot.state >> snapshot.error >> snapshot.scripts
        >> snapshot.runs >> snapshot.activeRuns;
    argument.endStructure();
    return argument;
}

QDBusArgument &operator<<(QDBusArgument &argument, const SshHostEntry &entry)
{
    argument.beginStructure();
    argument << entry.id << entry.hostError << entry.hostWarning << entry.hostname << entry.port
             << entry.user << entry.identity << entry.trashed << entry.probeStatus
             << entry.probeError << entry.probeMs << entry.probeCheckedAt;
    argument.endStructure();
    return argument;
}

const QDBusArgument &operator>>(const QDBusArgument &argument, SshHostEntry &entry)
{
    argument.beginStructure();
    argument >> entry.id >> entry.hostError >> entry.hostWarning >> entry.hostname >> entry.port
        >> entry.user >> entry.identity >> entry.trashed >> entry.probeStatus >> entry.probeError
        >> entry.probeMs >> entry.probeCheckedAt;
    argument.endStructure();
    return argument;
}

QDBusArgument &operator<<(QDBusArgument &argument, const SshKeyEntry &entry)
{
    argument.beginStructure();
    argument << entry.id << entry.fingerprint << entry.keyError;
    argument.endStructure();
    return argument;
}

const QDBusArgument &operator>>(const QDBusArgument &argument, SshKeyEntry &entry)
{
    argument.beginStructure();
    argument >> entry.id >> entry.fingerprint >> entry.keyError;
    argument.endStructure();
    return argument;
}

QDBusArgument &operator<<(QDBusArgument &argument, const SshSnapshot &snapshot)
{
    argument.beginStructure();
    argument << snapshot.revision << snapshot.state << snapshot.error << snapshot.hosts
             << snapshot.keys << snapshot.activeProbes;
    argument.endStructure();
    return argument;
}

const QDBusArgument &operator>>(const QDBusArgument &argument, SshSnapshot &snapshot)
{
    argument.beginStructure();
    argument >> snapshot.revision >> snapshot.state >> snapshot.error >> snapshot.hosts
        >> snapshot.keys >> snapshot.activeProbes;
    argument.endStructure();
    return argument;
}

void registerDbusTypes()
{
    static std::once_flag registered;
    std::call_once(registered, [] {
        qRegisterMetaType<AppEntry>();
        qRegisterMetaType<AppEntries>();
        qRegisterMetaType<DaemonEntry>();
        qRegisterMetaType<DaemonEntries>();
        qRegisterMetaType<Snapshot>();
        qRegisterMetaType<BusNodeEntry>();
        qRegisterMetaType<BusNodeEntries>();
        qRegisterMetaType<BusTrafficEntry>();
        qRegisterMetaType<BusTrafficEntries>();
        qRegisterMetaType<BusSnapshot>();
        qRegisterMetaType<MixScriptEntry>();
        qRegisterMetaType<MixScriptEntries>();
        qRegisterMetaType<MixRunEntry>();
        qRegisterMetaType<MixRunEntries>();
        qRegisterMetaType<MixOutputChunk>();
        qRegisterMetaType<MixOutputChunks>();
        qRegisterMetaType<MixSnapshot>();
        qRegisterMetaType<SshHostEntry>();
        qRegisterMetaType<SshHostEntries>();
        qRegisterMetaType<SshKeyEntry>();
        qRegisterMetaType<SshKeyEntries>();
        qRegisterMetaType<SshSnapshot>();
        qDBusRegisterMetaType<AppEntry>();
        qDBusRegisterMetaType<AppEntries>();
        qDBusRegisterMetaType<DaemonEntry>();
        qDBusRegisterMetaType<DaemonEntries>();
        qDBusRegisterMetaType<Snapshot>();
        qDBusRegisterMetaType<BusNodeEntry>();
        qDBusRegisterMetaType<BusNodeEntries>();
        qDBusRegisterMetaType<BusTrafficEntry>();
        qDBusRegisterMetaType<BusTrafficEntries>();
        qDBusRegisterMetaType<BusSnapshot>();
        qDBusRegisterMetaType<MixScriptEntry>();
        qDBusRegisterMetaType<MixScriptEntries>();
        qDBusRegisterMetaType<MixRunEntry>();
        qDBusRegisterMetaType<MixRunEntries>();
        qDBusRegisterMetaType<MixOutputChunk>();
        qDBusRegisterMetaType<MixOutputChunks>();
        qDBusRegisterMetaType<MixSnapshot>();
        qDBusRegisterMetaType<SshHostEntry>();
        qDBusRegisterMetaType<SshHostEntries>();
        qDBusRegisterMetaType<SshKeyEntry>();
        qDBusRegisterMetaType<SshKeyEntries>();
        qDBusRegisterMetaType<SshSnapshot>();
    });
}

}
