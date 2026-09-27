#!/usr/bin/env bash
# Contract conformance: generated schemas have not drifted, the test workspace validates,
# and every negative fixture is rejected by the real validator with a deterministic code.
set -euo pipefail
cd "$(dirname "$0")/.."
MIRA="${MIRA:-target/debug/mira}"
status=0

for schema in schemas/*.schema.json; do
  name="$(basename "$schema" .schema.json)"
  if ! "$MIRA" schema "$name" | cmp -s - "$schema"; then
    echo "drift: $schema (regenerate with: $MIRA schema $name > $schema)"; status=1
  fi
done

"$MIRA" validate tests/fixtures/workspace/.mira --json >/dev/null || { echo "test workspace failed validation"; status=1; }

while read -r case code; do
  [[ -z "$case" || "$case" == \#* ]] && continue
  got="$({ "$MIRA" validate "tests/fixtures/invalid/$case" --json || true; } | python3 -c 'import json,sys; r=json.load(sys.stdin); print("ok" if r["ok"] else r["error"]["code"])')"
  if [[ "$got" != "$code" ]]; then echo "fixture $case: expected $code, got $got"; status=1; fi
done < tests/fixtures/invalid/EXPECTED

[[ $status -eq 0 ]] && echo "contract: ok"
exit $status
