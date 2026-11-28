#!/bin/sh
# Idempotently publish one release archive/checksum pair.
set -eu

repository=${1:?usage: upload-release-asset-pair.sh REPOSITORY TAG ARCHIVE CHECKSUM}
tag_name=${2:?usage: upload-release-asset-pair.sh REPOSITORY TAG ARCHIVE CHECKSUM}
archive=${3:?usage: upload-release-asset-pair.sh REPOSITORY TAG ARCHIVE CHECKSUM}
checksum=${4:?usage: upload-release-asset-pair.sh REPOSITORY TAG ARCHIVE CHECKSUM}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    else
        echo "no SHA-256 tool available (need sha256sum or shasum)" >&2
        return 1
    fi
}

local_checksum_fields=$(awk 'END { if (NR == 1) print NF; else print 0 }' "$checksum")
local_declared_digest=$(awk 'NR == 1 { print $1 }' "$checksum")
local_declared_name=$(awk 'NR == 1 { print $2 }' "$checksum")
local_archive_digest=$(sha256_file "$archive")
if [ "$local_checksum_fields" != 2 ] ||
    ! printf '%s\n' "$local_declared_digest" | grep -Eq '^[0-9a-fA-F]{64}$' ||
    [ "$local_declared_name" != "$(basename "$archive")" ] ||
    [ "$local_declared_digest" != "$local_archive_digest" ]; then
    echo "prepared release archive/checksum pair is not internally exact" >&2
    exit 1
fi

asset_names=$(gh release view "$tag_name" --repo "$repository" --json assets --jq '.assets[].name')

has_asset() {
    printf '%s\n' "$asset_names" | grep -Fxq "$(basename "$1")"
}

archive_exists=false
checksum_exists=false
has_asset "$archive" && archive_exists=true
has_asset "$checksum" && checksum_exists=true

remote_dir=$(mktemp -d)
trap 'rm -rf "$remote_dir"' EXIT HUP INT TERM

if [ "$archive_exists" = true ]; then
    gh release download "$tag_name" --repo "$repository" \
        --pattern "$(basename "$archive")" --dir "$remote_dir"
fi
if [ "$checksum_exists" = true ]; then
    gh release download "$tag_name" --repo "$repository" \
        --pattern "$(basename "$checksum")" --dir "$remote_dir"
fi

if [ "$archive_exists" = true ] && [ "$checksum_exists" = true ]; then
    remote_archive="$remote_dir/$(basename "$archive")"
    remote_checksum="$remote_dir/$(basename "$checksum")"
    checksum_fields=$(awk 'END { if (NR == 1) print NF; else print 0 }' "$remote_checksum")
    declared_digest=$(awk 'NR == 1 { print $1 }' "$remote_checksum")
    declared_name=$(awk 'NR == 1 { print $2 }' "$remote_checksum")
    remote_archive_digest=$(sha256_file "$remote_archive")
    if [ "$checksum_fields" = 2 ] &&
        printf '%s\n' "$declared_digest" | grep -Eq '^[0-9a-fA-F]{64}$' &&
        [ "$declared_name" = "$(basename "$archive")" ] &&
        [ "$declared_digest" = "$remote_archive_digest" ]; then
        if cmp -s "$remote_archive" "$archive" && cmp -s "$remote_checksum" "$checksum"; then
            echo "Exact source-bound release asset pair already exists; preserving it"
            exit 0
        fi
        echo "existing release assets are internally valid but differ from the exact release build" >&2
        exit 1
    fi
    echo "existing release asset pair has an invalid checksum; refusing replacement" >&2
    exit 1
fi

if [ "$archive_exists" = true ]; then
    remote_archive="$remote_dir/$(basename "$archive")"
    repaired_checksum="$remote_dir/$(basename "$checksum")"
    remote_archive_digest=$(sha256_file "$remote_archive")
    if [ "$remote_archive_digest" != "$local_archive_digest" ]; then
        echo "existing archive differs from the exact release build; refusing checksum repair" >&2
        exit 1
    fi
    printf '%s  %s\n' "$remote_archive_digest" "$(basename "$archive")" > "$repaired_checksum"
    gh release upload "$tag_name" "$repaired_checksum" --repo "$repository"
    echo "Repaired missing checksum without replacing the existing archive"
    exit 0
fi

if [ "$checksum_exists" = true ]; then
    remote_checksum="$remote_dir/$(basename "$checksum")"
    if cmp -s "$remote_checksum" "$checksum"; then
        gh release upload "$tag_name" "$archive" --repo "$repository"
        echo "Repaired missing archive from its exact source-bound checksum"
        exit 0
    fi
    echo "release checksum exists without its exact source-bound archive; refusing repair" >&2
    exit 1
fi

gh release upload "$tag_name" "$archive" "$checksum" --repo "$repository"
