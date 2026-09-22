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
    src/vfcurvewidget.cpp src/vfcurvewidget.h
    src/nvidiatab.cpp    src/nvidiatab.h
    src/tray.cpp         src/tray.h
)
target_compile_definitions(legion-power-manager PRIVATE
    LPM_VERSION="${PROJECT_VERSION}"
    LPM_HELPER_DIR="${LPM_HELPER_DIR}"
    QT_NO_KEYWORDS)
target_compile_options(legion-power-manager PRIVATE -Wall -Wextra -Wpedantic)
target_link_libraries(legion-power-manager PRIVATE Qt6::Widgets Qt6::Network ${CMAKE_DL_LIBS})

include(GNUInstallDirs)
install(TARGETS legion-power-manager RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR})
install(FILES data/legion-power-manager.desktop DESTINATION ${CMAKE_INSTALL_DATADIR}/applications)
install(FILES data/legion-power-manager-autostart.desktop DESTINATION ${CMAKE_INSTALL_FULL_SYSCONFDIR}/xdg/autostart
        RENAME legion-power-manager.desktop)
install(FILES data/legion-power-manager.svg DESTINATION ${CMAKE_INSTALL_DATADIR}/icons/hicolor/scalable/apps)
