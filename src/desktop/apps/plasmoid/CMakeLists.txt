cmake_minimum_required(VERSION 3.24)

project(cosmix-plasmoid VERSION 0.3.8 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)
set(BUILD_SHARED_LIBS ON)

find_package(ECM 6.0 REQUIRED NO_MODULE)
set(CMAKE_MODULE_PATH ${ECM_MODULE_PATH})

include(KDEInstallDirs6)
include(CTest)
include(ECMQmlModule)
include(KDECompilerSettings NO_POLICY_SCOPE)
include(KDECMakeSettings)

find_package(Qt6 6.8 REQUIRED COMPONENTS Core DBus Qml Test)
find_package(Plasma 6.0 REQUIRED)

add_subdirectory(backend)

if(BUILD_TESTING)
    add_subdirectory(tests)
endif()

plasma_install_package(package dev.cosmix.plasmoid)
