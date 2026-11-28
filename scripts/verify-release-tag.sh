#!/bin/sh
# Verify that a v<semver> release tag matches the checked-out package version.
set -eu

tag_name=${1:?usage: verify-release-tag.sh TAG_NAME}
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
"$script_dir/validate-release-tag.sh" "$tag_name"

version=${tag_name#v}
cargo_version=$(awk '
    /^\[package\][[:space:]]*$/ { in_package = 1; next }
    /^\[/ { in_package = 0 }
    in_package && /^version[[:space:]]*=/ {
        line = $0
        sub(/^[^=]*=[[:space:]]*"/, "", line)
        sub(/".*/, "", line)
        print line
        exit
    }
' Cargo.toml)

if [ -z "$cargo_version" ] || [ "$version" != "$cargo_version" ]; then
    echo "tag $tag_name does not match Cargo.toml package version ${cargo_version:-<missing>}" >&2
    exit 1
fi

printf '%s\n' "$version"
