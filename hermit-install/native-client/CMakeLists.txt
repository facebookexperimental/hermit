# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

cmake_minimum_required(VERSION 3.14)
project(hermit_reverie_dbi_client C)

if(NOT DEFINED DynamoRIO_DIR)
  message(FATAL_ERROR "DynamoRIO_DIR must point to Cargo's vendored install")
endif()
find_package(DynamoRIO REQUIRED)

if(NOT DEFINED REVERIE_DBI_NATIVE_SOURCE)
  message(FATAL_ERROR "REVERIE_DBI_NATIVE_SOURCE must point to client.c")
endif()
if(NOT DEFINED HERMIT_RESOURCE_DIR)
  message(FATAL_ERROR "HERMIT_RESOURCE_DIR must name the package resource directory")
endif()

add_library(detcore_dbi_link_stub SHARED detcore_dbi_link_stub.c)
set_target_properties(detcore_dbi_link_stub PROPERTIES OUTPUT_NAME detcore_dbi)

add_library(reverie_dbi_client SHARED "${REVERIE_DBI_NATIVE_SOURCE}")
configure_DynamoRIO_client(reverie_dbi_client)
use_DynamoRIO_extension(reverie_dbi_client drmgr)
use_DynamoRIO_extension(reverie_dbi_client drreg)
use_DynamoRIO_extension(reverie_dbi_client drx)
use_DynamoRIO_extension(reverie_dbi_client drwrap)
target_link_libraries(reverie_dbi_client detcore_dbi_link_stub)

set(hermit_rpath
    "$ORIGIN;$ORIGIN/dynamorio/lib64/release;$ORIGIN/dynamorio/ext/lib64/release")
set_target_properties(
  reverie_dbi_client
  PROPERTIES LIBRARY_OUTPUT_DIRECTORY "${HERMIT_RESOURCE_DIR}"
             BUILD_RPATH "${hermit_rpath}"
             INSTALL_RPATH "${hermit_rpath}"
             BUILD_WITH_INSTALL_RPATH ON
             INSTALL_RPATH_USE_LINK_PATH OFF)
