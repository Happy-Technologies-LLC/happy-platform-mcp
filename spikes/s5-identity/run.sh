#!/usr/bin/env bash
# Reproduce the P1-S5 measurements locally (fake Entra). Needs docker compose and python3 with `requests`.
set -euo pipefail
cd "$(dirname "$0")"
docker compose up -d --wait 2>/dev/null || docker compose up -d
python3 -m pip install -q -r scripts/requirements.txt
python3 scripts/configure_hpm.py                 # IdP 'entra' + scopes (UPSTREAM=fake by default)
python3 scripts/measure.py "$@"                  # all profiles (~25 min); or: measure.py P0-stock P4-reconciler-graph
python3 scripts/rotation_check.py
python3 scripts/summarize.py results/run-main.json
# docker compose down -v                         # stop and delete state
