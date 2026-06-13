#!/usr/bin/env sh
# Provide git credentials to release-plz's temporary clones.
#
# actions/checkout@v6 scopes its credentials to the workspace gitdir via
# includeIf configuration, which release-plz's temp-directory copy of the
# repository does not inherit. Without this, release-plz's `git push` fails
# with "could not read Username for 'https://...'".
#
# Source this script (`. ./.forgejo/scripts/configure-release-plz-git-auth.sh`)
# in the same step that runs release-plz so the exported GIT_ASKPASS applies.

set -eu

if [ -z "${GIT_TOKEN:-}" ]; then
  echo "GIT_TOKEN must be set before configuring release-plz git auth" >&2
  exit 1
fi

askpass_path="${RUNNER_TEMP:-/tmp}/release-plz-git-askpass"
mkdir -p "$(dirname "$askpass_path")"

cat > "$askpass_path" <<'SH'
#!/usr/bin/env sh

case "$1" in
  *Username*)
    printf '%s\n' "${RELEASE_PLZ_GIT_USERNAME:-jwilger}"
    ;;
  *Password*)
    printf '%s\n' "$GIT_TOKEN"
    ;;
  *)
    printf '\n'
    ;;
esac
SH

chmod 700 "$askpass_path"

export GIT_ASKPASS="$askpass_path"
export GIT_TERMINAL_PROMPT=0
