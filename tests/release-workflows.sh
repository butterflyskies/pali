#!/bin/sh
# Behavioral and wiring tests for the split Forgejo/GitHub release path.
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM
# The signing mode is chosen per case below; never inherit the caller's.
unset TAG_SIGNING TAG_SIGNING_KEY ALLOWED_SIGNERS_FILE
# A Forgejo runner sets FORGEJO_OUTPUT, which the scripts prefer over the
# GITHUB_OUTPUT each case passes; clear both so every case reads its own file.
unset FORGEJO_OUTPUT GITHUB_OUTPUT

fail() {
    echo "release workflow test failed: $*" >&2
    exit 1
}

assert_contains() {
    file=$1
    text=$2
    grep -F -- "$text" "$file" >/dev/null || fail "$file lacks: $text"
}

# Requires a whole line, leading indentation aside, to equal the text, so
# neither a commented-out key nor a trailing comment can satisfy it.
assert_active() {
    file=$1
    text=$2
    sed 's/^[[:space:]]*//' "$file" | grep -Fx -- "$text" >/dev/null || fail "$file lacks active line: $text"
}

assert_output() {
    file=$1
    text=$2
    grep -Fx "$text" "$file" >/dev/null || fail "$file lacks output: $text"
}

# The executable tag oracle must distinguish an ordinary main push from a
# package-version change. Its local boundary tag is annotated and unsigned;
# the tag-release job creates and signs the tag it pushes.
tag_repo="$tmp_dir/tag-repo"
mkdir "$tag_repo"
git -C "$tag_repo" init -q
git -C "$tag_repo" config user.name test
git -C "$tag_repo" config user.email test@example.invalid
git -C "$tag_repo" config commit.gpgsign false
git -C "$tag_repo" config tag.gpgSign false
printf '[package]\nname = "pali"\nversion = "0.1.0"\n' > "$tag_repo/Cargo.toml"
printf '## [Unreleased]\n\n## [0.1.0] - 2026-01-01\n' > "$tag_repo/CHANGELOG.md"
git -C "$tag_repo" add Cargo.toml CHANGELOG.md
git -C "$tag_repo" commit -qm initial
git -C "$tag_repo" tag v0.1.0

before=$(git -C "$tag_repo" rev-parse HEAD)
printf '\n# unrelated\n' >> "$tag_repo/Cargo.toml"
git -C "$tag_repo" commit -qam unrelated
output="$tmp_dir/unchanged.out"
(
    cd "$tag_repo"
    GITHUB_OUTPUT="$output" "$repo_root/scripts/tag-release-on-version-change.sh" "$before"
)
assert_output "$output" 'release=false'
[ "$(git -C "$tag_repo" rev-parse refs/tags/v0.1.0)" = "$before" ] || \
    fail 'unchanged version moved its existing tag'

before=$(git -C "$tag_repo" rev-parse HEAD)
sed -i 's/version = "0.1.0"/version = "0.2.0"/' "$tag_repo/Cargo.toml"
git -C "$tag_repo" commit -qam release-0.2.0
if (
    cd "$tag_repo"
    GITHUB_OUTPUT="$tmp_dir/missing-changelog.out" "$repo_root/scripts/tag-release-on-version-change.sh" "$before"
); then
    fail 'version bump without a changelog update was accepted'
fi
printf '\n## [0.2.0] - 2026-01-02\n' >> "$tag_repo/CHANGELOG.md"
git -C "$tag_repo" add CHANGELOG.md
git -C "$tag_repo" commit --amend --no-edit -q
output="$tmp_dir/changed.out"
(
    cd "$tag_repo"
    GITHUB_OUTPUT="$output" "$repo_root/scripts/tag-release-on-version-change.sh" "$before"
)
assert_output "$output" 'release=true'
assert_output "$output" 'tag_name=v0.2.0'
assert_output "$output" "release_commit=$(git -C "$tag_repo" rev-parse HEAD)"
[ "$(git -C "$tag_repo" cat-file -t refs/tags/v0.2.0)" = tag ] || fail 'release tag is not an annotated tag object'
git -C "$tag_repo" cat-file tag refs/tags/v0.2.0 | grep -Fx 'pali v0.2.0' >/dev/null || \
    fail 'release tag message is not "pali v0.2.0"'
if git -C "$tag_repo" cat-file tag refs/tags/v0.2.0 | grep -F 'BEGIN SSH SIGNATURE' >/dev/null; then
    fail 'local release-boundary tag was signed'
fi
[ "$(git -C "$tag_repo" rev-parse 'refs/tags/v0.2.0^{commit}')" = "$(git -C "$tag_repo" rev-parse HEAD)" ] || fail 'release tag does not point to the version-bump commit'

before=$(git -C "$tag_repo" rev-parse HEAD)
sed -i 's/version = "0.2.0"/version = "0.3.0"/' "$tag_repo/Cargo.toml"
printf '\n## [0.3.0] - 2026-01-03\n' >> "$tag_repo/CHANGELOG.md"
git -C "$tag_repo" commit -qam release-0.3.0
git -C "$tag_repo" tag --annotate --message 'existing release tag' v0.3.0 HEAD
existing=$(git -C "$tag_repo" rev-parse refs/tags/v0.3.0)
output="$tmp_dir/existing.out"
(
    cd "$tag_repo"
    GITHUB_OUTPUT="$output" "$repo_root/scripts/tag-release-on-version-change.sh" "$before"
)
assert_output "$output" 'release=false'
[ "$(git -C "$tag_repo" rev-parse refs/tags/v0.3.0)" = "$existing" ] || fail 'existing tag was moved'
[ "$(git -C "$tag_repo" cat-file -t refs/tags/v0.3.0)" = tag ] || fail 'existing annotated tag was replaced'

# A tag at a later same-version commit is not the exact release boundary.
later_tag_repo="$tmp_dir/later-tag-repo"
git clone -q "$tag_repo" "$later_tag_repo"
git -C "$later_tag_repo" config user.name test
git -C "$later_tag_repo" config user.email test@example.invalid
git -C "$later_tag_repo" config commit.gpgsign false
git -C "$later_tag_repo" config tag.gpgSign false
git -C "$later_tag_repo" tag -d v0.3.0 >/dev/null
printf 'post-release code\n' > "$later_tag_repo/later.txt"
git -C "$later_tag_repo" add later.txt
git -C "$later_tag_repo" commit -qm later-same-version
git -C "$later_tag_repo" tag v0.3.0 HEAD
if (
    cd "$later_tag_repo"
    GITHUB_OUTPUT="$tmp_dir/later-tag.out" "$repo_root/scripts/tag-release-on-version-change.sh" "$before"
); then
    fail 'later same-version tag was accepted as the exact release boundary'
fi

before=$(git -C "$tag_repo" rev-parse HEAD)
git -C "$tag_repo" tag v0.4.0 "$before"
sed -i 's/version = "0.3.0"/version = "0.4.0"/' "$tag_repo/Cargo.toml"
printf '\n## [0.4.0] - 2026-01-04\n' >> "$tag_repo/CHANGELOG.md"
git -C "$tag_repo" commit -qam release-0.4.0
if (
    cd "$tag_repo"
    GITHUB_OUTPUT="$tmp_dir/mismatch.out" "$repo_root/scripts/tag-release-on-version-change.sh" "$before"
); then
    fail 'existing tag at the wrong commit was accepted'
fi
[ "$(git -C "$tag_repo" rev-parse refs/tags/v0.4.0)" = "$before" ] || fail 'mismatched existing tag was moved'

before=$(git -C "$tag_repo" rev-parse HEAD)
sed -i 's/version = "0.4.0"/version = "0.5.0"/' "$tag_repo/Cargo.toml"
printf '\n## [0.6.0] - 2026-01-05\n' >> "$tag_repo/CHANGELOG.md"
git -C "$tag_repo" commit -qam release-with-wrong-changelog
if (
    cd "$tag_repo"
    GITHUB_OUTPUT="$tmp_dir/mismatched-changelog.out" "$repo_root/scripts/tag-release-on-version-change.sh" "$before"
); then
    fail 'version bump with only a mismatched changelog entry was accepted'
fi

before=$(git -C "$tag_repo" rev-parse HEAD)
sed -i 's/version = "0.5.0"/version = "1.2.3-alpha.1"/' "$tag_repo/Cargo.toml"
printf '\n## [1.2.3-alpha.1] - 2026-01-06\n' >> "$tag_repo/CHANGELOG.md"
git -C "$tag_repo" commit -qam release-prerelease-semver
output="$tmp_dir/prerelease-semver.out"
(
    cd "$tag_repo"
    GITHUB_OUTPUT="$output" "$repo_root/scripts/tag-release-on-version-change.sh" "$before"
    "$repo_root/scripts/verify-release-tag.sh" v1.2.3-alpha.1 >/dev/null
)
assert_output "$output" 'tag_name=v1.2.3-alpha.1'
[ "$(git -C "$tag_repo" cat-file -t refs/tags/v1.2.3-alpha.1)" = tag ] || \
    fail 'prerelease tag is not an annotated tag object'

for invalid in v01.2.3 v1.02.3 v1.2.03 v1.2.3-alpha..1 v1.2.3-alpha. v1.2.3+build.7 v1.2.3-RC; do
    if "$repo_root/scripts/validate-release-tag.sh" "$invalid"; then
        fail "invalid or GHCR-ambiguous release tag was accepted: $invalid"
    fi
done
"$repo_root/scripts/validate-release-tag.sh" v1.2.3-alpha-01
# A tag is one line. A workflow input carrying a valid tag on one of several
# lines must not validate: the raw input names concurrency groups and step
# outputs before anything else reads it.
newline='
'
carriage_return=$(printf '\r')
for invalid in "v0.20.0${newline}tag_name=v0.20.0" "x${newline}v0.20.0" "v0.20.0${newline}" \
    "v0.20.0${carriage_return}"; do
    multiline_output="$tmp_dir/multiline.out"
    : > "$multiline_output"
    if GITHUB_OUTPUT="$multiline_output" "$repo_root/scripts/validate-release-tag.sh" "$invalid" 2>/dev/null; then
        fail 'a release tag containing a line break was accepted'
    fi
    [ ! -s "$multiline_output" ] || fail 'a rejected release tag wrote step output'
done

# Release order follows SemVer, including numeric prerelease identifiers and
# the rule that a stable version follows every prerelease of the same core.
for transition in \
    '1.0.0-alpha.1 1.0.0-alpha' \
    '1.0.0-alpha.2 1.0.0-alpha.1' \
    '1.0.0-beta 1.0.0-alpha.9' \
    '1.0.0 1.0.0-rc.1' \
    '1.1.0-alpha 1.0.0'; do
    set -- $transition
    "$repo_root/scripts/release-version-is-greater.sh" "$1" "$2" || \
        fail "increasing SemVer transition was rejected: $2 -> $1"
done

comparison_output="$tmp_dir/semver-comparison.out"
: > "$comparison_output"
GITHUB_OUTPUT="$comparison_output" \
    "$repo_root/scripts/release-version-is-greater.sh" 1.1.0-alpha.1 1.0.0
[ ! -s "$comparison_output" ] || fail 'internal SemVer validation polluted workflow outputs'
for transition in \
    '1.0.0-alpha 1.0.0-alpha' \
    '1.0.0-alpha.1 1.0.0-alpha.2' \
    '1.0.0-rc.1 1.0.0' \
    '0.19.0 0.20.0'; do
    set -- $transition
    if "$repo_root/scripts/release-version-is-greater.sh" "$1" "$2"; then
        fail "non-increasing SemVer transition was accepted: $2 -> $1"
    fi
done

downgrade_repo="$tmp_dir/downgrade-repo"
mkdir "$downgrade_repo"
git -C "$downgrade_repo" init -q
git -C "$downgrade_repo" config user.name test
git -C "$downgrade_repo" config user.email test@example.invalid
git -C "$downgrade_repo" config commit.gpgsign false
git -C "$downgrade_repo" config tag.gpgSign false
printf '[package]\nname = "pali"\nversion = "0.20.0"\n' > "$downgrade_repo/Cargo.toml"
printf '## [0.20.0] - 2026-01-01\n' > "$downgrade_repo/CHANGELOG.md"
git -C "$downgrade_repo" add .
git -C "$downgrade_repo" commit -qm initial
downgrade_before=$(git -C "$downgrade_repo" rev-parse HEAD)
sed -i 's/0.20.0/0.19.0/' "$downgrade_repo/Cargo.toml"
printf '\n## [0.19.0] - 2026-01-02\n' >> "$downgrade_repo/CHANGELOG.md"
git -C "$downgrade_repo" commit -qam downgrade
if (
    cd "$downgrade_repo"
    GITHUB_OUTPUT="$tmp_dir/downgrade.out" \
        "$repo_root/scripts/tag-release-on-version-change.sh" "$downgrade_before"
); then
    fail 'release workflow accepted a package-version downgrade'
fi

# These cases test commit and branch binding; the tag is the annotated unsigned
# boundary tag, so they check with TAG_SIGNING=none. Signing is tested below.
tag_remote="$tmp_dir/tag-remote.git"
git init -q --bare "$tag_remote"
git -C "$tag_repo" remote add test-origin "$tag_remote"
git -C "$tag_repo" push -q test-origin refs/tags/v1.2.3-alpha.1
git -C "$tag_repo" push -q test-origin HEAD:refs/heads/main
expected_release_sha=$(git -C "$tag_repo" rev-parse HEAD)
(
    cd "$tag_repo"
    TAG_SIGNING=none "$repo_root/scripts/verify-remote-release-tag.sh" test-origin \
        v1.2.3-alpha.1 "$expected_release_sha" main >/dev/null
)
# A same-named branch must not influence exact tag resolution.
git -C "$tag_repo" push -q test-origin HEAD^:refs/heads/v1.2.3-alpha.1
(
    cd "$tag_repo"
    TAG_SIGNING=none "$repo_root/scripts/verify-remote-release-tag.sh" test-origin \
        v1.2.3-alpha.1 "$expected_release_sha" main >/dev/null
)
git -C "$tag_repo" push -q --force test-origin HEAD^:refs/heads/main
if (
    cd "$tag_repo"
    TAG_SIGNING=none "$repo_root/scripts/verify-remote-release-tag.sh" test-origin \
        v1.2.3-alpha.1 "$expected_release_sha" main
); then
    fail 'release commit outside remote main was accepted'
fi
git -C "$tag_repo" push -q --force test-origin HEAD:refs/heads/main
git -C "$tag_repo" push -q --delete test-origin refs/tags/v1.2.3-alpha.1
if (
    cd "$tag_repo"
    TAG_SIGNING=none "$repo_root/scripts/verify-remote-release-tag.sh" test-origin \
        v1.2.3-alpha.1 "$expected_release_sha" main
); then
    fail 'deleted remote release tag was accepted'
fi
git -C "$tag_repo" push -q test-origin refs/tags/v1.2.3-alpha.1
git -C "$tag_repo" push -q --force test-origin HEAD^:refs/tags/v1.2.3-alpha.1
if (
    cd "$tag_repo"
    TAG_SIGNING=none "$repo_root/scripts/verify-remote-release-tag.sh" test-origin \
        v1.2.3-alpha.1 "$expected_release_sha" main
); then
    fail 'retargeted remote release tag was accepted'
fi

# Signed release tags. create-release-tag.sh makes the tag the tag-release job
# pushes; verify-remote-release-tag.sh reads it back (ssh unless TAG_SIGNING
# says otherwise). The keys are throwaway; only release_key is listed in the
# allowed signers.
command -v ssh-keygen >/dev/null || fail 'ssh-keygen is required to test signed release tags'
ssh-keygen -q -t ed25519 -N '' -C release -f "$tmp_dir/release_key"
ssh-keygen -q -t ed25519 -N '' -C rogue -f "$tmp_dir/rogue_key"
allowed_signers="$tmp_dir/allowed_signers"
printf '# test signers\npali-release@lacuna.local namespaces="git" %s\n' \
    "$(cat "$tmp_dir/release_key.pub")" > "$allowed_signers"
release_key=$(cat "$tmp_dir/release_key")
rogue_key=$(cat "$tmp_dir/rogue_key")

sign_repo="$tmp_dir/sign-repo"
mkdir "$sign_repo"
git -C "$sign_repo" init -q
git -C "$sign_repo" config user.name pali-release
git -C "$sign_repo" config user.email pali-release@lacuna.local
git -C "$sign_repo" config commit.gpgsign false
git -C "$sign_repo" config tag.gpgSign false
git -C "$sign_repo" commit -q --allow-empty -m release
sign_commit=$(git -C "$sign_repo" rev-parse HEAD)
# Test legacy list: v5.1.3 is listed at the release commit, v5.1.4 at another
# commit, and v5.1.5 twice.
legacy_tags="$tmp_dir/legacy_release_tags"
printf '# test legacy tags\nv5.1.3 %s\nv5.1.4 %s\nv5.1.5 %s\nv5.1.5 %s\n' \
    "$sign_commit" 1111111111111111111111111111111111111111 "$sign_commit" "$sign_commit" \
    > "$legacy_tags"

# create_tag SIGNING KEY TAG [TARGET [ALLOWED_SIGNERS]]: SIGNING "unset"
# exercises the default.
create_tag() {
    (
        cd "$sign_repo"
        [ "$1" = unset ] || export TAG_SIGNING="$1"
        TAG_SIGNING_KEY="$2" ALLOWED_SIGNERS_FILE="${5:-$allowed_signers}" \
            "$repo_root/scripts/create-release-tag.sh" "$3" "${4:-HEAD}"
    )
}

# remote_check SIGNING TAG [ALLOWED_SIGNERS [LEGACY_TAGS [TAG_OBJECT]]]: SIGNING "unset"
# exercises the default.
remote_check() {
    (
        cd "$sign_repo"
        [ "$1" = unset ] || export TAG_SIGNING="$1"
        ALLOWED_SIGNERS_FILE="${3:-$allowed_signers}" LEGACY_RELEASE_TAGS_FILE="${4:-$legacy_tags}" \
            "$repo_root/scripts/verify-remote-release-tag.sh" sign-origin "$2" "$sign_commit" main ${5:+"$5"}
    )
}

refute_tag() {
    if git -C "$sign_repo" show-ref --verify --quiet "refs/tags/$1"; then
        fail "$2"
    fi
}

# refuse WHAT EXPECTED COMMAND...: COMMAND must fail and report EXPECTED, so
# that each check is shown to be the one refusing, not a later fallback.
refuse() {
    what=$1
    expected=$2
    shift 2
    if "$@" >/dev/null 2>"$tmp_dir/refusal.err"; then
        fail "$what was accepted"
    fi
    if ! grep -F -- "$expected" "$tmp_dir/refusal.err" >/dev/null; then
        cat "$tmp_dir/refusal.err" >&2
        fail "$what was refused without reporting: $expected"
    fi
}

# TAG_SIGNING defaults to ssh: an annotated tag, signed by the listed key.
create_tag unset "$release_key" v5.0.0 >/dev/null
[ "$(git -C "$sign_repo" cat-file -t refs/tags/v5.0.0)" = tag ] || fail 'signed release tag is not annotated'
git -C "$sign_repo" cat-file tag refs/tags/v5.0.0 | grep -Fx 'pali v5.0.0' >/dev/null || \
    fail 'signed release tag message is not "pali v5.0.0"'
git -C "$sign_repo" cat-file tag refs/tags/v5.0.0 | grep -F 'BEGIN SSH SIGNATURE' >/dev/null || \
    fail 'default TAG_SIGNING did not SSH-sign the release tag'
git -C "$sign_repo" -c gpg.ssh.allowedSignersFile="$allowed_signers" verify-tag v5.0.0 2>/dev/null || \
    fail 'signed release tag does not verify against the allowed signers'

create_tag none '' v5.0.2 >/dev/null 2>"$tmp_dir/unsigned-tag.err"
[ "$(git -C "$sign_repo" cat-file -t refs/tags/v5.0.2)" = tag ] || fail 'TAG_SIGNING=none tag is not annotated'
if git -C "$sign_repo" cat-file tag refs/tags/v5.0.2 | grep -F 'BEGIN SSH SIGNATURE' >/dev/null; then
    fail 'TAG_SIGNING=none signed the release tag'
fi
grep -F 'warning: TAG_SIGNING=none' "$tmp_dir/unsigned-tag.err" >/dev/null || \
    fail 'TAG_SIGNING=none created an unsigned tag without a warning'

refuse 'a tag signed by a key missing from the allowed signers' 'does not verify against' \
    create_tag ssh "$rogue_key" v5.0.1
refute_tag v5.0.1 'an unverifiable signed tag was left behind to be pushed'
refuse 'an unknown TAG_SIGNING value' "unknown TAG_SIGNING value 'gpg'" \
    create_tag gpg "$release_key" v5.0.3
refute_tag v5.0.3 'an unknown TAG_SIGNING value still created a tag'
refuse 'TAG_SIGNING=ssh without a key' 'TAG_SIGNING_KEY is empty' \
    create_tag ssh '' v5.0.4
refute_tag v5.0.4 'TAG_SIGNING=ssh without a key fell back to another tag'
refuse 'an unusable signing key' 'not a usable passphrase-less OpenSSH private key' \
    create_tag ssh 'not a key' v5.0.5
refute_tag v5.0.5 'an unusable signing key still created a tag'
refuse 'signing without an allowed signers file' 'allowed signers file not found' \
    create_tag ssh "$release_key" v5.0.6 HEAD "$tmp_dir/missing_signers"
refute_tag v5.0.6 'signing without an allowed signers file still created a tag'
refuse 'a non-commit release tag target' 'release tag target is not a commit' \
    create_tag none '' v5.0.7 'HEAD^{tree}'
refute_tag v5.0.7 'a non-commit release tag target was tagged'
refuse 'an invalid release tag name' 'invalid release tag' \
    create_tag none '' v5.00.8
refute_tag v5.00.8 'an invalid release tag name was tagged'

# Remote readback. v5.1.0 is lightweight, v5.1.1 is signed by an unlisted key,
# v5.1.2 is the signed v5.0.0 object published under another tag name, and
# v5.1.3 to v5.1.5 are lightweight tags named in the test legacy list.
for lightweight in v5.1.0 v5.1.3 v5.1.4 v5.1.5; do
    git -C "$sign_repo" tag "$lightweight" HEAD
done
git -C "$sign_repo" -c gpg.format=ssh -c user.signingkey="$tmp_dir/rogue_key" \
    tag --sign --message 'pali v5.1.1' v5.1.1 HEAD
sign_remote="$tmp_dir/sign-remote.git"
git init -q --bare "$sign_remote"
git -C "$sign_repo" remote add sign-origin "$sign_remote"
git -C "$sign_repo" push -q sign-origin HEAD:refs/heads/main \
    refs/tags/v5.0.0 refs/tags/v5.0.2 refs/tags/v5.1.0 refs/tags/v5.1.1 \
    refs/tags/v5.0.0:refs/tags/v5.1.2 refs/tags/v5.1.3 refs/tags/v5.1.4 refs/tags/v5.1.5

remote_check ssh v5.0.0 >/dev/null 2>&1 || fail 'remote signed release tag was refused'
remote_check none v5.0.2 >/dev/null || fail 'remote annotated unsigned tag was refused with TAG_SIGNING=none'
remote_check unset v5.0.0 >/dev/null 2>&1 || fail 'remote signed release tag was refused with TAG_SIGNING unset'
for signing in unset ssh none; do
    refuse "a remote lightweight tag with TAG_SIGNING=$signing" 'names a commit, not an annotated tag object' \
        remote_check "$signing" v5.1.0
done
refuse 'a remote tag signed by a key missing from the allowed signers' 'signature does not verify' \
    remote_check ssh v5.1.1
refuse 'a remote unsigned tag with TAG_SIGNING=ssh' 'signature does not verify' \
    remote_check ssh v5.0.2
refuse 'a remote unsigned tag with TAG_SIGNING unset' 'signature does not verify' \
    remote_check unset v5.0.2
refuse 'a remote tag object for another tag name' "names a tag object for 'v5.0.0'" \
    remote_check ssh v5.1.2
refuse 'a remote check with an unknown TAG_SIGNING value' "unknown TAG_SIGNING value 'gpg'" \
    remote_check gpg v5.0.0
refuse 'a remote check without an allowed signers file' 'allowed signers file not found' \
    remote_check ssh v5.0.0 "$tmp_dir/missing_signers"
remote_check ssh v5.1.3 >/dev/null || fail 'a listed legacy tag at its exact commit was refused'
refuse 'a listed legacy tag at a different commit' 'is listed at 1111111111111111111111111111111111111111' \
    remote_check ssh v5.1.4
refuse 'a legacy tag listed more than once' 'is listed more than once' \
    remote_check ssh v5.1.5
refuse 'a remote check without a legacy tag list' 'legacy release tag list not found' \
    remote_check ssh v5.0.0 "$allowed_signers" "$tmp_dir/missing_legacy"

# A second validly signed tag object with the same name and target must not
# pass a read-back that names the object just created.
create_tag ssh "$release_key" v5.2.0 >/dev/null 2>&1
first_tag_object=$(git -C "$sign_repo" rev-parse refs/tags/v5.2.0)
git -C "$sign_repo" -c gpg.format=ssh -c user.signingkey="$tmp_dir/release_key" \
    tag --force --sign --message 'pali v5.2.0 (signed again)' v5.2.0 HEAD >/dev/null
second_tag_object=$(git -C "$sign_repo" rev-parse refs/tags/v5.2.0)
[ "$first_tag_object" != "$second_tag_object" ] || fail 'test setup made one tag object twice'
git -C "$sign_repo" push -q sign-origin refs/tags/v5.2.0
remote_check ssh v5.2.0 '' '' "$second_tag_object" >/dev/null 2>&1 || \
    fail 'remote tag object matching the expected object was refused'
refuse 'another tag object with the same name and target' 'not the expected tag object' \
    remote_check ssh v5.2.0 '' '' "$first_tag_object"
refuse 'a malformed expected tag object' 'invalid expected tag object' \
    remote_check ssh v5.2.0 '' '' not-an-object-id

reconcile_repo="$tmp_dir/reconcile-repo"
mkdir "$reconcile_repo"
git -C "$reconcile_repo" init -q
git -C "$reconcile_repo" config user.name test
git -C "$reconcile_repo" config user.email test@example.invalid
git -C "$reconcile_repo" config commit.gpgsign false
git -C "$reconcile_repo" config tag.gpgSign false
printf '[package]\nname = "pali"\nversion = "1.0.0"\n' > "$reconcile_repo/Cargo.toml"
printf '## [1.0.0] - 2026-01-01\n' > "$reconcile_repo/CHANGELOG.md"
git -C "$reconcile_repo" add .
git -C "$reconcile_repo" commit -qm initial
git -C "$reconcile_repo" tag v1.0.0
sed -i 's/1.0.0/1.1.0/' "$reconcile_repo/Cargo.toml"
printf '\n## [1.1.0] - 2026-01-02\n' >> "$reconcile_repo/CHANGELOG.md"
git -C "$reconcile_repo" commit -qam version-bump-whose-tag-push-failed
failed_bump=$(git -C "$reconcile_repo" rev-parse HEAD)
printf 'later green main\n' > "$reconcile_repo/fix.txt"
git -C "$reconcile_repo" add fix.txt
git -C "$reconcile_repo" commit -qm fix-ci
(
    cd "$reconcile_repo"
    GITHUB_OUTPUT="$tmp_dir/reconciled.out" \
        "$repo_root/scripts/tag-release-on-version-change.sh" "$failed_bump"
)
assert_output "$tmp_dir/reconciled.out" 'release=true'
[ "$(git -C "$reconcile_repo" rev-parse 'refs/tags/v1.1.0^{commit}')" = "$failed_bump" ] || \
    fail 'missed release tag included an unrelated later fix commit'

split_repo="$tmp_dir/split-release-repo"
mkdir "$split_repo"
git -C "$split_repo" init -q
git -C "$split_repo" config user.name test
git -C "$split_repo" config user.email test@example.invalid
git -C "$split_repo" config commit.gpgsign false
git -C "$split_repo" config tag.gpgSign false
printf '[package]\nname = "pali"\nversion = "3.0.0"\n' > "$split_repo/Cargo.toml"
printf '## [3.0.0] - 2026-01-01\n' > "$split_repo/CHANGELOG.md"
git -C "$split_repo" add .
git -C "$split_repo" commit -qm initial
git -C "$split_repo" tag v3.0.0
split_before=$(git -C "$split_repo" rev-parse HEAD)
sed -i 's/3.0.0/3.1.0/' "$split_repo/Cargo.toml"
git -C "$split_repo" commit -qam version-first
git -C "$split_repo" tag v3.1.0 HEAD
if (
    cd "$split_repo"
    GITHUB_OUTPUT="$tmp_dir/pre-changelog-tag.out" \
        "$repo_root/scripts/tag-release-on-version-change.sh" "$split_before"
); then
    fail 'pre-changelog same-version tag was accepted as a release boundary'
fi
git -C "$split_repo" tag -d v3.1.0 >/dev/null
printf '\n## [3.1.0] - 2026-01-02\n' >> "$split_repo/CHANGELOG.md"
git -C "$split_repo" commit -qam changelog-second
split_boundary=$(git -C "$split_repo" rev-parse HEAD)
printf 'later green repair\n' > "$split_repo/fix.txt"
git -C "$split_repo" add fix.txt
git -C "$split_repo" commit -qm green-recovery
(
    cd "$split_repo"
    GITHUB_OUTPUT="$tmp_dir/split-release.out" \
        "$repo_root/scripts/tag-release-on-version-change.sh" "$split_boundary"
)
assert_output "$tmp_dir/split-release.out" 'release=true'
[ "$(git -C "$split_repo" rev-parse 'refs/tags/v3.1.0^{commit}')" = "$split_boundary" ] || \
    fail 'split version/changelog release did not tag its exact release boundary'

stale_repo="$tmp_dir/stale-changelog-repo"
mkdir "$stale_repo"
git -C "$stale_repo" init -q
git -C "$stale_repo" config user.name test
git -C "$stale_repo" config user.email test@example.invalid
git -C "$stale_repo" config commit.gpgsign false
git -C "$stale_repo" config tag.gpgSign false
printf '[package]\nname = "pali"\nversion = "2.0.0"\n' > "$stale_repo/Cargo.toml"
printf '## [2.1.0] - 2026-01-01\n' > "$stale_repo/CHANGELOG.md"
git -C "$stale_repo" add .
git -C "$stale_repo" commit -qm stale-heading
sed -i 's/2.0.0/2.1.0/' "$stale_repo/Cargo.toml"
git -C "$stale_repo" commit -qam version-only
stale_bump=$(git -C "$stale_repo" rev-parse HEAD)
printf '\nUnrelated changelog prose.\n' >> "$stale_repo/CHANGELOG.md"
git -C "$stale_repo" commit -qam unrelated-changelog-edit
if (
    cd "$stale_repo"
    GITHUB_OUTPUT="$tmp_dir/stale-heading.out" \
        "$repo_root/scripts/tag-release-on-version-change.sh" "$stale_bump"
); then
    fail 'stale changelog heading plus unrelated edit satisfied the release gate'
fi

# The repository checks run against the live Cargo.toml, so derive its tag
# rather than pinning a version that the next release bump makes stale.
current_tag=v$(awk '
    /^\[package\][[:space:]]*$/ { in_package = 1; next }
    /^\[/ { in_package = 0 }
    in_package && /^version[[:space:]]*=/ {
        line = $0
        sub(/^[^=]*=[[:space:]]*"/, "", line)
        sub(/".*/, "", line)
        print line
        exit
    }
' "$repo_root/Cargo.toml")
(
    cd "$repo_root"
    scripts/verify-release-tag.sh "$current_tag" >/dev/null
)
if (
    cd "$repo_root"
    scripts/verify-release-tag.sh v0.17.2 >/dev/null
); then
    fail 'release tag/package version mismatch was accepted'
fi

# Fake curl supplies exact HTTP or transport outcomes without relying on the
# network. Only 200 and 404 are admitted states; ambiguity must remain fatal.
fake_bin="$tmp_dir/bin"
mkdir "$fake_bin"
cat > "$fake_bin/curl" <<'EOF'
#!/bin/sh
output=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --output) output=$2; shift 2 ;;
        *) shift ;;
    esac
done
if [ "${FAKE_CURL_EXIT:-0}" -ne 0 ]; then
    exit "$FAKE_CURL_EXIT"
fi
if [ "${FAKE_BODY+x}" = x ]; then
    printf '%s\n' "$FAKE_BODY" > "$output"
else
    printf '{}\n' > "$output"
fi
printf '%s' "${FAKE_HTTP_STATUS:?}"
EOF
chmod +x "$fake_bin/curl"
cat > "$fake_bin/skopeo" <<'EOF'
#!/bin/sh
[ -z "${FAKE_SKOPEO_LOG:-}" ] || printf '%s\n' "$*" >> "$FAKE_SKOPEO_LOG"
printf '%s\n' "${FAKE_SOURCE:?}"
EOF
chmod +x "$fake_bin/skopeo"

crate_file="$tmp_dir/pali-0.18.0.crate"
printf 'intended crate bytes\n' > "$crate_file"
crate_digest=$(sha256sum "$crate_file" | awk '{ print $1 }')
(
    cd "$repo_root"
    PATH="$fake_bin:$PATH" FAKE_HTTP_STATUS=200 \
        FAKE_BODY="{\"version\":{\"checksum\":\"$crate_digest\"}}" \
        GITHUB_OUTPUT="$tmp_dir/crate-200.out" \
        scripts/crates-io-publish-status.sh "$current_tag" "$crate_file"
)
(
    cd "$repo_root"
    PATH="$fake_bin:$PATH" FAKE_HTTP_STATUS=404 GITHUB_OUTPUT="$tmp_dir/crate-404.out" \
        scripts/crates-io-publish-status.sh "$current_tag" "$crate_file"
)
assert_output "$tmp_dir/crate-200.out" 'already_published=true'
assert_output "$tmp_dir/crate-200.out" 'publish=false'
assert_output "$tmp_dir/crate-404.out" 'already_published=false'
assert_output "$tmp_dir/crate-404.out" 'publish=true'

if (
    cd "$repo_root"
    PATH="$fake_bin:$PATH" FAKE_HTTP_STATUS=500 GITHUB_OUTPUT="$tmp_dir/crate-500.out" \
        scripts/crates-io-publish-status.sh "$current_tag" "$crate_file"
); then
    fail 'crates.io HTTP 500 was treated as success'
fi
if (
    cd "$repo_root"
    PATH="$fake_bin:$PATH" FAKE_CURL_EXIT=7 FAKE_HTTP_STATUS=000 GITHUB_OUTPUT="$tmp_dir/crate-network.out" \
        scripts/crates-io-publish-status.sh "$current_tag" "$crate_file"
); then
    fail 'crates.io transport failure was treated as success'
fi

for case in missing malformed mismatch; do
    case "$case" in
        missing) body='{}' ;;
        malformed) body='{"version":{"checksum":"not-a-sha256"}}' ;;
        mismatch) body='{"version":{"checksum":"0000000000000000000000000000000000000000000000000000000000000000"}}' ;;
    esac
    if (
        cd "$repo_root"
        PATH="$fake_bin:$PATH" FAKE_HTTP_STATUS=200 FAKE_BODY="$body" \
            GITHUB_OUTPUT="$tmp_dir/crate-$case.out" \
            scripts/crates-io-publish-status.sh "$current_tag" "$crate_file"
    ); then
        fail "crates.io $case checksum was treated as an exact published crate"
    fi
done

manifest_body='{"schemaVersion":2,"manifests":[]}'
manifest_digest="sha256:$(printf '%s\n' "$manifest_body" | sha256sum | awk '{ print $1 }')"
(
    cd "$repo_root"
    PATH="$fake_bin:$PATH" GHCR_BEARER_TOKEN=test FAKE_HTTP_STATUS=200 \
        FAKE_BODY="$manifest_body" GITHUB_OUTPUT="$tmp_dir/image-exact.out" \
        scripts/ghcr-image-status.sh digest ghcr.io/butterflyskies/pali 0.18.0 "$manifest_digest"
)
assert_output "$tmp_dir/image-exact.out" 'publish=false'
(
    cd "$repo_root"
    PATH="$fake_bin:$PATH" GHCR_BEARER_TOKEN=test FAKE_HTTP_STATUS=404 \
        GITHUB_OUTPUT="$tmp_dir/image-absent.out" \
        scripts/ghcr-image-status.sh digest ghcr.io/butterflyskies/pali 0.18.0 "$manifest_digest"
)
assert_output "$tmp_dir/image-absent.out" 'publish=true'
if (
    cd "$repo_root"
    PATH="$fake_bin:$PATH" GHCR_BEARER_TOKEN=test FAKE_HTTP_STATUS=200 FAKE_BODY="$manifest_body" \
        scripts/ghcr-image-status.sh digest ghcr.io/butterflyskies/pali 0.18.0 \
        sha256:0000000000000000000000000000000000000000000000000000000000000000
); then
    fail 'existing mismatched immutable GHCR tag was overwritten'
fi
if (
    cd "$repo_root"
    PATH="$fake_bin:$PATH" GHCR_BEARER_TOKEN=test FAKE_HTTP_STATUS=401 \
        scripts/ghcr-image-status.sh digest ghcr.io/butterflyskies/pali 0.18.0 "$manifest_digest"
); then
    fail 'GHCR authorization failure was treated as tag absence'
fi
legacy_allowlist="$tmp_dir/legacy-images.txt"
printf 'v0.17.2 %s\n' "$manifest_digest" > "$legacy_allowlist"
(
    cd "$repo_root"
    PATH="$fake_bin:$PATH" GHCR_BEARER_TOKEN=test FAKE_HTTP_STATUS=200 \
        FAKE_BODY="$manifest_body" GITHUB_OUTPUT="$tmp_dir/manual-image.out" \
        scripts/verify-manual-release-image.sh ghcr.io/butterflyskies/pali \
            v0.17.2 1111111111111111111111111111111111111111 "$legacy_allowlist"
)
assert_output "$tmp_dir/manual-image.out" "image_digest=$manifest_digest"
empty_allowlist="$tmp_dir/no-legacy-images.txt"
: > "$empty_allowlist"
(
    cd "$repo_root"
    PATH="$fake_bin:$PATH" GHCR_BEARER_TOKEN=test FAKE_HTTP_STATUS=200 \
        FAKE_BODY="$manifest_body" FAKE_SOURCE=1111111111111111111111111111111111111111 \
        GITHUB_OUTPUT="$tmp_dir/manual-modern-image.out" \
        scripts/verify-manual-release-image.sh ghcr.io/butterflyskies/pali \
            v0.18.0 1111111111111111111111111111111111111111 "$empty_allowlist"
)
assert_output "$tmp_dir/manual-modern-image.out" "image_digest=$manifest_digest"
if (
    cd "$repo_root"
    PATH="$fake_bin:$PATH" GHCR_BEARER_TOKEN=test FAKE_HTTP_STATUS=404 \
        scripts/verify-manual-release-image.sh ghcr.io/butterflyskies/pali \
            v0.17.2 1111111111111111111111111111111111111111 "$legacy_allowlist"
); then
    fail 'manual repair accepted a missing immutable image tag'
fi
if (
    cd "$repo_root"
    PATH="$fake_bin:$PATH" GHCR_BEARER_TOKEN=test FAKE_HTTP_STATUS=404 \
        scripts/ghcr-image-status.sh require-digest ghcr.io/butterflyskies/pali \
            0.17.2 "$manifest_digest"
); then
    fail 'terminal exact-digest check accepted a missing immutable image tag'
fi
printf 'v0.17.2 %s\nv0.17.2 %s\n' "$manifest_digest" "$manifest_digest" > "$tmp_dir/duplicate-legacy.txt"
if PATH="$fake_bin:$PATH" GHCR_BEARER_TOKEN=test FAKE_HTTP_STATUS=200 FAKE_BODY="$manifest_body" \
    "$repo_root/scripts/verify-manual-release-image.sh" ghcr.io/butterflyskies/pali \
        v0.17.2 1111111111111111111111111111111111111111 "$tmp_dir/duplicate-legacy.txt"; then
    fail 'duplicate legacy image identities were accepted'
fi

source_sha=1111111111111111111111111111111111111111
(
    cd "$repo_root"
    PATH="$fake_bin:$PATH" GHCR_BEARER_TOKEN=test FAKE_HTTP_STATUS=200 \
        FAKE_BODY="$manifest_body" FAKE_SOURCE="$source_sha" \
        FAKE_SKOPEO_LOG="$tmp_dir/image-source-skopeo.log" GITHUB_OUTPUT="$tmp_dir/image-source.out" \
        scripts/ghcr-image-status.sh source ghcr.io/butterflyskies/pali 0.18.0 "$source_sha"
)
assert_output "$tmp_dir/image-source.out" 'exists=true'
assert_output "$tmp_dir/image-source.out" "image_digest=$manifest_digest"
assert_contains "$tmp_dir/image-source-skopeo.log" "docker://ghcr.io/butterflyskies/pali@$manifest_digest"
if (
    cd "$repo_root"
    PATH="$fake_bin:$PATH" GHCR_BEARER_TOKEN=test FAKE_HTTP_STATUS=200 \
        FAKE_BODY="$manifest_body" FAKE_SOURCE=2222222222222222222222222222222222222222 \
        scripts/ghcr-image-status.sh source ghcr.io/butterflyskies/pali 0.18.0 "$source_sha"
); then
    fail 'existing immutable image from the wrong source was accepted'
fi

for status in 200 404; do
    output="$tmp_dir/release-$status.out"
    (
        cd "$repo_root"
        PATH="$fake_bin:$PATH" FAKE_HTTP_STATUS=$status GH_TOKEN=test GITHUB_OUTPUT="$output" \
            scripts/github-release-status.sh butterflyskies/pali v0.18.0
    )
done
assert_output "$tmp_dir/release-200.out" 'exists=true'
assert_output "$tmp_dir/release-404.out" 'exists=false'
(
    cd "$repo_root"
    PATH="$fake_bin:$PATH" FAKE_HTTP_STATUS=404 GH_TOKEN=test GITHUB_OUTPUT="$tmp_dir/release-prerelease.out" \
        scripts/github-release-status.sh butterflyskies/pali v1.2.3-alpha.1
)
assert_output "$tmp_dir/release-prerelease.out" 'exists=false'
if (
    cd "$repo_root"
    PATH="$fake_bin:$PATH" FAKE_HTTP_STATUS=403 GH_TOKEN=test GITHUB_OUTPUT="$tmp_dir/release-403.out" \
        scripts/github-release-status.sh butterflyskies/pali v0.18.0
); then
    fail 'GitHub authorization failure was treated as release absence'
fi

cat > "$fake_bin/gh" <<'EOF'
#!/bin/sh
case "$1 $2" in
    'release create')
        exit "${FAKE_GH_CREATE_EXIT:-0}"
        ;;
    'release view')
        printf '%s\n' "${FAKE_ASSETS:-}"
        ;;
    'release download')
        shift 3
        patterns=
        destination=
        while [ "$#" -gt 0 ]; do
            case "$1" in
                --pattern) patterns="$patterns $2"; shift 2 ;;
                --dir) destination=$2; shift 2 ;;
                *) shift ;;
            esac
        done
        for pattern in $patterns; do
            cp "${FAKE_REMOTE_DIR:?}/$pattern" "$destination/$pattern"
        done
        ;;
    'release upload')
        shift 3
        printf 'upload' >> "${FAKE_GH_LOG:?}"
        while [ "$#" -gt 0 ] && [ "$1" != --repo ]; do
            printf ' %s' "$1" >> "$FAKE_GH_LOG"
            shift
        done
        printf '\n' >> "$FAKE_GH_LOG"
        ;;
    *) exit 90 ;;
esac
EOF
chmod +x "$fake_bin/gh"

# A concurrent exact-tag release creation is idempotent, while absence or an
# ambiguous lookup after create failure remains fatal.
PATH="$fake_bin:$PATH" FAKE_GH_CREATE_EXIT=0 GH_TOKEN=test \
    "$repo_root/scripts/create-github-draft-release.sh" butterflyskies/pali v0.18.0 false
PATH="$fake_bin:$PATH" FAKE_GH_CREATE_EXIT=1 FAKE_HTTP_STATUS=200 GH_TOKEN=test \
    "$repo_root/scripts/create-github-draft-release.sh" butterflyskies/pali v0.18.0 false
if PATH="$fake_bin:$PATH" FAKE_GH_CREATE_EXIT=1 FAKE_HTTP_STATUS=404 GH_TOKEN=test \
    "$repo_root/scripts/create-github-draft-release.sh" butterflyskies/pali v0.18.0 false; then
    fail 'failed release creation with confirmed absence was treated as success'
fi
if PATH="$fake_bin:$PATH" FAKE_GH_CREATE_EXIT=1 FAKE_HTTP_STATUS=403 GH_TOKEN=test \
    "$repo_root/scripts/create-github-draft-release.sh" butterflyskies/pali v0.18.0 false; then
    fail 'failed release creation with ambiguous authorization was treated as success'
fi
archive="$tmp_dir/pali-v0.18.0-linux.tar.gz"
checksum="$archive.sha256"
printf 'fresh archive bytes\n' > "$archive"
printf '%s  %s\n' "$(sha256sum "$archive" | awk '{ print $1 }')" "$(basename "$archive")" > "$checksum"

# macos-latest provides BSD shasum, not GNU sha256sum. Exercise the actual
# helper with a PATH that deliberately contains only the former digest tool.
mac_bin="$tmp_dir/mac-bin"
mkdir "$mac_bin"
for tool in awk basename cmp cp grep mktemp rm shasum; do
    ln -s "$(command -v "$tool")" "$mac_bin/$tool"
done
cp "$fake_bin/gh" "$mac_bin/gh"
chmod +x "$mac_bin/gh"
: > "$tmp_dir/assets-macos.log"
PATH="$mac_bin" FAKE_ASSETS='' FAKE_GH_LOG="$tmp_dir/assets-macos.log" \
    FAKE_REMOTE_DIR="$tmp_dir" \
    "$repo_root/scripts/upload-release-asset-pair.sh" butterflyskies/pali v0.18.0 \
    "$archive" "$checksum"
assert_output "$tmp_dir/assets-macos.log" "upload $archive $checksum"
printf 'not a checksum\n' > "$tmp_dir/bad-local.sha256"
if PATH="$fake_bin:$PATH" FAKE_ASSETS='' FAKE_GH_LOG="$tmp_dir/bad-local.log" \
    FAKE_REMOTE_DIR="$tmp_dir" \
    "$repo_root/scripts/upload-release-asset-pair.sh" butterflyskies/pali v0.18.0 \
    "$archive" "$tmp_dir/bad-local.sha256"; then
    fail 'invalid locally prepared asset pair reached publication'
fi
for state in none archive both; do
    log="$tmp_dir/assets-$state.log"
    : > "$log"
    case "$state" in
        none) assets='' ;;
        archive) assets=$(basename "$archive") ;;
        both) assets="$(basename "$archive")
$(basename "$checksum")" ;;
    esac
    remote_dir="$tmp_dir/assets-$state-remote"
    mkdir "$remote_dir"
    [ "$state" = archive ] || [ "$state" = both ] && cp "$archive" "$remote_dir/$(basename "$archive")"
    [ "$state" = both ] && cp "$checksum" "$remote_dir/$(basename "$checksum")"
    PATH="$fake_bin:$PATH" FAKE_ASSETS="$assets" FAKE_GH_LOG="$log" FAKE_REMOTE_DIR="$remote_dir" \
        "$repo_root/scripts/upload-release-asset-pair.sh" butterflyskies/pali v0.18.0 "$archive" "$checksum"
done
assert_output "$tmp_dir/assets-none.log" "upload $archive $checksum"
grep -E "^upload .*/$(basename "$checksum")$" "$tmp_dir/assets-archive.log" >/dev/null || \
    fail 'archive-only repair did not upload exactly one derived checksum'
[ ! -s "$tmp_dir/assets-both.log" ] || fail 'complete release asset pair was replaced'

for state in stale-valid corrupt-checksum; do
    log="$tmp_dir/assets-$state.log"
    remote_dir="$tmp_dir/assets-$state-remote"
    mkdir "$remote_dir"
    cp "$archive" "$remote_dir/$(basename "$archive")"
    cp "$checksum" "$remote_dir/$(basename "$checksum")"
    if [ "$state" = stale-valid ]; then
        printf 'stale archive bytes\n' > "$remote_dir/$(basename "$archive")"
        printf '%s  %s\n' \
            "$(sha256sum "$remote_dir/$(basename "$archive")" | awk '{ print $1 }')" \
            "$(basename "$archive")" > "$remote_dir/$(basename "$checksum")"
    else
        printf 'corrupt checksum\n' > "$remote_dir/$(basename "$checksum")"
    fi
    : > "$log"
    assets="$(basename "$archive")
$(basename "$checksum")"
    if [ "$state" = stale-valid ]; then
        if PATH="$fake_bin:$PATH" FAKE_ASSETS="$assets" FAKE_GH_LOG="$log" FAKE_REMOTE_DIR="$remote_dir" \
            "$repo_root/scripts/upload-release-asset-pair.sh" butterflyskies/pali v0.18.0 "$archive" "$checksum"; then
            fail 'internally valid but source-mismatched asset pair was accepted'
        fi
    elif PATH="$fake_bin:$PATH" FAKE_ASSETS="$assets" FAKE_GH_LOG="$log" FAKE_REMOTE_DIR="$remote_dir" \
        "$repo_root/scripts/upload-release-asset-pair.sh" butterflyskies/pali v0.18.0 "$archive" "$checksum"; then
        fail 'corrupt complete asset pair was silently replaced'
    fi
done

different_checksum_remote="$tmp_dir/assets-different-checksum-remote"
mkdir "$different_checksum_remote"
cp "$archive" "$different_checksum_remote/$(basename "$archive")"
printf '%s %s\n' "$(sha256sum "$archive" | awk '{ print $1 }')" "$(basename "$archive")" \
    > "$different_checksum_remote/$(basename "$checksum")"
if PATH="$fake_bin:$PATH" FAKE_ASSETS="$(basename "$archive")
$(basename "$checksum")" FAKE_GH_LOG="$tmp_dir/assets-different-checksum.log" \
    FAKE_REMOTE_DIR="$different_checksum_remote" \
    "$repo_root/scripts/upload-release-asset-pair.sh" butterflyskies/pali v0.18.0 "$archive" "$checksum"; then
    fail 'semantically valid but byte-different checksum asset was accepted as exact'
fi

checksum_remote="$tmp_dir/assets-checksum-remote"
mkdir "$checksum_remote"
cp "$checksum" "$checksum_remote/$(basename "$checksum")"
PATH="$fake_bin:$PATH" FAKE_ASSETS="$(basename "$checksum")" \
    FAKE_GH_LOG="$tmp_dir/assets-checksum.log" FAKE_REMOTE_DIR="$checksum_remote" \
    "$repo_root/scripts/upload-release-asset-pair.sh" butterflyskies/pali v0.18.0 "$archive" "$checksum"
assert_output "$tmp_dir/assets-checksum.log" "upload $archive"

oci_fixture="$tmp_dir/oci-fixture"
mkdir "$oci_fixture"
printf '%s\n' '{"schemaVersion":2,"manifests":[{"digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","annotations":{"org.opencontainers.image.ref.name":"prepared"}}]}' \
    > "$oci_fixture/index.json"
tar -cf "$tmp_dir/prepared-oci.tar" -C "$oci_fixture" index.json
"$repo_root/scripts/verify-oci-release-archive.sh" "$tmp_dir/prepared-oci.tar" prepared
if "$repo_root/scripts/verify-oci-release-archive.sh" "$tmp_dir/prepared-oci.tar" pali-release:prepared; then
    fail 'OCI archive verifier accepted the wrong source reference'
fi

# Wiring assertions make the executable oracles non-vacuous: a workflow that
# stops calling them, restores GitHub tag creation, or republishes the legacy
# image name must turn this test red.
[ ! -e "$repo_root/.github/workflows/tag-release.yml" ] || fail 'GitHub still owns a tag-creation workflow'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" 'needs: linux'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" 'group: forgejo-ci-${{ forgejo.event_name }}-${{ forgejo.sha }}'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" 'scripts/tag-release-on-version-change.sh'
assert_contains "$repo_root/scripts/tag-release-on-version-change.sh" 'release-version-is-greater.sh'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" 'push origin "refs/tags/${TAG_NAME}:refs/tags/${TAG_NAME}"'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" 'git checkout --detach "refs/tags/${TAG_NAME}"'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" 'needs: release-boundary'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" 'release_commit: ${{ steps.verified-boundary.outputs.release_commit }}'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" 'tested_commit=$(git rev-parse HEAD)'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" 'tag_commit=$(git rev-parse "refs/tags/${TAG_NAME}^{commit}")'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" '[ "$tested_commit" = "$tag_commit" ]'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" '[ "$tested_commit" = "$PROPOSED_COMMIT" ]'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" 'git merge-base --is-ancestor "$tested_commit" "$CANONICAL_MAIN_SHA"'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" "'+refs/heads/main:refs/release-authority/main'"
assert_contains "$repo_root/.forgejo/workflows/ci.yml" '"$RELEASE_COMMIT" refs/release-authority/main'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" 'persist-credentials: false'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" 'http.extraHeader=Authorization: Basic ${auth_header}'
for boundary_gate in formatting workflows 'default features' 'all features' dependencies; do
    assert_contains "$repo_root/.forgejo/workflows/ci.yml" "release-boundary $boundary_gate"
done
boundary_checkout_line=$(grep -n 'git checkout --detach "refs/tags/${TAG_NAME}"' "$repo_root/.forgejo/workflows/ci.yml" | cut -d: -f1)
boundary_push_line=$(grep -n 'push origin "refs/tags/${TAG_NAME}:refs/tags/${TAG_NAME}"' "$repo_root/.forgejo/workflows/ci.yml" | cut -d: -f1)
[ "$boundary_checkout_line" -lt "$boundary_push_line" ] || fail 'release tag is pushed before exact-boundary testing'
# The pushed tag is annotated and SSH-signed; only tag-release sees the key.
assert_contains "$repo_root/scripts/tag-release-on-version-change.sh" 'TAG_SIGNING=none "$script_dir/create-release-tag.sh"'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" 'TAG_SIGNING: ssh'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" 'TAG_SIGNING_KEY: ${{ secrets.TAG_SIGNING_KEY }}'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" 'scripts/create-release-tag.sh "$TAG_NAME" "$RELEASE_COMMIT"'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" '[ "$(git cat-file -t "refs/tags/${TAG_NAME}")" = tag ]'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" 'scripts/verify-remote-release-tag.sh origin "$TAG_NAME" "$RELEASE_COMMIT" main "$tag_object"'
assert_contains "$repo_root/.forgejo/workflows/ci.yml" 'tag_object=$(git rev-parse "refs/tags/${TAG_NAME}")'
if grep -F -- '--no-sign' "$repo_root/.forgejo/workflows/ci.yml" >/dev/null; then
    fail 'Forgejo CI still creates an unsigned release tag directly'
fi
[ -f "$repo_root/.forgejo/allowed_signers" ] || fail '.forgejo/allowed_signers is missing'
tag_release_job_line=$(grep -n '^  tag-release:' "$repo_root/.forgejo/workflows/ci.yml" | cut -d: -f1)
first_key_line=$(grep -n 'secrets.TAG_SIGNING_KEY' "$repo_root/.forgejo/workflows/ci.yml" | head -n 1 | cut -d: -f1)
[ "$first_key_line" -gt "$tag_release_job_line" ] || fail 'the tag-signing key reaches a job before tag-release'
# Every GitHub workflow that checks a release tag requires a signed tag (or a
# listed legacy one) at workflow level, so no step can run with a weaker mode.
for workflow in "$repo_root"/.github/workflows/*.yml; do
    grep -E 'verify-(remote-)?release-tag\.sh' "$workflow" >/dev/null || continue
    awk '
        /^env:[[:space:]]*$/ { in_env = 1; next }
        /^[^[:space:]#]/ { in_env = 0 }
        in_env && /^  TAG_SIGNING: ssh[[:space:]]*$/ { found = 1 }
        END { exit(found ? 0 : 1) }
    ' "$workflow" || fail "$workflow checks release tags without workflow-level TAG_SIGNING: ssh"
done
legacy_release_tags="$repo_root/release/legacy-release-tags.txt"
assert_output "$legacy_release_tags" 'v0.19.0 d2241c23f14d613b8356b45763cfb5dba43ef693'
assert_output "$legacy_release_tags" 'v0.17.2 20454674c76d14955ab1b52af66c5adb5ef65557'
grep -v '^#' "$legacy_release_tags" | while read -r legacy_tag legacy_commit legacy_extra; do
    "$repo_root/scripts/validate-release-tag.sh" "$legacy_tag" || fail "malformed legacy release tag: $legacy_tag"
    printf '%s\n' "$legacy_commit" | grep -Eq '^[0-9a-f]{40}$' || fail "malformed legacy commit for $legacy_tag"
    [ -z "$legacy_extra" ] || fail "trailing data on legacy release tag $legacy_tag"
done
remote_readback_line=$(grep -n 'scripts/verify-remote-release-tag.sh origin "$TAG_NAME"' "$repo_root/.forgejo/workflows/ci.yml" | cut -d: -f1)
[ "$boundary_push_line" -lt "$remote_readback_line" ] || fail 'pushed release tag is not read back from Forgejo'
assert_contains "$repo_root/.github/workflows/release.yml" 'scripts/github-release-status.sh'
assert_contains "$repo_root/.github/workflows/release.yml" 'scripts/create-github-draft-release.sh'
assert_contains "$repo_root/.github/workflows/release.yml" 'scripts/verify-release-tag.sh'
assert_contains "$repo_root/.github/workflows/release.yml" 'scripts/verify-remote-release-tag.sh'
assert_contains "$repo_root/scripts/create-github-draft-release.sh" '--verify-tag'
assert_contains "$repo_root/.github/workflows/release.yml" 'ref: refs/tags/${{ steps.release-ref.outputs.tag_name }}'
assert_contains "$repo_root/.github/workflows/release.yml" "github.ref == format('refs/heads/{0}', github.event.repository.default_branch)"
assert_contains "$repo_root/.github/workflows/release.yml" 'ref: ${{ github.sha }}'
assert_contains "$repo_root/.github/workflows/release.yml" 'path: release-source'
assert_contains "$repo_root/.github/workflows/release.yml" 'workflow_dispatch:'
assert_contains "$repo_root/.github/workflows/release.yml" "github.event_name == 'workflow_dispatch' && inputs.tag || github.ref_name"
assert_contains "$repo_root/.github/workflows/release.yml" '--repo "${{ github.repository }}"'
assert_contains "$repo_root/.github/workflows/release.yml" 'scripts/upload-release-asset-pair.sh'
assert_contains "$repo_root/.github/workflows/release.yml" 'brew install gnu-tar'
assert_contains "$repo_root/.github/workflows/release.yml" 'tar_command=gtar'
assert_contains "$repo_root/.github/workflows/release.yml" '"$tar_command" --sort=name --mtime="@${source_date_epoch}"'
assert_contains "$repo_root/.github/workflows/release.yml" 'gzip -n'
if grep -F -- '--clobber' "$repo_root/.github/workflows/release.yml" >/dev/null; then
    fail 'repair workflow deletes existing release assets before replacement upload'
fi
assert_contains "$repo_root/.github/workflows/release.yml" "group: release-\${{ github.event_name }}-\${{ github.event_name == 'workflow_dispatch' && inputs.tag || github.ref_name }}"
if grep -R -F 'floating_tags' "$repo_root/.github/workflows" >/dev/null; then
    fail 'release workflows still maintain lossy mutable floating image tags'
fi
if grep -F 'pattern={{major}}' "$repo_root/.github/workflows/publish-image.yml" >/dev/null; then
    fail 'release image workflow still emits a mutable major/minor tag'
fi
assert_contains "$repo_root/.github/workflows/publish-image.yml" 'flavor: latest=false'
if grep -R -F 'ghcr.io/butterflyskies/pali:latest' "$repo_root/docs" >/dev/null; then
    fail 'documentation still directs consumers to the unordered mutable latest tag'
fi
assert_contains "$repo_root/docs/getting-started.md" 'ghcr.io/butterflyskies/pali:<VERSION>'
assert_contains "$repo_root/.github/workflows/release.yml" "github.event_name != 'workflow_dispatch' && needs.create-release.outputs.is_prerelease != 'true'"
assert_contains "$repo_root/.github/workflows/publish-crate.yml" 'scripts/crates-io-publish-status.sh'
assert_contains "$repo_root/.github/workflows/publish-crate.yml" 'cargo package --no-verify'
assert_contains "$repo_root/.github/workflows/publish-crate.yml" 'ref: ${{ inputs.control_ref }}'
assert_contains "$repo_root/.github/workflows/release.yml" 'control_ref: ${{ github.sha }}'
[ "$(grep -c 'path: .release-control' "$repo_root/.github/workflows/release.yml")" -eq 3 ] || \
    fail 'binary release job does not have its own controller checkout'
[ "$(grep -c 'path: release-source' "$repo_root/.github/workflows/release.yml")" -eq 3 ] || \
    fail 'binary release job does not isolate historical release source'
assert_contains "$repo_root/.github/workflows/publish-crate.yml" 'environment: release'
assert_contains "$repo_root/.github/workflows/publish-crate.yml" 'working-directory: release-source'
assert_contains "$repo_root/.github/workflows/publish-crate.yml" "if: steps.crate-status.outputs.publish == 'true'"
assert_contains "$repo_root/.github/workflows/publish-crate.yml" 'cargo publish --no-verify'
assert_contains "$repo_root/.github/workflows/build.yml" 'IMAGE: butterflyskies/pali-ci'
assert_contains "$repo_root/.github/workflows/build.yml" 'push: false'
if grep -F 'packages: write' "$repo_root/.github/workflows/build.yml" >/dev/null ||
    grep -F 'docker/login-action@' "$repo_root/.github/workflows/build.yml" >/dev/null; then
    fail 'ordinary CI still receives a GHCR write credential'
fi
assert_contains "$repo_root/.github/workflows/publish-image.yml" 'IMAGE: butterflyskies/pali'
assert_contains "$repo_root/.github/workflows/publish-image.yml" 'username: ${{ github.actor }}'
assert_contains "$repo_root/.github/workflows/publish-image.yml" 'password: ${{ secrets.GITHUB_TOKEN }}'
assert_contains "$repo_root/.github/workflows/publish-image.yml" 'packages: write'
if grep -R -E 'GHCR_RELEASE_(USER|TOKEN)' \
    "$repo_root/.github/workflows" "$repo_root/docs" >/dev/null; then
    fail 'release workflows require separately configured GHCR credentials'
fi
assert_contains "$repo_root/.github/workflows/release.yml" 'verify-manual-image:'
assert_contains "$repo_root/.github/workflows/release.yml" 'scripts/verify-manual-release-image.sh'
assert_contains "$repo_root/.github/workflows/release.yml" 'image_digest: ${{ steps.verify.outputs.image_digest }}'
assert_contains "$repo_root/.github/workflows/release.yml" 'EXPECTED_IMAGE_DIGEST: ${{ needs.verify-manual-image.outputs.image_digest }}'
assert_contains "$repo_root/.github/workflows/release.yml" '"$EXPECTED_IMAGE_DIGEST"'
assert_contains "$repo_root/.github/workflows/release.yml" 'ghcr-image-status.sh require-digest'
assert_contains "$repo_root/release/legacy-image-digests.txt" 'v0.17.2 sha256:7238086b580b53b4b751d6e9f19bbdab7f4b9e69964a05fe7df242623e6dc226'
assert_contains "$repo_root/.github/workflows/release.yml" "needs.verify-manual-image.result == 'success'"
assert_contains "$repo_root/.github/workflows/publish-image.yml" 'environment: release'
assert_contains "$repo_root/.github/workflows/update-release-notes.yml" 'environment: release'
assert_contains "$repo_root/.github/workflows/publish-crate.yml" 'ref: ${{ inputs.source_ref }}'
assert_contains "$repo_root/.github/workflows/publish-image.yml" 'ref: ${{ inputs.source_ref }}'
assert_contains "$repo_root/.github/workflows/update-release-notes.yml" 'ref: ${{ inputs.source_ref }}'
if grep -R -F 'ref: ${{ inputs.tag }}' "$repo_root/.github/workflows" >/dev/null; then
    fail 'a release source checkout can still be shadowed by a same-named branch'
fi
# Missing-image repair is a separate manual entry point: default branch only,
# bound to the remote tag's commit, serialized with that tag's push
# publication, and publishing only through publish-image.yml's no-overwrite
# path. The Release workflow's own manual repair still never publishes one.
repair_image="$repo_root/.github/workflows/repair-missing-image.yml"
assert_active "$repair_image" 'workflow_dispatch:'
assert_active "$repair_image" "if: \${{ github.ref == format('refs/heads/{0}', github.event.repository.default_branch) }}"
assert_active "$repair_image" 'group: release-push-${{ inputs.tag }}'
assert_active "$repair_image" 'ref: refs/tags/${{ steps.release-ref.outputs.tag_name }}'
assert_active "$repair_image" 'run: ../.release-control/scripts/verify-remote-release-tag.sh origin "$TAG_NAME" "$(git rev-parse HEAD)" "$DEFAULT_BRANCH"'
assert_active "$repair_image" "if: steps.tag-class.outputs.is_prerelease == 'true'"
assert_active "$repair_image" 'uses: ./.github/workflows/publish-image.yml'
assert_active "$repair_image" 'source_ref: ${{ needs.resolve-release.outputs.release_sha }}'
if grep -E '^[[:space:]]+(push|pull_request|workflow_call|schedule):' "$repair_image" >/dev/null; then
    fail 'missing-image repair has a trigger other than workflow_dispatch'
fi
[ "$(grep -c 'docker/build-push-action@\|skopeo copy' "$repair_image")" -eq 0 ] || \
    fail 'missing-image repair publishes outside publish-image.yml'
for workflow in publish-crate.yml publish-image.yml update-release-notes.yml; do
    assert_contains "$repo_root/.github/workflows/$workflow" 'scripts/verify-remote-release-tag.sh'
done
if grep -R -E '(^[[:space:]]*run:|origin).*\$\{\{ github\.event\.repository\.default_branch \}\}' \
    "$repo_root/.github/workflows" >/dev/null; then
    fail 'default branch context is interpolated directly into privileged shell source'
fi
for validator in tag-release-on-version-change.sh verify-release-tag.sh github-release-status.sh; do
    assert_contains "$repo_root/scripts/$validator" 'validate-release-tag.sh'
done
prepare_line=$(grep -n 'name: Prepare image build' "$repo_root/.github/workflows/publish-image.yml" | cut -d: -f1)
verify_line=$(grep -n 'name: Verify release tag binding at publication boundary' "$repo_root/.github/workflows/publish-image.yml" | cut -d: -f1)
publish_line=$(grep -n 'name: Publish verified image' "$repo_root/.github/workflows/publish-image.yml" | cut -d: -f1)
[ "$prepare_line" -lt "$verify_line" ] && [ "$verify_line" -lt "$publish_line" ] || \
    fail 'image tag binding is not checked between preparation and publication'
[ "$(grep -c 'docker/build-push-action@' "$repo_root/.github/workflows/publish-image.yml")" -eq 1 ] || \
    fail 'image publication rebuilds after the tag-binding check'
assert_contains "$repo_root/.github/workflows/publish-image.yml" 'outputs: type=oci,dest=${{ runner.temp }}/pali-image.tar'
assert_contains "$repo_root/.github/workflows/publish-image.yml" 'tags: pali-release:prepared'
assert_contains "$repo_root/.github/workflows/publish-image.yml" 'skopeo copy --all'
assert_contains "$repo_root/.github/workflows/publish-image.yml" '--preserve-digests'
assert_contains "$repo_root/.github/workflows/publish-image.yml" 'scripts/verify-oci-release-archive.sh'
assert_contains "$repo_root/.github/workflows/publish-image.yml" 'scripts/ghcr-image-status.sh'
assert_contains "$repo_root/.github/workflows/publish-image.yml" "if: steps.existing-image.outputs.exists == 'false'"
assert_contains "$repo_root/.github/workflows/publish-image.yml" 'org.opencontainers.image.revision=${{ inputs.source_ref }}'
assert_contains "$repo_root/.github/workflows/publish-image.yml" 'oci-archive:${{ runner.temp }}/pali-image.tar:prepared'
assert_contains "$repo_root/scripts/crates-io-publish-status.sh" '--connect-timeout 10'
assert_contains "$repo_root/scripts/crates-io-publish-status.sh" '--max-time 30'
assert_contains "$repo_root/scripts/github-release-status.sh" '--connect-timeout 10'
assert_contains "$repo_root/scripts/github-release-status.sh" '--max-time 30'
assert_contains "$repo_root/docs/adr/0045-forgejo-tag-authority-github-release-executor.md" 'permits creation only by'
assert_contains "$repo_root/docs/adr/0045-forgejo-tag-authority-github-release-executor.md" 'cannot prove who created'
assert_contains "$repo_root/docs/adr/0045-forgejo-tag-authority-github-release-executor.md" "Forgejo \`tag-release\` job's token"
assert_contains "$repo_root/docs/adr/0045-forgejo-tag-authority-github-release-executor.md" "GitHub's \`release\` environment"
assert_contains "$repo_root/docs/adr/0045-forgejo-tag-authority-github-release-executor.md" 'automatic `GITHUB_TOKEN`'
[ "$(grep -c 'environment: release' "$repo_root/.github/workflows/release.yml")" -eq 4 ] || \
    fail 'privileged release jobs are not all protected by the release environment'

assert_contains "$repo_root/.github/workflows/release.yml" 'needs: [create-release, publish-binaries, publish-crate, publish-image, verify-manual-image, update-release-notes]'
assert_contains "$repo_root/.github/workflows/release.yml" 'needs: [create-release, publish-binaries, publish-crate, publish-image, verify-manual-image]'
assert_contains "$repo_root/.github/workflows/release.yml" "needs.update-release-notes.result == 'success'"
notes_line=$(grep -n '^  update-release-notes:' "$repo_root/.github/workflows/release.yml" | cut -d: -f1)
undraft_line=$(grep -n 'gh release edit "$TAG_NAME"' "$repo_root/.github/workflows/release.yml" | cut -d: -f1)
[ "$notes_line" -gt 0 ] && [ "$undraft_line" -gt 0 ] || fail 'release notes or undraft job is missing'
[ "$(grep -c -- '--draft=false' "$repo_root/.github/workflows/release.yml")" -eq 1 ] || \
    fail 'release can be undrafted outside the terminal publication job'

if grep -R 'butterflyskies/memory-mcp' \
    "$repo_root/.github/workflows" "$repo_root/docs/deployment.md" "$repo_root/docs/getting-started.md" >/dev/null; then
    fail 'active workflow or deployment documentation still names the legacy GHCR image'
fi

# A plain-scalar run: folds its line break into a space, so a trailing
# backslash escapes that space and glues it onto the next argument.
# Multi-line commands use a block scalar (run: |).
if grep -RnE '^[[:space:]]*(- )?run:[[:space:]]+[^|>[:space:]].*\\$' \
    "$repo_root/.github/workflows" "$repo_root/.forgejo/workflows"; then
    fail 'a plain-scalar run: ends in a backslash; use run: | for multi-line commands'
fi
# A folded block scalar (run: >) joins lines the same way.
if grep -RnE '^[[:space:]]*(- )?run:[[:space:]]*>' \
    "$repo_root/.github/workflows" "$repo_root/.forgejo/workflows"; then
    fail 'run: > folds line breaks into spaces; use run: | for multi-line commands'
fi

echo 'release workflow tests passed'
