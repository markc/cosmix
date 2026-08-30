#pragma once

#include "dbustypes.h"

#include <QAbstractListModel>
#include <QHash>

namespace Cosmix
{

class COSMIXPLASMOIDBACKEND_EXPORT AppListModel final : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(int count READ rowCount NOTIFY countChanged)

public:
    enum Role {
        SlugRole = Qt::UserRole + 1,
        LabelRole,
        IconNameRole,
        LaunchableRole,
    };
    Q_ENUM(Role)

    explicit AppListModel(QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;

    const AppEntries &rows() const;
    void replace(AppEntries rows);

Q_SIGNALS:
    void countChanged();

private:
    AppEntries m_rows;
};

class COSMIXPLASMOIDBACKEND_EXPORT DaemonListModel final : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(int count READ rowCount NOTIFY countChanged)

public:
    enum Role {
        ManagerRole = Qt::UserRole + 1,
        UnitRole,
        StatusRole,
        StartEnabledRole,
        StopEnabledRole,
    };
    Q_ENUM(Role)

    explicit DaemonListModel(QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;

    const DaemonEntries &rows() const;
    void replace(DaemonEntries rows);

Q_SIGNALS:
    void countChanged();

private:
    DaemonEntries m_rows;
};

class COSMIXPLASMOIDBACKEND_EXPORT BusNodeListModel final : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(int count READ rowCount NOTIFY countChanged)

public:
    enum Role {
        NameRole = Qt::UserRole + 1,
        MeshIpRole,
        BusEnabledRole,
        StatusRole,
        StatusIconRole,
    };
    Q_ENUM(Role)

    explicit BusNodeListModel(QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;

    const BusNodeEntries &rows() const;
    void replace(BusNodeEntries rows);

Q_SIGNALS:
    void countChanged();

private:
    BusNodeEntries m_rows;
};

class COSMIXPLASMOIDBACKEND_EXPORT BusTrafficListModel final : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(int count READ rowCount NOTIFY countChanged)

public:
    static constexpr int MaximumRows = 128;
    static constexpr int MaximumPayloadBytes = 16 * 1024;

    enum Role {
        SequenceRole = Qt::UserRole + 1,
        TimestampRole,
        DirectionRole,
        OutcomeRole,
        MessageTypeRole,
        SourceRole,
        TargetRole,
        VerbRole,
        CorrelationIdRole,
        HasReturnCodeRole,
        ReturnCodeRole,
        SizeRole,
        BrokerDroppedRole,
        PayloadJsonRole,
        PayloadOmittedRole,
        DirectionIconRole,
    };
    Q_ENUM(Role)

    explicit BusTrafficListModel(QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;

    const BusTrafficEntries &rows() const;
    void clear();
    void replace(BusTrafficEntries rows);
    void appendBatch(BusTrafficEntries rows);

Q_SIGNALS:
    void countChanged();

private:
    static void enforcePayloadCap(BusTrafficEntry &entry);
    bool contains(const BusTrafficEntry &entry) const;

    BusTrafficEntries m_rows;
};

class COSMIXPLASMOIDBACKEND_EXPORT MixScriptListModel final : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(int count READ rowCount NOTIFY countChanged)

public:
    enum Role {
        IdRole = Qt::UserRole + 1,
        NameRole,
        DescriptionRole,
        TrashedRole,
        CreatedMsRole,
        UpdatedMsRole,
        ModifiedTextRole,
    };
    Q_ENUM(Role)

    explicit MixScriptListModel(QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;

    const MixScriptEntries &rows() const;
    void replace(MixScriptEntries rows);
    void setFilter(const QString &filter);

Q_SIGNALS:
    void countChanged();

private:
    void rebuild();

    MixScriptEntries m_allRows;
    MixScriptEntries m_rows;
    QString m_filter;
};

class COSMIXPLASMOIDBACKEND_EXPORT MixRunListModel final : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(int count READ rowCount NOTIFY countChanged)

public:
    static constexpr int MaximumOutputBytes = 256 * 1024;

    enum Role {
        IdRole = Qt::UserRole + 1,
        ScriptIdRole,
        ScriptNameRole,
        StateRole,
        StartedMsRole,
        FinishedMsRole,
        HasExitCodeRole,
        ExitCodeRole,
        StdoutRole,
        StderrRole,
        StdoutDroppedRole,
        StderrDroppedRole,
        ActiveRole,
        StatusIconRole,
    };
    Q_ENUM(Role)

    explicit MixRunListModel(QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;

    const MixRunEntries &rows() const;
    const MixRunEntry *find(const QString &id) const;
    void replace(MixRunEntries rows);
    // Returns false when the per-run sequence exposes a missed or reordered
    // D-Bus batch. The bridge then replaces this model from GetMixSnapshot.
    bool appendOutput(const QString &runId,
                      const MixOutputChunks &chunks,
                      quint64 stdoutDropped,
                      quint64 stderrDropped);

Q_SIGNALS:
    void countChanged();
    void runChanged(const QString &runId);

private:
    static bool isActive(const QString &state);
    static QString boundedTail(QString text);

    MixRunEntries m_rows;
    QHash<QString, quint64> m_nextOutputSequence;
};

class COSMIXPLASMOIDBACKEND_EXPORT SshHostListModel final : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(int count READ rowCount NOTIFY countChanged)

public:
    enum Role {
        IdRole = Qt::UserRole + 1,
        HostErrorRole,
        HostWarningRole,
        HostnameRole,
        PortRole,
        UserRole,
        IdentityRole,
        TrashedRole,
        ProbeStatusRole,
        ProbeErrorRole,
        ProbeMsRole,
        ProbeCheckedAtRole,
        DotStatusRole,
        ActionableRole,
    };
    Q_ENUM(Role)

    explicit SshHostListModel(QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;

    const SshHostEntries &rows() const;
    void replace(SshHostEntries rows);

Q_SIGNALS:
    void countChanged();

private:
    SshHostEntries m_rows;
};

class COSMIXPLASMOIDBACKEND_EXPORT SshKeyListModel final : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(int count READ rowCount NOTIFY countChanged)

public:
    enum Role {
        IdRole = Qt::UserRole + 1,
        FingerprintRole,
        KeyErrorRole,
    };
    Q_ENUM(Role)

    explicit SshKeyListModel(QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;

    const SshKeyEntries &rows() const;
    void replace(SshKeyEntries rows);

Q_SIGNALS:
    void countChanged();

private:
    SshKeyEntries m_rows;
};

}
