#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

python3 -m unittest discover -s tests -v
node tests/test_refresh_coordinator.js
node tests/test_service_worker_runtime.js
node --check web/app.js
node --check web/i18n.js
node --check web/refresh-coordinator.js
node --check web/service-worker.js
python3 -m json.tool web/manifest.webmanifest >/dev/null

echo "PWA tests: OK"
