#!/bin/sh
# Create a draft GitHub release, tolerating only an exact concurrent creation.
set -eu

repository=${1:?usage: create-github-draft-release.sh OWNER/REPO TAG IS_PRERELEASE}
tag_name=${2:?usage: create-github-draft-release.sh OWNER/REPO TAG IS_PRERELEASE}
is_prerelease=${3:?usage: create-github-draft-release.sh OWNER/REPO TAG IS_PRERELEASE}
: "${GH_TOKEN:?GH_TOKEN is required}"

if ! printf '%s\n' "$repository" | grep -Eq '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$'; then
    echo "invalid GitHub repository: $repository" >&2
    exit 1
fi
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
"$script_dir/validate-release-tag.sh" "$tag_name"

case "$is_prerelease" in
    true) set -- --prerelease ;;
    false) set -- ;;
    *) echo "invalid prerelease classification: $is_prerelease" >&2; exit 1 ;;
esac

if gh release create "$tag_name" --draft "$@" \
    --verify-tag --title "$tag_name" --repo "$repository"; then
    exit 0
fi

# Another exact-tag run may have won the create race. Reconcile only a
# canonical 200 response; 404, auth, network, and every ambiguous state remain
# fatal. This keeps push publication alive when manual repair creates first.
status_output=$(mktemp)
trap 'rm -f "$status_output"' EXIT HUP INT TERM
if GITHUB_OUTPUT="$status_output" \
    "$script_dir/github-release-status.sh" "$repository" "$tag_name" &&
    grep -Fx 'exists=true' "$status_output" >/dev/null; then
    echo "Concurrent exact release $tag_name exists; continuing"
    exit 0
fi

echo "failed to create or reconcile exact GitHub release $tag_name" >&2
exit 1
