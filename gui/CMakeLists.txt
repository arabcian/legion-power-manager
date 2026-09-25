cmake_minimum_required(VERSION 3.19)
project(legion-power-manager VERSION 2.0.0 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_AUTOMOC ON)

find_package(Qt6 6.4 REQUIRED COMPONENTS Widgets Network)

# Where the root helpers live. Must match the polkit exec.path annotations.
set(LPM_HELPER_DIR "/usr/libexec/legion-power-manager" CACHE PATH "Installed root helper directory")

add_executable(legion-power-manager
    src/main.cpp
    src/mainwindow.cpp   src/mainwindow.h
    src/theme.cpp        src/theme.h
    src/privileged.cpp   src/privileged.h
    src/platformprofile.cpp src/platformprofile.h
    src/sysinfo.cpp      src/sysinfo.h
    src/hometab.cpp      src/hometab.h
    src/fwattrtab.cpp    src/fwattrtab.h
    src/ryzentab.cpp     src/ryzentab.h
    src/inteltab.cpp     src/inteltab.h
    src/vfcurvewidget.cpp src/vfcurvewidget.h
    src/nvidiatab.cpp    src/nvidiatab.h
    src/optimizetab.cpp  src/optimizetab.h
    src/tray.cpp         src/tray.h
    src/scenes.cpp       src/scenes.h
    src/scenestab.cpp    src/scenestab.h
    src/bootadvisor.cpp  src/bootadvisor.h
    src/bundle.cpp       src/bundle.h
)
# Theme glyphs (checkbox tick, spin/combo chevrons), @2x for HiDPI. Compiled
# with qt_add_resources, not AUTORCC: CMake's Makefile generator runs the
# AUTORCC step through /bin/sh without escaping '(' — a checkout under a path
# like "files (1)" failed with a shell syntax error.
qt_add_resources(legion-power-manager theme
    PREFIX /theme
    BASE data/theme
    FILES
        data/theme/check.png data/theme/check@2x.png
        data/theme/chev-down.png data/theme/chev-down@2x.png
        data/theme/chev-up.png data/theme/chev-up@2x.png
        data/theme/chev-down-off.png data/theme/chev-down-off@2x.png
        data/theme/chev-up-off.png data/theme/chev-up-off@2x.png)

target_compile_definitions(legion-power-manager PRIVATE
    LPM_VERSION="${PROJECT_VERSION}"
    LPM_HELPER_DIR="${LPM_HELPER_DIR}"
    QT_NO_KEYWORDS)
target_compile_options(legion-power-manager PRIVATE -Wall -Wextra -Wpedantic)

# ── Optimization / hardening (driven by install.sh; all optional) ────────────
option(LPM_LTO "Link-time optimization" ON)
option(LPM_NATIVE "Tune for this CPU (-march=native); binary will not run on older CPUs" OFF)
option(LPM_HARDEN "Stack protector, FORTIFY_SOURCE=3, CET, full RELRO, PIE" ON)
set(LPM_PGO "" CACHE STRING "Profile-guided optimization: '' | generate | use")
set(LPM_PGO_DIR "${CMAKE_BINARY_DIR}/pgo" CACHE PATH "PGO profile directory")

if(LPM_LTO)
    include(CheckIPOSupported)
    check_ipo_supported(RESULT _ipo OUTPUT _ipo_msg LANGUAGES CXX)
    if(_ipo)
        set_property(TARGET legion-power-manager PROPERTY INTERPROCEDURAL_OPTIMIZATION ON)
    else()
        message(WARNING "LTO not supported by this toolchain: ${_ipo_msg}")
    endif()
endif()
if(LPM_NATIVE)
    target_compile_options(legion-power-manager PRIVATE -march=native -mtune=native)
endif()
if(LPM_HARDEN)
    include(CheckPIESupported)
    check_pie_supported(LANGUAGES CXX)
    set_property(TARGET legion-power-manager PROPERTY POSITION_INDEPENDENT_CODE ON)
    target_compile_options(legion-power-manager PRIVATE
        -fstack-protector-strong -fstack-clash-protection
        $<$<NOT:$<CONFIG:Debug>>:-U_FORTIFY_SOURCE -D_FORTIFY_SOURCE=3>)
    include(CheckCXXCompilerFlag)
    check_cxx_compiler_flag(-fcf-protection=full _cet)
    if(_cet)
        target_compile_options(legion-power-manager PRIVATE -fcf-protection=full)
    endif()
    target_link_options(legion-power-manager PRIVATE -Wl,-z,relro,-z,now -Wl,-z,noexecstack)
endif()
# GCC names each .gcda after the *object's full path*, and the instrumented
# and the final build live in different build directories (build-pgo vs
# build): without stripping that prefix the final build looked for files the
# training run never wrote, and -Wno-missing-profile hid that PGO was a no-op.
set(_pgo_prefix "")
if(CMAKE_CXX_COMPILER_ID STREQUAL "GNU" AND CMAKE_CXX_COMPILER_VERSION VERSION_GREATER_EQUAL 12)
    set(_pgo_prefix "-fprofile-prefix-path=${CMAKE_BINARY_DIR}")
elseif(CMAKE_CXX_COMPILER_ID STREQUAL "GNU" AND NOT LPM_PGO STREQUAL "")
    message(WARNING "GCC < 12 has no -fprofile-prefix-path: PGO only works if generate and use share one build directory")
endif()
if(LPM_PGO STREQUAL "generate")
    target_compile_options(legion-power-manager PRIVATE -fprofile-generate=${LPM_PGO_DIR} ${_pgo_prefix})
    target_link_options(legion-power-manager PRIVATE -fprofile-generate=${LPM_PGO_DIR})
elseif(LPM_PGO STREQUAL "use")
    if(CMAKE_CXX_COMPILER_ID STREQUAL "Clang")
        target_compile_options(legion-power-manager PRIVATE -fprofile-use=${LPM_PGO_DIR}/default.profdata -Wno-profile-instr-unprofiled)
    else()
        target_compile_options(legion-power-manager PRIVATE -fprofile-use=${LPM_PGO_DIR} ${_pgo_prefix} -fprofile-partial-training -Wno-missing-profile
            # moc output lives under each build dir, so its functions' source
            # locations differ between the two builds; GCC then drops the
            # profile for that glue code (harmless) instead of failing.
            -Wno-coverage-mismatch)
    endif()
elseif(NOT LPM_PGO STREQUAL "")
    message(FATAL_ERROR "LPM_PGO must be '', 'generate' or 'use'")
endif()
target_link_libraries(legion-power-manager PRIVATE Qt6::Widgets Qt6::Network ${CMAKE_DL_LIBS})

include(GNUInstallDirs)
install(TARGETS legion-power-manager RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR})
install(FILES data/legion-power-manager.desktop DESTINATION ${CMAKE_INSTALL_DATADIR}/applications)
install(FILES data/legion-power-manager-autostart.desktop DESTINATION ${CMAKE_INSTALL_FULL_SYSCONFDIR}/xdg/autostart
        RENAME legion-power-manager.desktop)
install(FILES data/legion-power-manager.svg DESTINATION ${CMAKE_INSTALL_DATADIR}/icons/hicolor/scalable/apps)
