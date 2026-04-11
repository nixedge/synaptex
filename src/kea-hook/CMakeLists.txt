cmake_minimum_required(VERSION 3.16)
project(synaptex_hook CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# ── Kea include path ──────────────────────────────────────────────────────────
# On NixOS pass -DKEA_INCLUDE_DIR=$(kea-dev)/include/kea via cmake invocation,
# or set KEA_INCLUDE_DIR in the environment / nix shell.
if(NOT DEFINED KEA_INCLUDE_DIR)
    find_path(KEA_INCLUDE_DIR hooks/hooks.h
        PATH_SUFFIXES kea
        HINTS
            /nix/store
            /usr/include
            /usr/local/include
    )
endif()

if(NOT KEA_INCLUDE_DIR)
    message(FATAL_ERROR
        "Kea headers not found.  Pass -DKEA_INCLUDE_DIR=<path/to/kea/include/kea>")
endif()

message(STATUS "Kea headers: ${KEA_INCLUDE_DIR}")

# ── Shared library ────────────────────────────────────────────────────────────
add_library(synaptex_hook SHARED src/synaptex_hook.cc)

target_include_directories(synaptex_hook PRIVATE ${KEA_INCLUDE_DIR})

# Kea hooks must not link against libkea-* at load time — the symbols are
# provided by the Kea process itself at runtime.  Build as a pure shared
# library with no SONAME.
set_target_properties(synaptex_hook PROPERTIES
    PREFIX ""
    OUTPUT_NAME "synaptex_hook"
)

target_compile_options(synaptex_hook PRIVATE
    -Wall -Wextra
)
