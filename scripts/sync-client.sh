#!/usr/bin/env bash
# Builds the web client and copies its output into ./client-dist, which the
# Dockerfile copies into the server image. Docker's build context is this
# repository, so the client's build output has to be brought inside it — the
# Dockerfile cannot reach a sibling directory on its own. Run this before
# `docker compose build` whenever the client changed.
#
# This script used to say "there is exactly one client tree, and that is the
# point". That was true while the client lived in the same repository as this
# script, and it stopped being true the moment the project was split. The
# guarantee was structural, and the split removed the structure.
#
# It is worth being precise about what that guarantee was protecting against,
# because it already failed once. A second client briefly existed as its own
# repository. This script kept building the old tree while every edit went to
# the new one, so a live registration ran code nobody was looking at. Later,
# cleaning that up re-imported the *stale* tree over the current one and
# silently reverted three finished features — a four-column shell, message
# formatting, and an emoji picker — roughly three thousand lines. Nobody
# deleted anything on purpose. Someone picked the wrong tree out of two.
#
# So the check below replaces the lost structural guarantee with an explicit
# one: name exactly one client tree, and refuse to run if the filesystem
# offers more than one candidate. A build that silently picks is the failure.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
backend_root="$(dirname "$script_dir")"

# Override for a checkout that is not laid out as siblings. Naming it
# explicitly is always safer than letting the discovery below guess.
client_root="${CLIENT_ROOT:-$backend_root/../r4chii-frontend}"

# Any of these existing besides the chosen one means two client trees are on
# disk, which is the exact condition that lost the three features above. The
# script cannot tell which one you meant, so it does not try.
declare -a stale_candidates=()
for candidate in "$backend_root/../web" "$backend_root/web" "$backend_root/../r4chii-web"; do
  [ -d "$candidate" ] && [ "$(cd "$candidate" && pwd)" != "$(cd "$client_root" 2>/dev/null && pwd)" ] \
    && stale_candidates+=("$candidate")
done

if [ ${#stale_candidates[@]} -gt 0 ]; then
  echo "error: more than one client tree is present, refusing to guess." >&2
  echo "  building:  $client_root" >&2
  for c in "${stale_candidates[@]}"; do
    echo "  also here: $c" >&2
  done
  echo "Delete the tree you are not developing in, or set CLIENT_ROOT to name" >&2
  echo "the one you mean. Whatever this script builds is what ships." >&2
  exit 1
fi

if [ ! -d "$client_root" ]; then
  echo "error: expected the web client at $client_root, not found" >&2
  echo "Clone https://github.com/xhon4/r4chii-frontend next to this repository," >&2
  echo "or set CLIENT_ROOT to where it already is." >&2
  exit 1
fi

# Report the resolved path and its HEAD before building, so the output says
# which tree shipped rather than leaving it to be inferred afterwards.
client_root="$(cd "$client_root" && pwd)"
client_head="$(git -C "$client_root" rev-parse --short HEAD 2>/dev/null || echo 'not a git checkout')"
echo "building client at $client_root ($client_head)"

(cd "$client_root" && npm run build)

rm -rf "$backend_root/client-dist"
cp -r "$client_root/dist" "$backend_root/client-dist"

echo "synced $client_root/dist -> $backend_root/client-dist"
