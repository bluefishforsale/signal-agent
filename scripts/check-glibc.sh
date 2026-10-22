#!/usr/bin/env bash
# Proves a Linux binary runs on the oldest glibc we promise (PRD goal 1), by
# reading the newest versioned glibc symbol it imports.
set -euo pipefail

bin=$1
max=$2

need=$(objdump -T "$bin" | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' | sort -uV | tail -n1)
[[ -n "$need" ]] || { echo "::error::$bin imports no versioned glibc symbols; is it dynamically linked?"; exit 1; }

echo "$bin needs glibc $need; the floor is $max"
if [[ "$(printf '%s\n%s\n' "$need" "$max" | sort -V | tail -n1)" != "$max" ]]; then
  echo "::error::$bin needs glibc $need, newer than the promised $max"
  exit 1
fi
