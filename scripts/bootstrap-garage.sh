#!/usr/bin/env bash
# One-time Garage setup: assign a storage layout, create the media bucket, and
# mint an S3 key for the API server.
#
# The layout step is the one that bites. A fresh Garage node starts, answers
# `garage status`, and passes its healthcheck while holding NO ROLE — and a
# node with no role serves no S3 at all. Every request comes back as a cluster
# error that reads like a credentials problem. Nothing works until a layout is
# assigned AND applied.
#
# Safe to re-run: every step below checks for its own result first. What it
# cannot do is re-print an existing key's secret — Garage shows that once, at
# creation. See the note at the bottom if you lost it.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
backend_root="$(dirname "$script_dir")"
cd "$backend_root"

bucket="${S3_BUCKET:-r4chii-media}"
key_name="r4chii-server"

garage() { docker compose exec -T garage /garage "$@"; }

if ! docker compose ps --status running --services | grep -qx garage; then
  echo "error: the garage service isn't running. Start it with:" >&2
  echo "  docker compose up -d garage" >&2
  exit 1
fi

# --- 1. Layout -------------------------------------------------------------
if garage layout show 2>/dev/null | grep -q "$(garage node id -q 2>/dev/null | cut -d@ -f1)"; then
  echo "layout: already assigned, leaving it alone"
else
  node_id="$(garage node id -q | cut -d@ -f1)"
  echo "layout: assigning role to node ${node_id}"
  # -c is this node's usable capacity, not a quota on what we store. One zone
  # because there is one machine; pretending otherwise would let Garage think
  # it has redundancy it does not have.
  garage layout assign "$node_id" -z dc1 -c 10G
  garage layout apply --version 1
fi

# --- 2. Bucket -------------------------------------------------------------
if garage bucket list | grep -q "\b${bucket}\b"; then
  echo "bucket: ${bucket} already exists"
else
  echo "bucket: creating ${bucket}"
  garage bucket create "$bucket"
fi

# --- 3. Key ----------------------------------------------------------------
if garage key list | grep -q "\b${key_name}\b"; then
  echo "key: ${key_name} already exists — not recreating it"
  echo
  echo "If you don't have its secret any more, Garage cannot show it again."
  echo "Delete and remake the key, then update .env:"
  echo "  docker compose exec garage /garage key delete --yes ${key_name}"
  echo "  ./scripts/bootstrap-garage.sh"
  exit 0
fi

echo "key: creating ${key_name}"
key_output="$(garage key create "$key_name")"
garage bucket allow --read --write --owner "$bucket" --key "$key_name" >/dev/null

echo
echo "Bucket and key are ready. Put these in .env — the secret key is shown"
echo "ONCE, right here, and Garage will never print it again:"
echo
echo "$key_output" | grep -E "Key ID|Secret key" || echo "$key_output"
echo
echo "  S3_ACCESS_KEY_ID=<Key ID above>"
echo "  S3_SECRET_ACCESS_KEY=<Secret key above>"
