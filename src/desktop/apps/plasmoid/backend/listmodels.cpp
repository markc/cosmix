#include "listmodels.h"

#include <QDateTime>
#include <QLocale>

#include <algorithm>
#include <utility>

namespace Cosmix
{

AppListModel::AppListModel(QObject *parent)
    : QAbstractListModel(parent)
{
}

int AppListModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : m_rows.size();
}

QVariant AppListModel::data(const QModelIndex &index, int role) const
{
    if (!index.isValid() || index.row() < 0 || index.row() >= m_rows.size()) {
        return {};
    }

    const auto &entry = m_rows.at(index.row());
    switch (role) {
    case SlugRole:
        return entry.slug;
    case LabelRole:
        return entry.label;
    case IconNameRole:
        return entry.iconName;
    case LaunchableRole:
        return entry.launchable;
    default:
        return {};
    }
}

QHash<int, QByteArray> AppListModel::roleNames() const
{
    return {
        {SlugRole, "slug"},
        {LabelRole, "label"},
        {IconNameRole, "iconName"},
        {LaunchableRole, "launchable"},
    };
}

const AppEntries &AppListModel::rows() const
{
    return m_rows;
}

void AppListModel::replace(AppEntries rows)
{
    if (m_rows == rows) {
        return;
    }

    beginResetModel();
    m_rows = std::move(rows);
    endResetModel();
    Q_EMIT countChanged();
}

DaemonListModel::DaemonListModel(QObject *parent)
    : QAbstractListModel(parent)
{
}

int DaemonListModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : m_rows.size();
}

QVariant DaemonListModel::data(const QModelIndex &index, int role) const
{
    if (!index.isValid() || index.row() < 0 || index.row() >= m_rows.size()) {
        return {};
    }

    const auto &entry = m_rows.at(index.row());
    switch (role) {
    case ManagerRole:
        return entry.manager;
    case UnitRole:
        return entry.unit;
    case StatusRole:
        return entry.status;
    case StartEnabledRole:
        return entry.status != QStringLiteral("active");
    case StopEnabledRole:
        return entry.status != QStringLiteral("inactive");
    default:
        return {};
    }
}

QHash<int, QByteArray> DaemonListModel::roleNames() const
{
    return {
        {ManagerRole, "manager"},
        {UnitRole, "unit"},
        {StatusRole, "status"},
        {StartEnabledRole, "startEnabled"},
        {StopEnabledRole, "stopEnabled"},
    };
}

const DaemonEntries &DaemonListModel::rows() const
{
    return m_rows;
}

void DaemonListModel::replace(DaemonEntries rows)
{
    if (m_rows == rows) {
        return;
    }

    beginResetModel();
    m_rows = std::move(rows);
    endResetModel();
    Q_EMIT countChanged();
}

BusNodeListModel::BusNodeListModel(QObject *parent)
    : QAbstractListModel(parent)
{
}

int BusNodeListModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : m_rows.size();
}

QVariant BusNodeListModel::data(const QModelIndex &index, int role) const
{
    if (!index.isValid() || index.row() < 0 || index.row() >= m_rows.size()) {
        return {};
    }

    const auto &entry = m_rows.at(index.row());
    switch (role) {
    case NameRole:
        return entry.name;
    case MeshIpRole:
        return entry.meshIp;
    case BusEnabledRole:
        return entry.busEnabled;
    case StatusRole:
        return entry.status;
    case StatusIconRole:
        if (!entry.busEnabled) {
            return QStringLiteral("network-disconnect");
        }
        if (entry.status == QStringLiteral("active")) {
            return QStringLiteral("network-connect");
        }
        return QStringLiteral("network-offline");
    default:
        return {};
    }
}

QHash<int, QByteArray> BusNodeListModel::roleNames() const
{
    return {
        {NameRole, "name"},
        {MeshIpRole, "meshIp"},
        {BusEnabledRole, "busEnabled"},
        {StatusRole, "status"},
        {StatusIconRole, "statusIcon"},
    };
}

const BusNodeEntries &BusNodeListModel::rows() const
{
    return m_rows;
}

void BusNodeListModel::replace(BusNodeEntries rows)
{
    if (m_rows == rows) {
        return;
    }
    beginResetModel();
    m_rows = std::move(rows);
    endResetModel();
    Q_EMIT countChanged();
}

BusTrafficListModel::BusTrafficListModel(QObject *parent)
    : QAbstractListModel(parent)
{
}

int BusTrafficListModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : m_rows.size();
}

QVariant BusTrafficListModel::data(const QModelIndex &index, int role) const
{
    if (!index.isValid() || index.row() < 0 || index.row() >= m_rows.size()) {
        return {};
    }

    const auto &entry = m_rows.at(index.row());
    switch (role) {
    case SequenceRole:
        return entry.sequence;
    case TimestampRole:
        return entry.timestamp;
    case DirectionRole:
        return entry.direction;
    case OutcomeRole:
        return entry.outcome;
    case MessageTypeRole:
        return entry.messageType;
    case SourceRole:
        return entry.source;
    case TargetRole:
        return entry.target;
    case VerbRole:
        return entry.verb;
    case CorrelationIdRole:
        return entry.correlationId;
    case HasReturnCodeRole:
        return entry.hasReturnCode;
    case ReturnCodeRole:
        return entry.returnCode;
    case SizeRole:
        return entry.size;
    case BrokerDroppedRole:
        return entry.brokerDropped;
    case PayloadJsonRole:
        return entry.payloadJson;
    case PayloadOmittedRole:
        return entry.payloadOmitted;
    case DirectionIconRole:
        if (entry.direction == QStringLiteral("mesh_in")) {
            return QStringLiteral("arrow-down");
        }
        if (entry.direction == QStringLiteral("mesh_out")) {
            return QStringLiteral("arrow-up");
        }
        return QStringLiteral("exchange-positions");
    default:
        return {};
    }
}

QHash<int, QByteArray> BusTrafficListModel::roleNames() const
{
    return {
        {SequenceRole, "sequence"},
        {TimestampRole, "timestamp"},
        {DirectionRole, "direction"},
        {OutcomeRole, "outcome"},
        {MessageTypeRole, "messageType"},
        {SourceRole, "sourceService"},
        {TargetRole, "targetService"},
        {VerbRole, "verb"},
        {CorrelationIdRole, "correlationId"},
        {HasReturnCodeRole, "hasReturnCode"},
        {ReturnCodeRole, "returnCode"},
        {SizeRole, "messageSize"},
        {BrokerDroppedRole, "brokerDropped"},
        {PayloadJsonRole, "payloadJson"},
        {PayloadOmittedRole, "payloadOmitted"},
        {DirectionIconRole, "directionIcon"},
    };
}

const BusTrafficEntries &BusTrafficListModel::rows() const
{
    return m_rows;
}

void BusTrafficListModel::clear()
{
    if (m_rows.isEmpty()) {
        return;
    }
    beginResetModel();
    m_rows.clear();
    endResetModel();
    Q_EMIT countChanged();
}

void BusTrafficListModel::replace(BusTrafficEntries rows)
{
    for (auto &entry : rows) {
        enforcePayloadCap(entry);
    }
    if (rows.size() > MaximumRows) {
        rows = rows.mid(rows.size() - MaximumRows);
    }
    if (m_rows == rows) {
        return;
    }
    beginResetModel();
    m_rows = std::move(rows);
    endResetModel();
    Q_EMIT countChanged();
}

void BusTrafficListModel::appendBatch(BusTrafficEntries rows)
{
    for (auto &entry : rows) {
        enforcePayloadCap(entry);
        if (contains(entry)) {
            continue;
        }
        if (m_rows.size() == MaximumRows) {
            beginRemoveRows({}, 0, 0);
            m_rows.removeFirst();
            endRemoveRows();
        }
        const auto newRow = m_rows.size();
        beginInsertRows({}, newRow, newRow);
        m_rows.append(std::move(entry));
        endInsertRows();
        Q_EMIT countChanged();
    }
}

void BusTrafficListModel::enforcePayloadCap(BusTrafficEntry &entry)
{
    if (entry.payloadJson.toUtf8().size() <= MaximumPayloadBytes) {
        return;
    }
    entry.payloadJson.clear();
    entry.payloadOmitted = QStringLiteral("ui_limit");
}

bool BusTrafficListModel::contains(const BusTrafficEntry &entry) const
{
    return std::any_of(m_rows.cbegin(), m_rows.cend(), [&entry](const auto &existing) {
        return existing.sequence == entry.sequence && existing.timestamp == entry.timestamp
            && existing.direction == entry.direction && existing.verb == entry.verb
            && existing.correlationId == entry.correlationId;
    });
}

MixScriptListModel::MixScriptListModel(QObject *parent)
    : QAbstractListModel(parent)
{
}

int MixScriptListModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : m_rows.size();
}

QVariant MixScriptListModel::data(const QModelIndex &index, int role) const
{
    if (!index.isValid() || index.row() < 0 || index.row() >= m_rows.size()) {
        return {};
    }
    const auto &entry = m_rows.at(index.row());
    switch (role) {
    case IdRole:
        return entry.id;
    case NameRole:
        return entry.name;
    case DescriptionRole:
        return entry.description;
    case TrashedRole:
        return entry.trashed;
    case CreatedMsRole:
        return entry.createdMs;
    case UpdatedMsRole:
        return entry.updatedMs;
    case ModifiedTextRole:
        return QLocale().toString(QDateTime::fromMSecsSinceEpoch(
                                      static_cast<qint64>(entry.updatedMs)),
                                  QLocale::ShortFormat);
    default:
        return {};
    }
}

QHash<int, QByteArray> MixScriptListModel::roleNames() const
{
    return {
        {IdRole, "scriptId"},
        {NameRole, "name"},
        {DescriptionRole, "description"},
        {TrashedRole, "trashed"},
        {CreatedMsRole, "createdMs"},
        {UpdatedMsRole, "updatedMs"},
        {ModifiedTextRole, "modifiedText"},
    };
}

const MixScriptEntries &MixScriptListModel::rows() const
{
    return m_rows;
}

void MixScriptListModel::replace(MixScriptEntries rows)
{
    if (m_allRows == rows) {
        return;
    }
    m_allRows = std::move(rows);
    rebuild();
}

void MixScriptListModel::setFilter(const QString &filter)
{
    const auto normalised = filter.trimmed();
    if (m_filter == normalised) {
        return;
    }
    m_filter = normalised;
    rebuild();
}

void MixScriptListModel::rebuild()
{
    MixScriptEntries filtered;
    filtered.reserve(m_allRows.size());
    for (const auto &entry : std::as_const(m_allRows)) {
        if (m_filter.isEmpty()
            || entry.name.contains(m_filter, Qt::CaseInsensitive)
            || entry.description.contains(m_filter, Qt::CaseInsensitive)) {
            filtered.append(entry);
        }
    }
    if (m_rows == filtered) {
        return;
    }
    const auto oldCount = m_rows.size();
    beginResetModel();
    m_rows = std::move(filtered);
    endResetModel();
    if (oldCount != m_rows.size()) {
        Q_EMIT countChanged();
    }
}

MixRunListModel::MixRunListModel(QObject *parent)
    : QAbstractListModel(parent)
{
}

int MixRunListModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : m_rows.size();
}

QVariant MixRunListModel::data(const QModelIndex &index, int role) const
{
    if (!index.isValid() || index.row() < 0 || index.row() >= m_rows.size()) {
        return {};
    }
    const auto &entry = m_rows.at(index.row());
    switch (role) {
    case IdRole:
        return entry.id;
    case ScriptIdRole:
        return entry.scriptId;
    case ScriptNameRole:
        return entry.scriptName;
    case StateRole:
        return entry.state;
    case StartedMsRole:
        return entry.startedMs;
    case FinishedMsRole:
        return entry.finishedMs;
    case HasExitCodeRole:
        return entry.hasExitCode;
    case ExitCodeRole:
        return entry.exitCode;
    case StdoutRole:
        return entry.stdoutText;
    case StderrRole:
        return entry.stderrText;
    case StdoutDroppedRole:
        return entry.stdoutDropped;
    case StderrDroppedRole:
        return entry.stderrDropped;
    case ActiveRole:
        return isActive(entry.state);
    case StatusIconRole:
        if (entry.state == QStringLiteral("succeeded")) {
            return QStringLiteral("dialog-ok");
        }
        if (entry.state == QStringLiteral("failed")
            || entry.state == QStringLiteral("launch_failed")) {
            return QStringLiteral("dialog-error");
        }
        if (entry.state == QStringLiteral("stopped")) {
            return QStringLiteral("process-stop");
        }
        return QStringLiteral("media-playback-start");
    default:
        return {};
    }
}

QHash<int, QByteArray> MixRunListModel::roleNames() const
{
    return {
        {IdRole, "runId"},
        {ScriptIdRole, "scriptId"},
        {ScriptNameRole, "scriptName"},
        {StateRole, "runState"},
        {StartedMsRole, "startedMs"},
        {FinishedMsRole, "finishedMs"},
        {HasExitCodeRole, "hasExitCode"},
        {ExitCodeRole, "exitCode"},
        {StdoutRole, "stdoutText"},
        {StderrRole, "stderrText"},
        {StdoutDroppedRole, "stdoutDropped"},
        {StderrDroppedRole, "stderrDropped"},
        {ActiveRole, "active"},
        {StatusIconRole, "statusIcon"},
    };
}

const MixRunEntries &MixRunListModel::rows() const
{
    return m_rows;
}

const MixRunEntry *MixRunListModel::find(const QString &id) const
{
    const auto found = std::find_if(m_rows.cbegin(), m_rows.cend(), [&id](const auto &entry) {
        return entry.id == id;
    });
    return found == m_rows.cend() ? nullptr : &*found;
}

void MixRunListModel::replace(MixRunEntries rows)
{
    QHash<QString, quint64> nextSequences;
    for (auto &row : rows) {
        row.stdoutText = boundedTail(std::move(row.stdoutText));
        row.stderrText = boundedTail(std::move(row.stderrText));
        nextSequences.insert(row.id, row.nextSequence);
    }
    if (m_rows == rows) {
        m_nextOutputSequence = std::move(nextSequences);
        return;
    }
    const auto oldCount = m_rows.size();
    beginResetModel();
    m_rows = std::move(rows);
    m_nextOutputSequence = std::move(nextSequences);
    endResetModel();
    if (oldCount != m_rows.size()) {
        Q_EMIT countChanged();
    }
}

bool MixRunListModel::appendOutput(const QString &runId,
                                   const MixOutputChunks &chunks,
                                   quint64 stdoutDropped,
                                   quint64 stderrDropped)
{
    const auto found = std::find_if(m_rows.begin(), m_rows.end(), [&runId](const auto &entry) {
        return entry.id == runId;
    });
    if (found == m_rows.end()) {
        return false;
    }
    bool changed = false;
    bool contiguous = true;
    for (const auto &chunk : chunks) {
        const auto expected = m_nextOutputSequence.value(runId, 1);
        if (chunk.sequence < expected) {
            // A snapshot may already contain this chunk. Never let a delayed
            // signal duplicate it or move the installed baseline backwards.
            continue;
        }
        if (chunk.sequence > expected) {
            // Preserve the last contiguous baseline. The bridge will request
            // a corrective snapshot; no out-of-order suffix is rendered.
            contiguous = false;
            break;
        }
        m_nextOutputSequence.insert(runId, chunk.sequence + 1);
        if (chunk.stream == QStringLiteral("stdout")) {
            found->stdoutText.append(chunk.text);
        } else if (chunk.stream == QStringLiteral("stderr")) {
            found->stderrText.append(chunk.text);
        }
        changed = true;
    }
    changed = changed
        || (contiguous
            && (found->stdoutDropped != stdoutDropped
                || found->stderrDropped != stderrDropped));
    if (!changed) {
        return contiguous;
    }
    found->stdoutText = boundedTail(std::move(found->stdoutText));
    found->stderrText = boundedTail(std::move(found->stderrText));
    if (contiguous) {
        found->stdoutDropped = stdoutDropped;
        found->stderrDropped = stderrDropped;
    }
    const auto index = createIndex(static_cast<int>(std::distance(m_rows.begin(), found)), 0);
    Q_EMIT dataChanged(index,
                       index,
                       {StdoutRole, StderrRole, StdoutDroppedRole, StderrDroppedRole});
    Q_EMIT runChanged(runId);
    return contiguous;
}

bool MixRunListModel::isActive(const QString &state)
{
    return state == QStringLiteral("starting") || state == QStringLiteral("running")
        || state == QStringLiteral("stopping");
}

QString MixRunListModel::boundedTail(QString text)
{
    auto bytes = text.toUtf8();
    if (bytes.size() <= MaximumOutputBytes) {
        return text;
    }
    bytes = bytes.right(MaximumOutputBytes);
    while (!bytes.isEmpty()
           && (static_cast<unsigned char>(bytes.front()) & 0xc0U) == 0x80U) {
        bytes.removeFirst();
    }
    return QString::fromUtf8(bytes);
}

SshHostListModel::SshHostListModel(QObject *parent)
    : QAbstractListModel(parent)
{
}

int SshHostListModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : m_rows.size();
}

QVariant SshHostListModel::data(const QModelIndex &index, int role) const
{
    if (!index.isValid() || index.row() < 0 || index.row() >= m_rows.size()) {
        return {};
    }
    const auto &entry = m_rows.at(index.row());
    switch (role) {
    case IdRole:
        return entry.id;
    case HostErrorRole:
        return entry.hostError;
    case HostWarningRole:
        return entry.hostWarning;
    case HostnameRole:
        return entry.hostname;
    case PortRole:
        return entry.port;
    case UserRole:
        return entry.user;
    case IdentityRole:
        return entry.identity;
    case TrashedRole:
        return entry.trashed;
    case ProbeStatusRole:
        return entry.probeStatus;
    case ProbeErrorRole:
        return entry.probeError;
    case ProbeMsRole:
        return entry.probeMs;
    case ProbeCheckedAtRole:
        return entry.probeCheckedAt;
    case DotStatusRole:
        return entry.probeStatus.isEmpty() ? QStringLiteral("unknown") : entry.probeStatus;
    case ActionableRole:
        return !entry.trashed && entry.hostError.isEmpty();
    default:
        return {};
    }
}

QHash<int, QByteArray> SshHostListModel::roleNames() const
{
    return {
        {IdRole, "hostId"},
        {HostErrorRole, "hostError"},
        {HostWarningRole, "hostWarning"},
        {HostnameRole, "hostname"},
        {PortRole, "port"},
        {UserRole, "user"},
        {IdentityRole, "identity"},
        {TrashedRole, "trashed"},
        {ProbeStatusRole, "probeStatus"},
        {ProbeErrorRole, "probeError"},
        {ProbeMsRole, "probeMs"},
        {ProbeCheckedAtRole, "probeCheckedAt"},
        {DotStatusRole, "dotStatus"},
        {ActionableRole, "actionable"},
    };
}

const SshHostEntries &SshHostListModel::rows() const
{
    return m_rows;
}

void SshHostListModel::replace(SshHostEntries rows)
{
    if (m_rows == rows) {
        return;
    }
    const auto oldCount = m_rows.size();
    beginResetModel();
    m_rows = std::move(rows);
    endResetModel();
    if (oldCount != m_rows.size()) {
        Q_EMIT countChanged();
    }
}

SshKeyListModel::SshKeyListModel(QObject *parent)
    : QAbstractListModel(parent)
{
}

int SshKeyListModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : m_rows.size();
}

QVariant SshKeyListModel::data(const QModelIndex &index, int role) const
{
    if (!index.isValid() || index.row() < 0 || index.row() >= m_rows.size()) {
        return {};
    }
    const auto &entry = m_rows.at(index.row());
    switch (role) {
    case IdRole:
        return entry.id;
    case FingerprintRole:
        return entry.fingerprint;
    case KeyErrorRole:
        return entry.keyError;
    default:
        return {};
    }
}

QHash<int, QByteArray> SshKeyListModel::roleNames() const
{
    return {
        {IdRole, "keyId"},
        {FingerprintRole, "fingerprint"},
        {KeyErrorRole, "keyError"},
    };
}

const SshKeyEntries &SshKeyListModel::rows() const
{
    return m_rows;
}

void SshKeyListModel::replace(SshKeyEntries rows)
{
    if (m_rows == rows) {
        return;
    }
    const auto oldCount = m_rows.size();
    beginResetModel();
    m_rows = std::move(rows);
    endResetModel();
    if (oldCount != m_rows.size()) {
        Q_EMIT countChanged();
    }
}

}
