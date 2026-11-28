#!/bin/sh
# Bind a manual repair to an exact modern source label or reviewed legacy digest.
set -eu

image=${1:?usage: verify-manual-release-image.sh IMAGE TAG SOURCE_SHA [ALLOWLIST]}
tag_name=${2:?usage: verify-manual-release-image.sh IMAGE TAG SOURCE_SHA [ALLOWLIST]}
source_sha=${3:?usage: verify-manual-release-image.sh IMAGE TAG SOURCE_SHA [ALLOWLIST]}
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
allowlist=${4:-"$script_dir/../release/legacy-image-digests.txt"}
caller_output=${GITHUB_OUTPUT:-}
status_output=$(mktemp)
trap 'rm -f "$status_output"' EXIT HUP INT TERM

"$script_dir/validate-release-tag.sh" "$tag_name"
printf '%s\n' "$source_sha" | grep -Eq '^[0-9a-f]{40}$' || {
    echo "invalid release source SHA: $source_sha" >&2
    exit 1
}

matches=$(awk -v tag="$tag_name" '$1 == tag { print $2 }' "$allowlist")
match_count=$(printf '%s\n' "$matches" | awk 'NF { count++ } END { print count + 0 }')
case "$match_count" in
    0)
        GITHUB_OUTPUT="$status_output" "$script_dir/ghcr-image-status.sh" source \
            "$image" "${tag_name#v}" "$source_sha"
        grep -Fx 'exists=true' "$status_output" >/dev/null || {
            echo "modern release image $tag_name is absent" >&2
            exit 1
        }
        image_digest=$(awk -F= '$1 == "image_digest" { print $2 }' "$status_output")
        printf '%s\n' "$image_digest" | grep -Eq '^sha256:[0-9a-f]{64}$' || {
            echo "modern release image $tag_name lacks a verified digest" >&2
            exit 1
        }
        [ -z "$caller_output" ] || echo "image_digest=$image_digest" >> "$caller_output"
        ;;
    1)
        expected_digest=$matches
        printf '%s\n' "$expected_digest" | grep -Eq '^sha256:[0-9a-f]{64}$' || {
            echo "invalid legacy image digest for $tag_name" >&2
            exit 1
        }
        GITHUB_OUTPUT="$status_output" "$script_dir/ghcr-image-status.sh" require-digest \
            "$image" "${tag_name#v}" "$expected_digest"
        grep -Fx 'verified=true' "$status_output" >/dev/null || {
            echo "legacy release image $tag_name is absent" >&2
            exit 1
        }
        [ -z "$caller_output" ] || echo "image_digest=$expected_digest" >> "$caller_output"
        ;;
    *)
        echo "duplicate legacy image digest entries for $tag_name" >&2
        exit 1
        ;;
esac
