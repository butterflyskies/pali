#!/bin/sh
# Verify the prepared OCI archive has one exact source reference for promotion.
set -eu

archive=${1:?usage: verify-oci-release-archive.sh ARCHIVE EXPECTED_REFERENCE}
expected_reference=${2:?usage: verify-oci-release-archive.sh ARCHIVE EXPECTED_REFERENCE}
index_file=$(mktemp)
trap 'rm -f "$index_file"' EXIT HUP INT TERM

tar -xOf "$archive" index.json > "$index_file"
jq -e --arg expected "$expected_reference" '
    (.manifests | length) == 1 and
    .manifests[0].annotations["org.opencontainers.image.ref.name"] == $expected
' "$index_file" >/dev/null || {
    echo "OCI archive does not contain exactly one $expected_reference reference" >&2
    exit 1
}

image_digest=$(jq -er --arg expected "$expected_reference" '
    .manifests[] |
    select(.annotations["org.opencontainers.image.ref.name"] == $expected) |
    .digest | select(type == "string")
' "$index_file")
if ! printf '%s\n' "$image_digest" | grep -Eq '^sha256:[0-9a-f]{64}$'; then
    echo "OCI archive contains an invalid prepared image digest" >&2
    exit 1
fi
[ -z "${GITHUB_OUTPUT:-}" ] || echo "image_digest=$image_digest" >> "$GITHUB_OUTPUT"
