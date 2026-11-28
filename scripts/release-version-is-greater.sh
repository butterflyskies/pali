#!/bin/sh
# Succeed only when NEW is strictly greater than OLD in Pali's SemVer dialect.
set -eu

new=${1:?usage: release-version-is-greater.sh NEW OLD}
old=${2:?usage: release-version-is-greater.sh NEW OLD}
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

# Validation is internal to the comparison. Do not let the validator's optional
# workflow output overwrite the caller's release classification.
(unset GITHUB_OUTPUT; "$script_dir/validate-release-tag.sh" "v$new" >/dev/null)
(unset GITHUB_OUTPUT; "$script_dir/validate-release-tag.sh" "v$old" >/dev/null)

awk -v new="$new" -v old="$old" '
function numeric_compare(a, b,    aa, bb) {
    aa = a; bb = b
    sub(/^0+/, "", aa); sub(/^0+/, "", bb)
    if (aa == "") aa = "0"
    if (bb == "") bb = "0"
    if (length(aa) != length(bb)) return length(aa) > length(bb) ? 1 : -1
    if (aa == bb) return 0
    return aa > bb ? 1 : -1
}
function compare(a, b,    ap, bp, ac, bc, acn, bcn, an, bn, i, c, a_num, b_num) {
    ap = index(a, "-"); bp = index(b, "-")
    ac = ap ? substr(a, 1, ap - 1) : a
    bc = bp ? substr(b, 1, bp - 1) : b
    split(ac, an, "."); split(bc, bn, ".")
    for (i = 1; i <= 3; i++) {
        c = numeric_compare(an[i], bn[i])
        if (c) return c
    }
    if (!ap && !bp) return 0
    if (!ap) return 1
    if (!bp) return -1
    ac = substr(a, ap + 1); bc = substr(b, bp + 1)
    acn = split(ac, an, "."); bcn = split(bc, bn, ".")
    for (i = 1; i <= acn && i <= bcn; i++) {
        a_num = an[i] ~ /^[0-9]+$/; b_num = bn[i] ~ /^[0-9]+$/
        if (a_num && b_num) c = numeric_compare(an[i], bn[i])
        else if (a_num != b_num) c = a_num ? -1 : 1
        else if (an[i] == bn[i]) c = 0
        else c = an[i] > bn[i] ? 1 : -1
        if (c) return c
    }
    if (acn == bcn) return 0
    return acn > bcn ? 1 : -1
}
BEGIN { exit(compare(new, old) > 0 ? 0 : 1) }
'
