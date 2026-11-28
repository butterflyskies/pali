#!/bin/sh
# Create the local release tag for a Cargo.toml version change.
#
# Forgejo is the sole tag authority. This local tag is annotated and unsigned:
# it only drives the exact-boundary checkout and gates, in a job that holds no
# signing key. The Forgejo tag-release job creates the SSH-signed tag that is
# pushed, with create-release-tag.sh.
set -eu

before_sha=${1:?usage: tag-release-on-version-change.sh BEFORE_SHA}
output_file=${FORGEJO_OUTPUT:-${GITHUB_OUTPUT:-}}

if [ -z "$output_file" ]; then
    echo "FORGEJO_OUTPUT or GITHUB_OUTPUT must name the workflow output file" >&2
    exit 1
fi

read_version() {
    awk '
        /^\[package\][[:space:]]*$/ { in_package = 1; next }
        /^\[/ { in_package = 0 }
        in_package && /^version[[:space:]]*=/ {
            line = $0
            sub(/^[^=]*=[[:space:]]*"/, "", line)
            sub(/".*/, "", line)
            print line
            exit
        }
    '
}

current_version=$(read_version < Cargo.toml)
if [ -z "$current_version" ]; then
    echo "failed to extract [package].version from Cargo.toml" >&2
    exit 1
fi

case "$before_sha" in
    ''|*[!0-9a-fA-F]*)
        echo "invalid before SHA: $before_sha" >&2
        exit 1
        ;;
esac

if printf '%s' "$before_sha" | grep -Eq '^0+$'; then
    echo "initial repository push has no prior Cargo.toml; refusing to infer a version change" >&2
    exit 1
fi

previous_manifest=$(git show "${before_sha}:Cargo.toml") || {
    echo "cannot read Cargo.toml at before SHA $before_sha" >&2
    exit 1
}
previous_version=$(printf '%s\n' "$previous_manifest" | read_version)
if [ -z "$previous_version" ]; then
    echo "failed to extract prior [package].version from Cargo.toml" >&2
    exit 1
fi

tag_name="v${current_version}"
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
"$script_dir/validate-release-tag.sh" "$tag_name"
{
    echo "version=$current_version"
    echo "tag_name=$tag_name"
} >> "$output_file"

# Reconcile a missed tag after a failed version-bump run: find the first-parent
# commit where the current package version entered canonical history.
version_commit=$(git rev-parse HEAD)
while parent=$(git rev-parse "${version_commit}^" 2>/dev/null); do
    parent_manifest=$(git show "${parent}:Cargo.toml") || exit 1
    parent_version=$(printf '%s\n' "$parent_manifest" | read_version)
    [ "$parent_version" = "$current_version" ] || break
    version_commit=$parent
done
version_parent=$(git rev-parse --verify "${version_commit}^" 2>/dev/null) || version_parent=
if [ -n "$version_parent" ]; then
    release_parent_manifest=$(git show "${version_parent}:Cargo.toml") || exit 1
    release_parent_version=$(printf '%s\n' "$release_parent_manifest" | read_version)
    if [ -z "$release_parent_version" ]; then
        echo "failed to extract release-parent [package].version" >&2
        exit 1
    fi
    if ! "$script_dir/release-version-is-greater.sh" "$current_version" "$release_parent_version"; then
        echo "release version must increase strictly: $release_parent_version -> $current_version" >&2
        exit 1
    fi
fi

if ! awk -v prefix="## [$current_version] - " '
    index($0, prefix) == 1 {
        date = substr($0, length(prefix) + 1)
        if (date ~ /^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$/) {
            found = 1
        }
    }
    END { exit(found ? 0 : 1) }
' CHANGELOG.md; then
    echo "CHANGELOG.md lacks a dated ## [$current_version] release entry" >&2
    exit 1
fi

# The release boundary is the first commit, at or after version entry, that
# adds the matching changelog heading. This admits a version+changelog pair of
# commits without letting later unrelated fixes leak into the tagged release.
release_commit=
if [ -n "$version_parent" ]; then
    candidates=$(git rev-list --first-parent --reverse "${version_parent}..HEAD")
else
    candidates=$(git rev-list --first-parent --reverse HEAD)
fi
for candidate in $candidates; do
    candidate_parent=$(git rev-parse --verify "${candidate}^" 2>/dev/null) || candidate_parent=
    if [ -n "$candidate_parent" ]; then
        changelog_diff=$(git diff --unified=0 "$candidate_parent" "$candidate" -- CHANGELOG.md)
    else
        changelog_diff=$(git diff-tree --root -p --unified=0 "$candidate" -- CHANGELOG.md)
    fi
    if printf '%s\n' "$changelog_diff" |
        awk -v prefix="+## [$current_version] - " '
            index($0, prefix) == 1 {
                date = substr($0, length(prefix) + 1)
                if (date ~ /^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$/) found = 1
            }
            END { exit(found ? 0 : 1) }
        '; then
        release_commit=$candidate
        break
    fi
done
if [ -z "$release_commit" ]; then
    echo "no commit in the $current_version release boundary added its exact dated CHANGELOG.md heading" >&2
    exit 1
fi
git merge-base --is-ancestor "$release_commit" HEAD || {
    echo "release boundary $release_commit is not contained in tested main" >&2
    exit 1
}
echo "release_commit=$release_commit" >> "$output_file"

# An existing tag must identify the exact derived version+changelog boundary.
# A same-version ancestor can be either pre-finalization or later unrelated
# code and is not an equivalent release identity.
if git show-ref --verify --quiet "refs/tags/${tag_name}"; then
    tag_commit=$(git rev-parse "refs/tags/${tag_name}^{}")
    if [ "$tag_commit" = "$release_commit" ]; then
        echo "release=false" >> "$output_file"
        echo "Tag $tag_name already identifies exact release boundary $release_commit; leaving its object unchanged"
        exit 0
    fi
    echo "tag $tag_name resolves to $tag_commit, not exact release boundary $release_commit" >&2
    exit 1
fi

TAG_SIGNING=none "$script_dir/create-release-tag.sh" "$tag_name" "$release_commit"
echo "release=true" >> "$output_file"
echo "Created reconciled local tag $tag_name at release boundary $release_commit; the tag-release job signs the tag it pushes"
