#!/bin/sh
# Reconcile an exact immutable GHCR tag by prepared digest or source commit.
set -eu

mode=${1:?usage: ghcr-image-status.sh digest|require-digest|source IMAGE TAG EXPECTED}
image=${2:?usage: ghcr-image-status.sh digest|require-digest|source IMAGE TAG EXPECTED}
tag=${3:?usage: ghcr-image-status.sh digest|require-digest|source IMAGE TAG EXPECTED}
expected=${4:?usage: ghcr-image-status.sh digest|require-digest|source IMAGE TAG EXPECTED}

if ! printf '%s\n' "$image" | grep -Eq '^ghcr\.io/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$'; then
    echo "invalid GHCR image: $image" >&2
    exit 1
fi
if ! printf '%s\n' "$tag" | grep -Eq '^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$'; then
    echo "invalid OCI tag: $tag" >&2
    exit 1
fi
case "$mode" in
    digest|require-digest)
        printf '%s\n' "$expected" | grep -Eq '^sha256:[0-9a-f]{64}$' || {
            echo "invalid expected image digest: $expected" >&2
            exit 1
        }
        ;;
    source)
        printf '%s\n' "$expected" | grep -Eq '^[0-9a-f]{40}$' || {
            echo "invalid expected source commit: $expected" >&2
            exit 1
        }
        ;;
    *)
        echo "invalid reconciliation mode: $mode" >&2
        exit 1
        ;;
esac

registry=${image%%/*}
repository=${image#*/}
bearer_token=${GHCR_BEARER_TOKEN:-}
token_file=
manifest_file=
trap 'rm -f ${token_file:+"$token_file"} ${manifest_file:+"$manifest_file"}' EXIT HUP INT TERM
if [ -z "$bearer_token" ]; then
    : "${GHCR_USER:?GHCR_USER is required}"
    : "${GHCR_TOKEN:?GHCR_TOKEN is required}"
    token_file=$(mktemp)
    curl --silent --show-error --fail \
        --connect-timeout 10 --max-time 30 \
        --user "$GHCR_USER:$GHCR_TOKEN" \
        --get "https://$registry/token" \
        --data-urlencode "service=$registry" \
        --data-urlencode "scope=repository:$repository:pull" \
        --output "$token_file"
    bearer_token=$(jq -er '.token | select(type == "string" and length > 0)' "$token_file")
fi

manifest_file=$(mktemp)
status=$(curl --silent --show-error \
    --connect-timeout 10 --max-time 30 \
    --header "Authorization: Bearer $bearer_token" \
    --header 'Accept: application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json' \
    --output "$manifest_file" --write-out '%{http_code}' \
    "https://$registry/v2/$repository/manifests/$tag")

case "$status" in
    200)
        actual_digest="sha256:$(sha256sum "$manifest_file" | awk '{ print $1 }')"
        if [ "$mode" = digest ] || [ "$mode" = require-digest ]; then
            if [ "$actual_digest" != "$expected" ]; then
                echo "GHCR tag $image:$tag resolves to $actual_digest, not prepared $expected" >&2
                exit 1
            fi
            if [ "$mode" = digest ]; then
                [ -z "${GITHUB_OUTPUT:-}" ] || echo "publish=false" >> "$GITHUB_OUTPUT"
            else
                [ -z "${GITHUB_OUTPUT:-}" ] || echo "verified=true" >> "$GITHUB_OUTPUT"
            fi
            echo "GHCR tag $image:$tag already matches the prepared image"
        else
            actual_source=$(skopeo inspect \
                --authfile "${REGISTRY_AUTH_FILE:-$HOME/.docker/config.json}" \
                --format '{{ index .Labels "org.opencontainers.image.revision" }}' \
                "docker://$image@$actual_digest")
            if [ "$actual_source" != "$expected" ]; then
                echo "GHCR tag $image:$tag identifies source $actual_source, not release $expected" >&2
                exit 1
            fi
            if [ -n "${GITHUB_OUTPUT:-}" ]; then
                echo "exists=true" >> "$GITHUB_OUTPUT"
                echo "image_digest=$actual_digest" >> "$GITHUB_OUTPUT"
            fi
            echo "GHCR tag $image:$tag already identifies the exact release source"
        fi
        ;;
    404)
        if [ "$mode" = digest ]; then
            [ -z "${GITHUB_OUTPUT:-}" ] || echo "publish=true" >> "$GITHUB_OUTPUT"
        elif [ "$mode" = require-digest ]; then
            echo "required immutable GHCR tag $image:$tag is absent" >&2
            exit 1
        else
            [ -z "${GITHUB_OUTPUT:-}" ] || echo "exists=false" >> "$GITHUB_OUTPUT"
        fi
        echo "GHCR tag $image:$tag is absent"
        ;;
    *)
        echo "GHCR manifest lookup returned HTTP $status" >&2
        exit 1
        ;;
esac
