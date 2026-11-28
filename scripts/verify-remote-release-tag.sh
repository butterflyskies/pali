#!/bin/sh
# Bind an exact remote release tag to the checked-out commit before mutation.
# The tag must be a signed annotated tag object, or a listed legacy tag at its
# exact commit; see TAG_SIGNING below.
set -eu

remote=${1:?usage: verify-remote-release-tag.sh REMOTE TAG EXPECTED_SHA DEFAULT_BRANCH [EXPECTED_TAG_OBJECT]}
tag_name=${2:?usage: verify-remote-release-tag.sh REMOTE TAG EXPECTED_SHA DEFAULT_BRANCH [EXPECTED_TAG_OBJECT]}
expected_sha=${3:?usage: verify-remote-release-tag.sh REMOTE TAG EXPECTED_SHA DEFAULT_BRANCH [EXPECTED_TAG_OBJECT]}
default_branch=${4:?usage: verify-remote-release-tag.sh REMOTE TAG EXPECTED_SHA DEFAULT_BRANCH [EXPECTED_TAG_OBJECT]}
expected_tag_object=${5:-}
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
"$script_dir/validate-release-tag.sh" "$tag_name"

if ! printf '%s\n' "$expected_sha" | grep -Eq '^[0-9a-fA-F]{40}$'; then
    echo "invalid expected release commit: $expected_sha" >&2
    exit 1
fi

remote_refs=$(git ls-remote --tags "$remote" \
    "refs/tags/$tag_name" "refs/tags/$tag_name^{}")
direct=$(printf '%s\n' "$remote_refs" | awk -v ref="refs/tags/$tag_name" '$2 == ref { print $1 }')
peeled=$(printf '%s\n' "$remote_refs" | awk -v ref="refs/tags/$tag_name^{}" '$2 == ref { print $1 }')

actual_sha=${peeled:-$direct}
if [ -z "$actual_sha" ]; then
    echo "remote release tag $tag_name does not exist" >&2
    exit 1
fi
if [ "$actual_sha" != "$expected_sha" ]; then
    echo "remote release tag $tag_name resolves to $actual_sha, not checked-out commit $expected_sha" >&2
    exit 1
fi

# A caller that just created the tag passes its object id: the remote must
# advertise exactly that object, not another tag of the same name and target.
if [ -n "$expected_tag_object" ]; then
    if ! printf '%s\n' "$expected_tag_object" | grep -Eq '^[0-9a-f]{40}$'; then
        echo "invalid expected tag object: $expected_tag_object" >&2
        exit 1
    fi
    if [ "$direct" != "$expected_tag_object" ]; then
        echo "remote release tag $tag_name is object $direct, not the expected tag object $expected_tag_object" >&2
        exit 1
    fi
fi

# TAG_SIGNING (default ssh, as for create-release-tag.sh) requires the remote
# ref to name an annotated tag object for this exact tag name; in ssh mode its
# signature must also verify against ALLOWED_SIGNERS_FILE. The only exception
# is a historical tag listed in LEGACY_RELEASE_TAGS_FILE at exactly the commit
# the remote tag resolves to; it is accepted as it is, lightweight or unsigned.
signing=${TAG_SIGNING:-ssh}
case "$signing" in
    ssh|none) ;;
    *)
        echo "unknown TAG_SIGNING value '$signing' (expected ssh or none)" >&2
        exit 1
        ;;
esac
legacy_tags=${LEGACY_RELEASE_TAGS_FILE:-$script_dir/../release/legacy-release-tags.txt}
if [ ! -f "$legacy_tags" ]; then
    echo "legacy release tag list not found: $legacy_tags" >&2
    exit 1
fi
legacy_count=$(awk -v tag="$tag_name" '$1 == tag { n++ } END { print n + 0 }' "$legacy_tags")
legacy_sha=$(awk -v tag="$tag_name" '$1 == tag { print $2; exit }' "$legacy_tags")
if [ "$legacy_count" -gt 1 ]; then
    echo "legacy release tag $tag_name is listed more than once in $legacy_tags" >&2
    exit 1
fi
if [ "$legacy_count" -eq 1 ] && [ "$legacy_sha" != "$actual_sha" ]; then
    echo "legacy release tag $tag_name is listed at $legacy_sha, but the remote tag resolves to $actual_sha" >&2
    exit 1
fi
if [ "$legacy_count" -eq 1 ]; then
    echo "Remote release tag $tag_name is a listed legacy tag at $legacy_sha; not requiring a signed tag object"
else
    if [ -z "$peeled" ] || [ "$direct" = "$peeled" ]; then
        echo "remote release tag $tag_name names a commit, not an annotated tag object" >&2
        exit 1
    fi
    # Inspect the exact object the remote advertised, not a local same-named tag.
    git fetch --quiet --no-tags "$remote" "refs/tags/$tag_name"
    embedded_name=$(git cat-file tag "$direct" | awk '/^tag / { print $2; exit } /^$/ { exit }')
    if [ "$embedded_name" != "$tag_name" ]; then
        echo "remote release tag $tag_name names a tag object for '$embedded_name'" >&2
        exit 1
    fi
    if [ "$signing" = ssh ]; then
        allowed_signers=${ALLOWED_SIGNERS_FILE:-$script_dir/../.forgejo/allowed_signers}
        if [ ! -f "$allowed_signers" ]; then
            echo "allowed signers file not found: $allowed_signers" >&2
            exit 1
        fi
        if ! git -c gpg.format=ssh -c gpg.ssh.allowedSignersFile="$allowed_signers" \
            verify-tag "$direct"; then
            echo "remote release tag $tag_name signature does not verify against $allowed_signers" >&2
            exit 1
        fi
    fi
fi

git check-ref-format --branch "$default_branch" >/dev/null
branch_ref="refs/heads/$default_branch"
branch_sha=$(git ls-remote --heads "$remote" "$branch_ref" |
    awk -v ref="$branch_ref" '$2 == ref { print $1 }')
if [ -z "$branch_sha" ]; then
    echo "remote default branch $default_branch does not exist" >&2
    exit 1
fi
git fetch --quiet --no-tags "$remote" "$branch_ref"
if ! git merge-base --is-ancestor "$expected_sha" FETCH_HEAD; then
    echo "release commit $expected_sha is not contained in remote $default_branch" >&2
    exit 1
fi

if [ -n "${GITHUB_OUTPUT:-}" ]; then
    echo "release_sha=$actual_sha" >> "$GITHUB_OUTPUT"
fi
echo "Remote release tag $tag_name is bound to $actual_sha"
