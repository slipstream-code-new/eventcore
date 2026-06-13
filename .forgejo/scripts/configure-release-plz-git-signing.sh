#!/usr/bin/env sh
# Copyright 2026 John Wilger

set -eu

if [ -z "${RELEASE_SIGNING_KEY:-}" ]; then
  echo "RELEASE_SIGNING_KEY must be set before configuring release-plz git signing" >&2
  exit 1
fi

signing_dir="${RUNNER_TEMP:-/tmp}/release-plz-signing"
mkdir -p "$signing_dir"
chmod 700 "$signing_dir"

GIT_CONFIG_GLOBAL="${GIT_CONFIG_GLOBAL:-$signing_dir/gitconfig}"
export GIT_CONFIG_GLOBAL

signing_key_path="$signing_dir/release-signing-key"
printf '%s\n' "$RELEASE_SIGNING_KEY" | sed 's/\\n/\
/g' > "$signing_key_path"
chmod 600 "$signing_key_path"

configure_ssh_signing() {
  ssh_keygen="$(command -v ssh-keygen || true)"

  if [ -z "$ssh_keygen" ]; then
    echo "ssh-keygen must be available to configure SSH commit signing" >&2
    exit 1
  fi

  git config --global gpg.format ssh
  git config --global gpg.ssh.program "$ssh_keygen"
  git config --global user.signingkey "$signing_key_path"
  git config --global commit.gpgsign true
}

configure_gpg_signing() {
  gpg_bin="$(command -v gpg || true)"

  if [ -z "$gpg_bin" ]; then
    echo "gpg must be available to configure GPG commit signing" >&2
    exit 1
  fi

  export GNUPGHOME="$signing_dir/gnupg"
  mkdir -p "$GNUPGHOME"
  chmod 700 "$GNUPGHOME"

  "$gpg_bin" --batch --import "$signing_key_path"
  rm -f "$signing_key_path"

  signing_key="$("$gpg_bin" --batch --list-secret-keys --with-colons | awk -F: '$1 == "sec" { print $5; exit }')"

  if [ -z "$signing_key" ]; then
    echo "RELEASE_SIGNING_KEY did not contain an importable GPG secret key" >&2
    exit 1
  fi

  gpg_wrapper_path="$signing_dir/gpg-wrapper"
  {
    printf '%s\n' '#!/usr/bin/env sh'
    printf 'exec %s --batch --yes --pinentry-mode loopback "$@"\n' "$gpg_bin"
  } > "$gpg_wrapper_path"
  chmod 700 "$gpg_wrapper_path"

  git config --global gpg.format openpgp
  git config --global gpg.program "$gpg_wrapper_path"
  git config --global user.signingkey "$signing_key"
  git config --global commit.gpgsign true
}

if grep -q "BEGIN PGP PRIVATE KEY BLOCK" "$signing_key_path"; then
  configure_gpg_signing
elif grep -q "BEGIN OPENSSH PRIVATE KEY" "$signing_key_path" \
  || grep -q "BEGIN RSA PRIVATE KEY" "$signing_key_path" \
  || grep -q "BEGIN EC PRIVATE KEY" "$signing_key_path" \
  || grep -q "BEGIN PRIVATE KEY" "$signing_key_path"; then
  configure_ssh_signing
else
  echo "RELEASE_SIGNING_KEY must contain an SSH or GPG private signing key" >&2
  exit 1
fi
