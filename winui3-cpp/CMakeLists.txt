cmake_minimum_required(VERSION 3.28)
project(EchoDupWinUI3 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 23)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

add_executable(EchoDupWinUI3
    App/main.cpp
    Core/Audio/AudioReader.cpp
    Core/Detect/Fingerprint.cpp
)

target_include_directories(EchoDupWinUI3 PRIVATE
    ${CMAKE_CURRENT_SOURCE_DIR}
)
