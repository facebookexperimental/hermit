#!/usr/bin/env bash
# The local validation ledger is the landing authority.
exec ./scripts/validate.rs "$@"
