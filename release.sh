#!/bin/bash
set -euo pipefail

# Creates a GitHub release for the next version tag. Publishing the release
# triggers .github/workflows/publish.yml, which sets both crate versions from the
# tag and publishes to crates.io. Prerelease tags use Cargo semver: v0.1.0-rc.1.

TAG_PREFIX="${1:-v}"

# Refuse to release unless every check on main's head commit passed: the
# release goes public before the publish workflow runs its own checks.
MAIN_SHA=$(gh api 'repos/{owner}/{repo}/commits/main' --jq .sha)
NOT_GREEN=$(gh api "repos/{owner}/{repo}/commits/${MAIN_SHA}/check-runs" \
    --jq '(.check_runs | length == 0) or any(.check_runs[]; .conclusion != "success" and .conclusion != "skipped")')
if [[ "$NOT_GREEN" != "false" ]]; then
    echo "CI on main (${MAIN_SHA:0:7}) is not green yet; wait for it or fix it before releasing." >&2
    exit 1
fi

# Highest published (non-draft) release by semver; -rc.N sorts below its release.
TAGS=$(gh release list --exclude-drafts --limit 1000 --json tagName --jq '.[].tagName')
LAST_TAG=$(grep "^${TAG_PREFIX}[0-9]" <<<"$TAGS" | sed 's/-rc\./~rc./' | sort -V | tail -1 | sed 's/~rc\./-rc./' || true)
LAST_TAG="${LAST_TAG:-"${TAG_PREFIX}0.0.0"}"

highlight_version() {
    local bump="$1"
    local tag="$2"
    local prefix="${tag%%[0-9]*}"
    local version="${tag#"$prefix"}"
    local base="${version%%-rc.*}"
    local rc_suffix="${version#"$base"}"
    local major minor patch
    IFS='.' read -r major minor patch _ <<<"$base"

    local G="\033[1;32m"
    local R="\033[0m"
    case "$bump" in
        major) printf "%s${G}%s${R}.%s.%s%s" "$prefix" "$major" "$minor" "$patch" "$rc_suffix" ;;
        minor) printf "%s%s.${G}%s${R}.%s%s" "$prefix" "$major" "$minor" "$patch" "$rc_suffix" ;;
        patch) printf "%s%s.%s.${G}%s${R}%s" "$prefix" "$major" "$minor" "$patch" "$rc_suffix" ;;
        rc)    printf "%s%s.%s.%s${G}%s${R}" "$prefix" "$major" "$minor" "$patch" "$rc_suffix" ;;
    esac
}

draw_bump_menu() {
    local idx="$1"
    local patch_tag="$2"
    local minor_tag="$3"
    local major_tag="$4"
    local rc_tag="$5"
    printf "Select version bump (j/k or up/down or 1-4, Enter to confirm):\n" >&2
    printf "%s 1) patch (" "$( [[ "$idx" -eq 0 ]] && echo ">" || echo " " )" >&2; highlight_version "patch" "$patch_tag" >&2; printf ")\n" >&2
    printf "%s 2) minor (" "$( [[ "$idx" -eq 1 ]] && echo ">" || echo " " )" >&2; highlight_version "minor" "$minor_tag" >&2; printf ")\n" >&2
    printf "%s 3) major (" "$( [[ "$idx" -eq 2 ]] && echo ">" || echo " " )" >&2; highlight_version "major" "$major_tag" >&2; printf ")\n" >&2
    printf "%s 4) rc    (" "$( [[ "$idx" -eq 3 ]] && echo ">" || echo " " )" >&2; highlight_version "rc" "$rc_tag" >&2; printf ")\n" >&2
}

clear_bump_menu() {
    for _ in 1 2 3 4 5; do
        printf "\033[1A\033[2K" >&2
    done
}

select_bump() {
    local last_tag="$1"
    local options=("patch" "minor" "major" "rc")
    local patch_tag=""
    local minor_tag=""
    local major_tag=""
    local rc_tag=""
    local idx=0
    local key=""
    local key_rest=""
    local new_idx=0

    patch_tag=$(next_tag_for_bump "patch" "$last_tag") || return 1
    minor_tag=$(next_tag_for_bump "minor" "$last_tag") || return 1
    major_tag=$(next_tag_for_bump "major" "$last_tag") || return 1
    rc_tag=$(next_tag_for_bump "rc" "$last_tag") || return 1

    if [[ ! -t 0 ]]; then
        printf "Select version bump: 1) patch (%s) 2) minor (%s) 3) major (%s) 4) rc (%s): " "$patch_tag" "$minor_tag" "$major_tag" "$rc_tag" >&2
        read -r key
        case "$key" in
            1|patch) echo "patch" ;;
            2|minor) echo "minor" ;;
            3|major) echo "major" ;;
            4|rc) echo "rc" ;;
            *) echo "Invalid selection." >&2; return 1 ;;
        esac
        return 0
    fi

    draw_bump_menu "$idx" "$patch_tag" "$minor_tag" "$major_tag" "$rc_tag"
    while true; do
        new_idx="$idx"
        IFS= read -rsn1 key
        if [[ "$key" == $'\x1b' ]]; then
            read -rsn2 -t 1 key_rest || true
            key+="$key_rest"
        fi

        case "$key" in
            "") printf "\n" >&2; echo "${options[$idx]}"; return 0 ;;
            1|2|3|4) printf "\n" >&2; echo "${options[$((key-1))]}"; return 0 ;;
            j|$'\x1b[B'|$'\x1bOB') new_idx=$(( (idx + 1) % 4 )) ;;
            k|$'\x1b[A'|$'\x1bOA') new_idx=$(( (idx + 3) % 4 )) ;;
        esac

        if [[ "$new_idx" -ne "$idx" ]]; then
            idx="$new_idx"
            clear_bump_menu
            draw_bump_menu "$idx" "$patch_tag" "$minor_tag" "$major_tag" "$rc_tag"
        else
            continue
        fi
    done
}

next_tag_for_bump() {
    local bump="$1"
    local last_tag="$2"
    local prefix="${last_tag%%[0-9]*}"
    local version="${last_tag#"$prefix"}"

    # Strip any existing -rc.N suffix for base version parsing
    local base_version="${version%%-rc.*}"
    local major=0 minor=0 patch=0

    IFS='.' read -r major minor patch _ <<<"$base_version"
    major=${major:-0}
    minor=${minor:-0}
    patch=${patch:-0}

    case "$bump" in
        # A patch bump from an rc releases that rc's version (v0.1.0-rc.2 -> v0.1.0)
        patch) [[ "$version" == *-rc.* ]] || patch=$((patch + 1)) ;;
        minor) minor=$((minor + 1)); patch=0 ;;
        major) major=$((major + 1)); minor=0; patch=0 ;;
        rc)
            # If already an rc, bump the rc number; otherwise create rc1 of next patch
            if [[ "$version" == *-rc.* ]]; then
                local rc_num="${version##*-rc.}"
                rc_num=$((rc_num + 1))
                echo "${prefix}${major}.${minor}.${patch}-rc.${rc_num}"
                return 0
            else
                patch=$((patch + 1))
                echo "${prefix}${major}.${minor}.${patch}-rc.1"
                return 0
            fi
            ;;
        *) echo "Unknown bump type: $bump" >&2; return 1 ;;
    esac

    echo "${prefix}${major}.${minor}.${patch}"
}

if ! BUMP_TYPE=$(select_bump "${LAST_TAG}"); then
    exit 1
fi
NEXT_TAG=$(next_tag_for_bump "${BUMP_TYPE}" "${LAST_TAG}")
echo ""
printf "  %s -> " "${LAST_TAG}"
highlight_version "${BUMP_TYPE}" "${NEXT_TAG}"
printf "\n\n"

confirm() {
    # call with a prompt string or use a default
    read -r -p "${1:-Are you sure? [y/N]} " response
    case "$response" in
        [yY][eE][sS]|[yY])
            true
            ;;
        *)
            false
            ;;
    esac
}
PRERELEASE_FLAG=""
if [[ "$NEXT_TAG" == *-rc.* ]]; then
    PRERELEASE_FLAG="--prerelease"
fi
if confirm "Submit to GitHub (y/N)?"; then
    echo "Submitting..."
    gh release create "${NEXT_TAG}" --generate-notes ${PRERELEASE_FLAG}
else
    echo "Aborted."
fi
