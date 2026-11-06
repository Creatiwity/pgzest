#!/usr/bin/env bash
# Publishes the workspace's crates to crates.io, in dependency order, and
# can be run again after a partial publication: a version already on
# crates.io is skipped, and a 429 (crates.io limits new crates to a burst
# of 5, then one every 10 minutes) waits for the delay it announces, then
# retries. Needs CARGO_REGISTRY_TOKEN.
set -euo pipefail

agent="pgzest-release (https://github.com/Creatiwity/pgzest)"

# Publishable crates, each after its normal and build dependencies.
order=$(cargo metadata --format-version 1 --no-deps --locked | python3 -c '
import json, sys
meta = json.load(sys.stdin)
pkgs = {p["name"]: p for p in meta["packages"] if p.get("publish") != []}
deps = {n: sorted(d["name"] for d in p["dependencies"]
                  if d["name"] in pkgs and d["kind"] in (None, "build"))
        for n, p in pkgs.items()}
done, out = set(), []
def visit(n):
    if n in done:
        return
    done.add(n)
    for d in deps[n]:
        visit(d)
    out.append(n + " " + pkgs[n]["version"])
for n in sorted(pkgs):
    visit(n)
print("\n".join(out))
')

published() {
  [ "$(curl -s -o /dev/null -w '%{http_code}' -A "$agent" "https://crates.io/api/v1/crates/$1/$2")" = 200 ]
}

while read -r name version; do
  if published "$name" "$version"; then
    echo "$name $version: already on crates.io"
    continue
  fi
  for attempt in $(seq 1 30); do
    if out=$(cargo publish -p "$name" --locked 2>&1); then
      echo "$out" | tail -n 3
      break
    fi
    echo "$out" | tail -n 5
    if ! grep -q '429 Too Many Requests' <<<"$out"; then
      echo "::error::$name $version: publication failed"
      exit 1
    fi
    # "Please try again after Fri, 06 Nov 2026 22:17:58 GMT"
    until_date=$(grep -o 'try again after [^.]* GMT' <<<"$out" | sed 's/try again after //' || true)
    wait=600
    if [ -n "$until_date" ]; then
      wait=$(( $(date -d "$until_date" +%s) - $(date +%s) + 15 ))
      [ "$wait" -lt 15 ] && wait=15
    fi
    echo "$name: rate limited, retrying in ${wait}s (attempt $attempt)"
    sleep "$wait"
  done
  published "$name" "$version" || { echo "::error::$name $version: still not published"; exit 1; }
done <<<"$order"
