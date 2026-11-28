#!/bin/sh
# Decide whether an exact Pali crate version still needs publication.
set -eu

tag_name=${1:?usage: crates-io-publish-status.sh TAG_NAME CRATE_FILE}
crate_file=${2:?usage: crates-io-publish-status.sh TAG_NAME CRATE_FILE}
output_file=${GITHUB_OUTPUT:?GITHUB_OUTPUT must name the workflow output file}

version=$("$(dirname "$0")/verify-release-tag.sh" "$tag_name")

response_file=$(mktemp)
trap 'rm -f "$response_file"' EXIT HUP INT TERM

# This exact-version API is the authority for the idempotent-success branch.
# curl transport failures remain failures; only HTTP 200 means already exists.
status=$(curl --silent --show-error \
    --connect-timeout 10 \
    --max-time 30 \
    --user-agent 'pali-release-workflow/1' \
    --output "$response_file" \
    --write-out '%{http_code}' \
    "https://crates.io/api/v1/crates/pali/${version}")

case "$status" in
    200)
        intended_checksum=$(sha256sum "$crate_file" | awk '{ print $1 }')
        published_checksum=$(jq -er '.version.checksum | select(type == "string")' "$response_file") || {
            echo "crates.io response lacks a valid checksum" >&2
            exit 1
        }
        if ! printf '%s\n' "$published_checksum" | grep -Eq '^[0-9a-f]{64}$'; then
            echo "crates.io returned malformed checksum: $published_checksum" >&2
            exit 1
        fi
        if [ "$published_checksum" != "$intended_checksum" ]; then
            echo "crates.io checksum $published_checksum does not match intended crate $intended_checksum" >&2
            exit 1
        fi
        echo "already_published=true" >> "$output_file"
        echo "publish=false" >> "$output_file"
        echo "pali $version already exists on crates.io; publication is complete"
        ;;
    404)
        echo "already_published=false" >> "$output_file"
        echo "publish=true" >> "$output_file"
        echo "pali $version is absent from crates.io; publication is required"
        ;;
    *)
        echo "crates.io exact-version lookup returned HTTP $status" >&2
        exit 1
        ;;
esac
