#include <QCoreApplication>
#include <QDir>
#include <QFile>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QMetaMethod>
#include <QQmlComponent>
#include <QQmlEngine>
#include <QRegularExpression>
#include <QTextStream>
#include <QUrl>

#include <memory>

namespace
{

constexpr auto requiredImport = "import \"CosmixBackend\" as CosmixBackend";
constexpr auto requiredStatus =
    "Plasmoid.status: PlasmaCore.Types.ActiveStatus";

QString joinedErrors(const QQmlComponent &component)
{
    QStringList messages;
    for (const auto &error : component.errors()) {
        messages.append(error.toString());
    }
    return messages.join(QLatin1Char('\n'));
}

bool hasMethodNamed(const QMetaObject *metaObject, const QByteArray &name)
{
    for (int index = 0; index < metaObject->methodCount(); ++index) {
        if (metaObject->method(index).name() == name) {
            return true;
        }
    }
    return false;
}

}

int main(int argc, char **argv)
{
    QCoreApplication application(argc, argv);
    QTextStream errorStream(stderr);
    QTextStream outputStream(stdout);

    if (application.arguments().size() != 2) {
        errorStream << "usage: staged_backend_probe PACKAGE_ROOT\n";
        return 2;
    }

    const QDir packageRoot(application.arguments().at(1));
    const auto uiPath = packageRoot.filePath(QStringLiteral("contents/ui"));
    QFile metadata(packageRoot.filePath(QStringLiteral("metadata.json")));
    if (!metadata.open(QIODevice::ReadOnly | QIODevice::Text)) {
        errorStream << "STAGED_BACKEND_PROBE_ERROR cannot read "
                    << metadata.fileName() << ": " << metadata.errorString() << '\n';
        return 1;
    }
    QJsonParseError metadataError;
    const auto metadataDocument =
        QJsonDocument::fromJson(metadata.readAll(), &metadataError);
    if (metadataError.error != QJsonParseError::NoError
        || !metadataDocument.isObject()) {
        errorStream << "STAGED_BACKEND_PROBE_ERROR invalid metadata.json: "
                    << metadataError.errorString() << '\n';
        return 1;
    }
    const auto metadataObject = metadataDocument.object();
    const auto pluginObject = metadataObject.value(QStringLiteral("KPlugin")).toObject();
    const auto formFactors =
        pluginObject.value(QStringLiteral("FormFactors")).toArray();
    if (metadataObject.value(QStringLiteral("X-Plasma-NotificationArea")).toString()
            != QStringLiteral("true")
        || metadataObject
                .value(QStringLiteral("X-Plasma-NotificationAreaCategory"))
                .toString()
            != QStringLiteral("SystemServices")
        || !pluginObject.value(QStringLiteral("EnabledByDefault")).toBool()
        || !formFactors.contains(QStringLiteral("desktop"))) {
        errorStream << "STAGED_BACKEND_PROBE_ERROR metadata.json does not "
                       "declare an enabled desktop SystemServices tray applet\n";
        return 1;
    }

    QFile mainQml(QDir(uiPath).filePath(QStringLiteral("main.qml")));
    if (!mainQml.open(QIODevice::ReadOnly | QIODevice::Text)) {
        errorStream << "STAGED_BACKEND_PROBE_ERROR cannot read "
                    << mainQml.fileName() << ": " << mainQml.errorString() << '\n';
        return 1;
    }

    const auto mainSource = QString::fromUtf8(mainQml.readAll());
    if (!mainSource.contains(QString::fromLatin1(requiredStatus))) {
        errorStream << "STAGED_BACKEND_PROBE_ERROR main.qml must keep the tray "
                       "entry active: "
                    << requiredStatus << '\n';
        return 1;
    }

    QFile fullRepresentation(
        QDir(uiPath).filePath(QStringLiteral("FullRepresentation.qml")));
    QFile sshTab(QDir(uiPath).filePath(QStringLiteral("SshTab.qml")));
    if (!fullRepresentation.open(QIODevice::ReadOnly | QIODevice::Text)
        || !sshTab.open(QIODevice::ReadOnly | QIODevice::Text)) {
        errorStream << "STAGED_BACKEND_PROBE_ERROR SSH tab files are missing\n";
        return 1;
    }
    const auto fullRepresentationSource =
        QString::fromUtf8(fullRepresentation.readAll());
    if (!fullRepresentationSource.contains(QStringLiteral("text: \"SSH\""))
        || !fullRepresentationSource.contains(QStringLiteral("SshTab {"))) {
        errorStream << "STAGED_BACKEND_PROBE_ERROR FullRepresentation does not expose SSH\n";
        return 1;
    }
    const QRegularExpression backendImport(
        QStringLiteral(
            R"(^\s*(import\s+(?:"CosmixBackend"|CosmixBackend)\s+as\s+CosmixBackend)\s*$)"),
        QRegularExpression::MultilineOption);
    const auto importMatch = backendImport.match(mainSource);
    if (!importMatch.hasMatch()) {
        errorStream << "STAGED_BACKEND_PROBE_ERROR cannot find the CosmixBackend "
                       "import in main.qml\n";
        return 1;
    }

    QQmlEngine engine;
    auto importPaths = engine.importPathList();
    importPaths.removeAll(QCoreApplication::applicationDirPath());
    engine.setImportPathList(importPaths);
    QQmlComponent component(&engine);
    const auto importLine = importMatch.captured(1);
    const auto probeSource =
        QStringLiteral("import QtQml\n%1\nCosmixBackend.TraydBridge {}\n")
            .arg(importLine)
            .toUtf8();
    component.setData(
        probeSource,
        QUrl::fromLocalFile(QDir(uiPath).filePath(
            QStringLiteral("StagedBackendImportProbe.qml"))));
    if (component.isError()) {
        errorStream << "STAGED_BACKEND_PROBE_ERROR QML import failed:\n"
                    << joinedErrors(component) << '\n';
        return 1;
    }

    if (importLine != QString::fromLatin1(requiredImport)) {
        errorStream
            << "STAGED_BACKEND_PROBE_ERROR main.qml must use package-relative import: "
            << requiredImport << '\n';
        return 1;
    }

    std::unique_ptr<QObject> instance(component.create());
    if (!instance) {
        errorStream << "STAGED_BACKEND_PROBE_ERROR component creation failed:\n"
                    << joinedErrors(component) << '\n';
        return 1;
    }

    const auto *metaObject = instance->metaObject();
    const QList<QByteArray> sshProperties = {
        QByteArrayLiteral("sshHostsModel"),
        QByteArrayLiteral("sshTrashModel"),
        QByteArrayLiteral("sshKeysModel"),
        QByteArrayLiteral("sshRevision"),
        QByteArrayLiteral("sshState"),
        QByteArrayLiteral("sshError"),
        QByteArrayLiteral("sshActiveProbes"),
    };
    for (const auto &property : sshProperties) {
        if (metaObject->indexOfProperty(property.constData()) < 0) {
            errorStream << "STAGED_BACKEND_PROBE_ERROR missing SSH property "
                        << property << '\n';
            return 1;
        }
    }
    const QList<QByteArray> sshMethods = {
        QByteArrayLiteral("connectSshHost"),
        QByteArrayLiteral("probeSshHosts"),
        QByteArrayLiteral("createSshHost"),
        QByteArrayLiteral("editSshHost"),
        QByteArrayLiteral("trashSshHost"),
        QByteArrayLiteral("restoreSshHost"),
        QByteArrayLiteral("purgeSshHost"),
    };
    for (const auto &method : sshMethods) {
        if (!hasMethodNamed(metaObject, method)) {
            errorStream << "STAGED_BACKEND_PROBE_ERROR missing SSH method "
                        << method << '\n';
            return 1;
        }
    }

    outputStream << "STAGED_BACKEND_PROBE_PASS package-relative CosmixBackend "
                    "loaded; tray metadata and SSH surface active\n";
    return 0;
}
