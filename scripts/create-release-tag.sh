#!/bin/sh
# Create the annotated release tag TAG at COMMIT, with message "pali TAG".
#
# TAG_SIGNING selects the signature:
#   ssh (default)  sign with the OpenSSH private key in TAG_SIGNING_KEY and
#                  verify the result against ALLOWED_SIGNERS_FILE before
#                  returning, so a tag that can never verify is not left
#                  behind to be pushed;
#   none           annotated and unsigned, with a warning.
# Any other value is an error. There is no fallback from ssh to unsigned: a
# missing or unusable key fails.
set -eu

usage='usage: create-release-tag.sh TAG COMMIT'
tag_name=${1:?$usage}
commit=${2:?$usage}
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
GITHUB_OUTPUT='' "$script_dir/validate-release-tag.sh" "$tag_name"

release_commit=$(git rev-parse --verify --quiet "${commit}^{commit}") || {
    echo "release tag target is not a commit: $commit" >&2
    exit 1
}

signing=${TAG_SIGNING:-ssh}
case "$signing" in
    ssh|none) ;;
    *)
        echo "unknown TAG_SIGNING value '$signing' (expected ssh or none)" >&2
        exit 1
        ;;
esac

message="pali $tag_name"
if [ "$signing" = none ]; then
    echo "warning: TAG_SIGNING=none: creating annotated, unsigned tag $tag_name" >&2
    git tag --annotate --no-sign --message "$message" "$tag_name" "$release_commit"
else
    if [ -z "${TAG_SIGNING_KEY:-}" ]; then
        echo "TAG_SIGNING=ssh but TAG_SIGNING_KEY is empty; refusing to create an unsigned tag" >&2
        exit 1
    fi
    allowed_signers=${ALLOWED_SIGNERS_FILE:-$script_dir/../.forgejo/allowed_signers}
    if [ ! -f "$allowed_signers" ]; then
        echo "allowed signers file not found: $allowed_signers" >&2
        exit 1
    fi
    key_dir=$(mktemp -d)
    trap 'rm -rf "$key_dir"' EXIT
    trap 'exit 1' HUP INT TERM
    key_file="$key_dir/tag-signing-key"
    (
        umask 077
        printf '%s\n' "$TAG_SIGNING_KEY" > "$key_file"
    )
    chmod 600 "$key_file"
    if ! ssh-keygen -y -P '' -f "$key_file" >/dev/null 2>&1; then
        echo "TAG_SIGNING_KEY is not a usable passphrase-less OpenSSH private key" >&2
        exit 1
    fi
    git -c gpg.format=ssh -c user.signingkey="$key_file" \
        tag --sign --message "$message" "$tag_name" "$release_commit"
    if ! git -c gpg.format=ssh -c gpg.ssh.allowedSignersFile="$allowed_signers" \
        verify-tag "refs/tags/$tag_name"; then
        git tag --delete "$tag_name" >/dev/null
        echo "signed tag $tag_name does not verify against $allowed_signers; is the signing key's public half listed there?" >&2
        exit 1
    fi
fi

echo "Created annotated tag $tag_name (signing: $signing) at $release_commit"
