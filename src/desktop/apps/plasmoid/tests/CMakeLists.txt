find_program(DBUS_RUN_SESSION_EXECUTABLE dbus-run-session REQUIRED)

add_executable(tst_backend
    tst_backend.cpp
)

target_link_libraries(tst_backend
    PRIVATE
        cosmixplasmoidbackend
        Qt6::Core
        Qt6::DBus
        Qt6::Test
)

add_test(
    NAME backend
    COMMAND
        "${DBUS_RUN_SESSION_EXECUTABLE}"
        "--config-file=${CMAKE_CURRENT_SOURCE_DIR}/session-bus.conf"
        --
        $<TARGET_FILE:tst_backend>
)

set_tests_properties(backend PROPERTIES TIMEOUT 30)

add_executable(live_bus_probe
    live_bus_probe.cpp
)

target_link_libraries(live_bus_probe
    PRIVATE
        cosmixplasmoidbackend
        Qt6::Core
        Qt6::DBus
)

add_executable(staged_backend_probe
    staged_backend_probe.cpp
)

target_link_libraries(staged_backend_probe
    PRIVATE
        Qt6::Core
        Qt6::Qml
)
