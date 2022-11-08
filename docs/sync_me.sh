#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

set -xeuo pipefail

buck run //scripts/richiev/wiki_sync:wiki_sync -- \
     --wiki-root=Hermit/Docs/ \
     --md-root=./
