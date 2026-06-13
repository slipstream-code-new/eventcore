#!/usr/bin/env sh
# Copyright 2026 John Wilger

set -eu

if [ -z "${GIT_TOKEN:-}" ]; then
  echo "GIT_TOKEN must be set before creating the release PR" >&2
  exit 1
fi

if [ -z "${GITHUB_REPOSITORY:-}" ]; then
  echo "GITHUB_REPOSITORY must be set before creating the release PR" >&2
  exit 1
fi

server_url="${GITHUB_SERVER_URL:-https://git.johnwilger.com}"
repository_owner="${GITHUB_REPOSITORY%%/*}"
repository_name="${GITHUB_REPOSITORY#*/}"
base_branch="${GITHUB_REF_NAME:-main}"
api_url="$server_url/api/v1/repos/$repository_owner/$repository_name"
branch_prefix="${RELEASE_PLZ_PR_BRANCH_PREFIX:-release-plz-}"
pr_title_override="${RELEASE_PLZ_PR_TITLE:-}"

release-plz update --forge gitea

if [ -z "$(git status --porcelain)" ]; then
  echo "release-plz found no release PR changes"
  exit 0
fi

# Derive the release version from the manifest. Workspaces inherit a single
# `[workspace.package]` version (e.g. eventcore's lockstep version per
# ADR-025); single-crate repos carry it under `[package]`. Either way the
# first version in that section is the canonical identifier for the release.
version="$(
  awk '
    /^\[workspace\.package\]/ || /^\[package\]/ { in_v = 1; next }
    /^\[/                                       { in_v = 0 }
    in_v && /^version[[:space:]]*=/ {
      gsub(/[^0-9.]/, "", $0)
      print
      exit
    }
  ' Cargo.toml
)"

if [ -z "$version" ]; then
  echo "could not determine workspace release version from Cargo.toml" >&2
  exit 1
fi

# Conventional-commit release title that names the version. The
# `chore(release):` scope is matched by the Phase 1 loop-prevention guard and
# the Phase 2 publish trigger, and is skipped by changelog generation (see
# release-plz.toml). The auto_review "PR metadata quality" check rejects a
# bare `chore: release` title because it neither follows the release commit
# convention nor names a version.
pr_title="${pr_title_override:-chore(release): v$version}"

# Print the newest numbered version section from a CHANGELOG, skipping the
# persistent `## [Unreleased]` header.
new_changelog_section() {
  awk '
    /^## \[[0-9]/ { if (started) exit; started = 1 }
    started       { print }
  ' "$1"
}

# Build a body that explains the purpose of the release and embeds the new
# changelog sections for every crate that changed. The auto_review metadata
# check requires the body to explain why the release is necessary, not just
# that it exists.
pr_body="$(
  printf '## Release v%s\n\n' "$version"
  printf 'Automated release prepared by release-plz. All EventCore workspace crates are bumped to **v%s** in lockstep (shared major.minor per ADR-025) and their changelogs regenerated from the conventional commits merged to `main` since the previous release.\n\n' "$version"
  printf '### Changes\n\n'
  changed_changelogs="$(git status --porcelain | grep 'CHANGELOG\.md$' | awk '{ print $NF }')"
  if [ -n "$changed_changelogs" ]; then
    for changelog in $changed_changelogs; do
      crate="$(dirname "$changelog")"
      section="$(new_changelog_section "$changelog")"
      if [ -n "$section" ]; then
        if [ "$crate" = "." ]; then
          printf '%s\n\n' "$section"
        else
          printf '#### %s\n\n%s\n\n' "$crate" "$section"
        fi
      fi
    done
  else
    printf 'Version bump only; see individual crate changelogs for details.\n\n'
  fi
  printf '### Verification\n'
  printf -- '- `release-plz update --forge gitea`\n'
  printf -- '- signed release PR commit created by Forgejo Actions\n'
)"

open_pulls="$(
  curl -fsS \
    -H "Authorization: token $GIT_TOKEN" \
    "$api_url/pulls?state=open&limit=50"
)"

existing_release_pr="$(
  printf '%s' "$open_pulls" \
    | jq -r --arg prefix "$branch_prefix" 'map(select(.head.ref | startswith($prefix))) | sort_by(.number) | last | .number // empty'
)"

existing_branch=""
if [ -n "$existing_release_pr" ]; then
  existing_branch="$(
    printf '%s' "$open_pulls" \
      | jq -r --argjson number "$existing_release_pr" '.[] | select(.number == $number) | .head.ref'
  )"
fi

# release-plz rebuilds the release branch off the latest `main` each run, so
# updating an open release PR in place would require a force-push. Every
# branch is covered by the repository's `*` branch-protection rule, which
# blocks force-push (and deletion). Rather than rewriting the existing release
# branch, always open a fresh release branch and supersede the previous
# release PR. This is the only force-push-free way to keep the release PR
# rebased on a moving `main`.
release_branch="$branch_prefix$(date -u '+%Y-%m-%dT%H-%M-%SZ')"

git switch -c "$release_branch"
git add -A
git commit -m "$pr_title"

if ! git cat-file commit HEAD | grep -q '^gpgsig '; then
  echo "release PR commit was not signed" >&2
  exit 1
fi

# Avoid churn: if the open release PR already has identical content, leave it
# alone instead of replacing it with an equivalent PR on every push to `main`.
if [ -n "$existing_branch" ]; then
  git fetch --quiet origin "$existing_branch" 2>/dev/null || true
  if git rev-parse --verify -q "origin/$existing_branch" >/dev/null 2>&1 \
    && git diff --quiet "origin/$existing_branch" HEAD; then
    echo "release PR #$existing_release_pr already up to date; nothing to do"
    exit 0
  fi
fi

git push origin "HEAD:$release_branch"

created_pr="$(
  curl -fsS \
    -X POST \
    -H "Authorization: token $GIT_TOKEN" \
    -H "Content-Type: application/json" \
    --data "$(
      jq -n \
        --arg base "$base_branch" \
        --arg head "$release_branch" \
        --arg title "$pr_title" \
        --arg body "$pr_body" \
        '{base: $base, head: $head, title: $title, body: $body}'
    )" \
    "$api_url/pulls"
)"
new_release_pr="$(printf '%s' "$created_pr" | jq -r '.number')"
printf '%s' "$created_pr" | jq -r '"created release PR #\(.number): \(.html_url)"'

# Supersede the previous release PR. Its branch is protected from force-push
# and deletion, so close the PR (the stale branch is left behind harmlessly)
# and leave a pointer to the replacement.
if [ -n "$existing_release_pr" ] && [ "$existing_release_pr" != "$new_release_pr" ]; then
  curl -fsS \
    -X POST \
    -H "Authorization: token $GIT_TOKEN" \
    -H "Content-Type: application/json" \
    --data "$(jq -n --arg body "Superseded by #$new_release_pr (rebased on the latest \`main\`)." '{body: $body}')" \
    "$api_url/issues/$existing_release_pr/comments" >/dev/null || true

  curl -fsS \
    -X PATCH \
    -H "Authorization: token $GIT_TOKEN" \
    -H "Content-Type: application/json" \
    --data '{"state": "closed"}' \
    "$api_url/pulls/$existing_release_pr" >/dev/null
  echo "closed superseded release PR #$existing_release_pr"
fi
