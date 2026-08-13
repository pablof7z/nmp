# Tracked-file enumeration for repository gates (#1178).
#
# A gate that WALKS A DIRECTORY judges whatever the developer's working tree
# happens to hold. `Packages/` holds gitignored uniffi output --
# `Packages/NMP/Sources/NMPFFI/nmp_ffi.swift` and
# `Packages/NMPKotlin/src/main/kotlin/uniffi/nmp_ffi/nmp_ffi.kt` -- so
# `scripts/check-nip29-ownership.sh` failed against a STALE GENERATED BINDING
# and reported it as a tombstone violation, which reads as "someone resurrected
# a deleted spelling", with the offending text present in no tracked file and
# in no commit. #1033 lost two BDD scenarios to exactly that. It cannot
# reproduce in CI, where the checkout is clean, so the whole cost lands on
# local debugging and what it teaches is to distrust a correct gate.
#
# The corpus here is the one a clean checkout would have.
#
# Sourced, never executed. Callers must already have `git`, `grep` and `awk`
# (`scripts/lib/require-commands.sh`) and must call `tracked_paths` at TOP
# LEVEL: inside `$(...)` a failed enumeration would exit nothing but the
# subshell and would read to the caller as "no violations found".
#
# Tracked regular files under each pathspec, collected into TRACKED_PATHS.
#
#   -s -z        NUL-terminated and carrying the index mode, so a path holding
#                a space or a newline survives intact and a non-regular entry
#                is filtered by what it IS rather than by what its name reads
#                like.
#   100644/755   regular files only. A tracked SYMLINK (mode 120000) would make
#                grep read through the link -- a duplicate hit, or an error if
#                the link dangles -- and a SUBMODULE gitlink (mode 160000)
#                names a directory grep cannot open. Neither holds source of
#                its own, so excluding them costs no coverage: a symlink's
#                target, when it is in the corpus, is scanned under its own
#                name.
#   --full-name  answers relative to the repository top level, which the root
#                argument is required to BE -- otherwise a pathspec resolved
#                against the caller's directory and a path reported against the
#                top level would silently disagree.
#
# Each pathspec must match at least one tracked regular file. A corpus narrowed
# until it sees nothing is strictly worse than a flaky gate: it goes green by
# scanning air.
tracked_paths() {
  local root=$1
  shift

  local top physical_root physical_top
  if ! top=$(git -C "$root" rev-parse --show-toplevel 2>/dev/null); then
    printf 'tracked-corpus: %s is not inside a git repository, so the tracked corpus cannot be read\n' \
      "$root" >&2
    return 1
  fi
  physical_root=$(cd "$root" && pwd -P)
  physical_top=$(cd "$top" && pwd -P)
  if [[ $physical_root != "$physical_top" ]]; then
    printf 'tracked-corpus: %s is not the repository top level (%s); pathspecs and reported paths would disagree\n' \
      "$root" "$top" >&2
    return 1
  fi

  local pathspec entry mode path matched
  TRACKED_PATHS=()
  for pathspec in "$@"; do
    matched=0
    while IFS= read -r -d '' entry; do
      mode=${entry%% *}
      path=${entry#*$'\t'}
      case $mode in
      100644 | 100755)
        TRACKED_PATHS+=("$path")
        matched=1
        ;;
      esac
    done < <(git -C "$root" ls-files -s -z --full-name -- "$pathspec")
    if ((matched == 0)); then
      printf 'tracked-corpus: no tracked regular file matches %s under %s; a scan over it would be vacuous\n' \
        "$pathspec" "$root" >&2
      return 1
    fi
  done
}

# `census <root> <ere> <path>...` prints every match as `path:line:text`.
#
# Callers test the captured OUTPUT, never the exit status: the corpus is
# scanned in batches and "this batch matched nothing" is grep's exit 1.
census() {
  local root=$1 pattern=$2
  shift 2

  local path
  local -a batch=()
  for path in "$@"; do
    if [[ -f $root/$path ]]; then
      batch+=("$path")
      if ((${#batch[@]} == 256)); then
        census_working_tree "$root" "$pattern" "${batch[@]}"
        batch=()
      fi
    else
      census_index "$root" "$pattern" "$path"
    fi
  done
  ((${#batch[@]} == 0)) || census_working_tree "$root" "$pattern" "${batch[@]}"
  return 0
}

# `-H` is not optional: the last batch can hold exactly one file, and grep
# omits the name when it is given exactly one -- the report would then say
# `42:...` and name nothing. `--` guards a path that begins with a dash.
#
# A grep that genuinely FAILED (status above 1) is reported AS OUTPUT rather
# than swallowed, so a caller that treats a non-empty result as a violation
# fails closed. `xargs` is avoided for the same reason its "some invocation
# exited 1" status is unusable: every batch's status is read here directly.
census_working_tree() {
  local root=$1 pattern=$2
  shift 2

  local out status=0
  out=$(cd "$root" && grep -IHnE -e "$pattern" -- "$@") || status=$?
  if ((status > 1)); then
    printf 'tracked-corpus: grep exited %s while scanning %s\n' "$status" "$1"
    return 0
  fi
  [[ -z $out ]] || printf '%s\n' "$out"
  return 0
}

# Tracked, but absent from the working tree -- deleted without `git rm`. A
# clean checkout HAS this file, so its index content is what the gate must
# judge; walking the working tree would have skipped it and greping it there
# would have errored. `awk` re-attaches the real path so the report names the
# file a clean checkout would have rather than a stream.
census_index() {
  local root=$1 pattern=$2 path=$3

  local out
  out=$(git -C "$root" show ":$path" 2>/dev/null |
    grep -InE -e "$pattern" |
    awk -v tracked="$path" '{ print tracked ":" $0 }') || true
  [[ -z $out ]] || printf '%s\n' "$out"
  return 0
}
