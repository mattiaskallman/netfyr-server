#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)

for expected in \
  'FIRST RUN — administrator account created' \
  'username: admin' \
  'one-time password: {password}' \
  'Sign in and change the password immediately.'
do
  grep -Fq "$expected" "$root/src/main.rs" || {
    echo "missing English first-run log text: $expected" >&2
    exit 1
  }
done

for file in \
  "$root/deploy/install.sh" \
  "$root/deploy/install-package.sh" \
  "$root/README.md"
do
  grep -Fq "FIRST RUN" "$file" || {
    echo "password retrieval command does not use FIRST RUN: $file" >&2
    exit 1
  }
done

if grep -R -n -E 'FÖRSTA KÖRNINGEN|administratörskonto skapat|användarnamn:|engångslösenord:|Logga in och byt lösenordet' \
  "$root/src/main.rs" "$root/deploy/install.sh" "$root/deploy/install-package.sh" "$root/README.md"
then
  echo "Swedish first-run bootstrap text remains in public installation paths" >&2
  exit 1
fi

echo "English first-run bootstrap regression test: OK"
