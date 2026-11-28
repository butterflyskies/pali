#!/bin/sh
# Validate the release tag shape shared by Forgejo and GitHub workflows.
set -eu

tag_name=${1:?usage: validate-release-tag.sh TAG_NAME}

# A tag is exactly one line. The line-oriented match below accepts any input
# with one matching line, and callers use the raw input for concurrency groups.
newline='
'
carriage_return=$(printf '\r')
case "$tag_name" in
    *"$newline"* | *"$carriage_return"*)
        echo 'invalid release tag: contains a line break' >&2
        exit 1
        ;;
esac

# Pali release tags are strict lowercase SemVer without build metadata. OCI
# tags cannot represent `+` reversibly, and GitHub concurrency keys fold case;
# a canonical lowercase dialect avoids both alias classes.
if ! printf '%s\n' "$tag_name" | grep -Eq '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9a-z-]+(\.[0-9a-z-]+)*)?$'; then
    echo "invalid release tag: $tag_name" >&2
    exit 1
fi

case "$tag_name" in
    *-*) is_prerelease=true ;;
    *) is_prerelease=false ;;
esac

if [ "$is_prerelease" = true ]; then
    prerelease=${tag_name#*-}
    old_ifs=$IFS
    IFS=.
    set -- $prerelease
    IFS=$old_ifs
    for identifier in "$@"; do
        if printf '%s\n' "$identifier" | grep -Eq '^0[0-9]+$'; then
            echo "invalid numeric prerelease identifier: $identifier" >&2
            exit 1
        fi
    done
fi

if [ -n "${GITHUB_OUTPUT:-}" ]; then
    echo "is_prerelease=$is_prerelease" >> "$GITHUB_OUTPUT"
fi
