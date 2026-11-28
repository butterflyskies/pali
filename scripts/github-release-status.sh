#!/bin/sh
# Decide whether the exact GitHub release already exists for a rerun.
set -eu

repository=${1:?usage: github-release-status.sh OWNER/REPO TAG_NAME}
tag_name=${2:?usage: github-release-status.sh OWNER/REPO TAG_NAME}
output_file=${GITHUB_OUTPUT:?GITHUB_OUTPUT must name the workflow output file}
: "${GH_TOKEN:?GH_TOKEN is required for the GitHub release lookup}"

if ! printf '%s\n' "$repository" | grep -Eq '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$'; then
    echo "invalid GitHub repository: $repository" >&2
    exit 1
fi
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
"$script_dir/validate-release-tag.sh" "$tag_name"

response_file=$(mktemp)
trap 'rm -f "$response_file"' EXIT HUP INT TERM

status=$(curl --silent --show-error \
    --connect-timeout 10 \
    --max-time 30 \
    --header "Authorization: Bearer $GH_TOKEN" \
    --header 'Accept: application/vnd.github+json' \
    --header 'X-GitHub-Api-Version: 2022-11-28' \
    --output "$response_file" \
    --write-out '%{http_code}' \
    "https://api.github.com/repos/${repository}/releases/tags/${tag_name}")

case "$status" in
    200)
        echo "exists=true" >> "$output_file"
        echo "GitHub release $tag_name already exists; reusing it"
        ;;
    404)
        echo "exists=false" >> "$output_file"
        echo "GitHub release $tag_name does not exist; creation is required"
        ;;
    *)
        echo "GitHub release lookup returned HTTP $status" >&2
        exit 1
        ;;
esac
