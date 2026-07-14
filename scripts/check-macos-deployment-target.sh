#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage:
  scripts/check-macos-deployment-target.sh --print-deployment-target
  scripts/check-macos-deployment-target.sh [--exact] ARTIFACT

Read the declared macOS minimum from Packages/NMP/Package.swift. Archive mode
rejects every Mach-O member whose encoded minimum is newer. --exact requires
the inspected Mach-O image itself to encode exactly the declared minimum.
USAGE
}

fail() {
  echo "error: $1" >&2
  exit 1
}

REPO_ROOT=$(git rev-parse --show-toplevel)
PACKAGE_MANIFEST="$REPO_ROOT/Packages/NMP/Package.swift"
DECLARED_TARGETS=$(
  sed -nE 's/^[[:space:]]*\.macOS\(\.v([0-9]+)\),?[[:space:]]*$/\1.0/p' \
    "$PACKAGE_MANIFEST"
)
DECLARED_TARGET_COUNT=$(printf '%s\n' "$DECLARED_TARGETS" | awk 'NF { count++ } END { print count + 0 }')
[[ $DECLARED_TARGET_COUNT -eq 1 ]] \
  || fail "expected exactly one macOS minimum in $PACKAGE_MANIFEST"
DECLARED_TARGET=$DECLARED_TARGETS

if [[ ${1:-} == --print-deployment-target ]]; then
  [[ $# -eq 1 ]] || fail "--print-deployment-target takes no artifact"
  printf '%s\n' "$DECLARED_TARGET"
  exit 0
fi

EXACT=0
if [[ ${1:-} == --exact ]]; then
  EXACT=1
  shift
fi
[[ $# -eq 1 ]] || {
  usage >&2
  exit 2
}

ARTIFACT=$1
[[ -f $ARTIFACT ]] || fail "artifact does not exist: $ARTIFACT"

otool -l "$ARTIFACT" | awk -v expected="$DECLARED_TARGET" -v exact="$EXACT" '
  function die(message) {
    print "error: " message > "/dev/stderr"
    failed = 1
  }

  function version_value(version, parts, count, position, value) {
    count = split(version, parts, ".")
    if (count < 1 || count > 3) {
      die("invalid Mach-O deployment target " version " in " unit)
      return -1
    }
    for (position = 1; position <= count; position++) {
      if (parts[position] !~ /^[0-9]+$/) {
        die("invalid Mach-O deployment target " version " in " unit)
        return -1
      }
    }
    value = parts[1] * 1000000
    if (count >= 2) value += parts[2] * 1000
    if (count >= 3) value += parts[3]
    return value
  }

  function finish_unit() {
    if (unit == "") return
    if (minimum_count != 1) {
      die(unit " encoded " minimum_count " macOS deployment targets; expected exactly one")
    }
  }

  /^Archive : / { next }

  /^[^[:space:]].*:[[:space:]]*$/ {
    finish_unit()
    unit = $0
    sub(/:[[:space:]]*$/, "", unit)
    minimum_count = 0
    command = ""
    platform = ""
    unit_count++
    next
  }

  $1 == "cmd" && $2 == "LC_BUILD_VERSION" {
    command = "build"
    platform = ""
    next
  }

  $1 == "cmd" && $2 == "LC_VERSION_MIN_MACOSX" {
    command = "legacy"
    next
  }

  command == "build" && $1 == "platform" {
    platform = $2
    next
  }

  command == "build" && $1 == "minos" {
    if (platform != "1") {
      die(unit " is not a macOS Mach-O image (platform " platform ")")
    }
    actual_value = version_value($2)
    expected_value = version_value(expected)
    if (actual_value > expected_value) {
      die(unit " requires macOS " $2 ", newer than declared " expected)
    }
    if (exact && actual_value != expected_value) {
      die(unit " encodes macOS " $2 ", expected exactly " expected)
    }
    minimum_count++
    command = ""
    next
  }

  command == "legacy" && $1 == "version" {
    actual_value = version_value($2)
    expected_value = version_value(expected)
    if (actual_value > expected_value) {
      die(unit " requires macOS " $2 ", newer than declared " expected)
    }
    if (exact && actual_value != expected_value) {
      die(unit " encodes macOS " $2 ", expected exactly " expected)
    }
    minimum_count++
    command = ""
    next
  }

  END {
    finish_unit()
    if (unit_count == 0) die("otool found no Mach-O images")
    if (failed) exit 1
    print "ok - " unit_count " Mach-O image(s) target macOS " expected " or older"
  }
'
