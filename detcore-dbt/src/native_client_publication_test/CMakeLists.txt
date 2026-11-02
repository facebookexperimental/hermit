cmake_minimum_required(VERSION 3.21)
project(native_client_publication C)
find_package(DynamoRIO REQUIRED)
add_library(reverie_dbt_client SHARED client.c)
configure_DynamoRIO_client(reverie_dbt_client)
# The gate affects only this test's real linker, after its real output unlink.
if(EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/gate.enabled")
  set_property(TARGET reverie_dbt_client PROPERTY C_LINKER_LAUNCHER
    "${CMAKE_COMMAND};-E;env;LD_PRELOAD=${CMAKE_CURRENT_SOURCE_DIR}/link_gate.so;PUBLICATION_GATE=${CMAKE_CURRENT_SOURCE_DIR}/gate.sock")
endif()
