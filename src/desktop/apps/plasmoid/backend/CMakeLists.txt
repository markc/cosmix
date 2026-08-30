set(COSMIX_APPLET_INSTALL_ROOT
    "${KDE_INSTALL_DATADIR}/plasma/plasmoids/dev.cosmix.plasmoid")

include(GenerateExportHeader)

add_library(cosmixplasmoidbackend SHARED)
generate_export_header(cosmixplasmoidbackend)

ecm_add_qml_module(cosmixplasmoidbackend
    URI CosmixBackend
    VERSION 1.0
    GENERATE_PLUGIN_SOURCE
)

target_sources(cosmixplasmoidbackend PRIVATE
    dbustypes.cpp
    dbustypes.h
    listmodels.cpp
    listmodels.h
    traydbridge.cpp
    traydbridge.h
)

target_include_directories(cosmixplasmoidbackend
    PUBLIC
        ${CMAKE_CURRENT_SOURCE_DIR}
        ${CMAKE_CURRENT_BINARY_DIR}
)

target_link_libraries(cosmixplasmoidbackend
    PUBLIC
        Qt6::Core
        Qt6::DBus
        Qt6::Qml
)

set_target_properties(cosmixplasmoidbackend PROPERTIES
    INSTALL_RPATH "$ORIGIN"
)
set_target_properties(cosmixplasmoidbackendplugin PROPERTIES
    INSTALL_RPATH "$ORIGIN"
)

ecm_finalize_qml_module(cosmixplasmoidbackend
    DESTINATION "${COSMIX_APPLET_INSTALL_ROOT}/contents/ui"
)

install(
    TARGETS cosmixplasmoidbackend
    LIBRARY DESTINATION
        "${COSMIX_APPLET_INSTALL_ROOT}/contents/ui/CosmixBackend"
)
