#!/usr/bin/env bash
#
# Prints the newest ethpandaops image tag for each client in
# network_params.yaml, so those pins can be refreshed instead of guessed.
#
#   ./refresh-tags.sh      newest tags on the highest-numbered devnet
#   ./refresh-tags.sh 8    newest tags on glamsterdam-devnet-8
#
# This is a committed script rather than a README one-liner because the tags
# rotate every few weeks and a stale or invented tag fails Kurtosis's label
# validation before a single container starts (#17). Two details make the
# obvious query quietly wrong:
#
#   * Docker Hub does not return tags in date order, so "take the last one"
#     picks an arbitrary build. Sort on last_updated.
#   * Without the name= filter, the first page of ethpandaops/nethermind holds
#     no glamsterdam tags at all, and the query reports the client as having no
#     build when it has thirty.

set -euo pipefail

REPOS=(reth nethermind lighthouse)
HUB="https://hub.docker.com/v2/repositories/ethpandaops"

command -v jq >/dev/null || { echo "refresh-tags.sh needs jq" >&2; exit 1; }

# Every tag in repo $1 whose name contains $2, as "<last_updated> <name>" lines.
tags() {
  local repo=$1 url="$HUB/$1/tags?page_size=100&name=$2" page
  while [[ -n $url && $url != "null" ]]; do
    page=$(curl -sf "$url") || { echo "could not query ethpandaops/$repo" >&2; return 1; }
    jq -r '.results[] | "\(.last_updated) \(.name)"' <<<"$page"
    url=$(jq -r '.next' <<<"$page")
  done
}

# Newest concretely-pinned tag for repo $1 on devnet $2, or empty if none.
#
# The `|| true` matters: under pipefail a grep that matches nothing would fail
# the whole pipeline, and "this client has no build for this devnet yet" is a
# result the caller has to report, not an error that should kill the script. A
# curl or jq failure inside tags() still propagates.
newest() {
  tags "$1" "glamsterdam-devnet-$2" \
    | { grep -E " glamsterdam-devnet-$2-[a-f0-9]{7}$" || true; } \
    | sort \
    | tail -1 \
    | awk '{print $2}'
}

devnet=${1:-}
if [[ -z $devnet ]]; then
  devnet=$(tags reth glamsterdam-devnet \
    | sed -nE 's/.* glamsterdam-devnet-([0-9]+)-[a-f0-9]{7}$/\1/p' \
    | sort -n | tail -1)
  [[ -n $devnet ]] || { echo "no glamsterdam-devnet tags found" >&2; exit 1; }
  echo "# highest devnet found: glamsterdam-devnet-$devnet" >&2
fi

status=0
for repo in "${REPOS[@]}"; do
  tag=$(newest "$repo" "$devnet")
  if [[ -z $tag ]]; then
    # Not necessarily fatal: a client can lag a devnet bump by a few days. It
    # does mean this devnet number is not yet usable for a two-client network.
    echo "$repo: no glamsterdam-devnet-$devnet build published yet" >&2
    status=1
  else
    echo "ethpandaops/$repo:$tag"
  fi
done
exit $status
