#!/bin/sh
# Validate the release tag shape shared by Forgejo and GitHub workflows.
set -eu

tag_name=${1:?usage: validate-release-tag.sh TAG_NAME}

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
