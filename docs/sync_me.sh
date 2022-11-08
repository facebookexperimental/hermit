#!/bin/bash
# (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

set -xeuo pipefail

buck run //scripts/richiev/wiki_sync:wiki_sync -- \
     --wiki-root=Hermit/Docs/ \
     --md-root=./
