# Shared fail-closed prerequisite check for repository verification scripts.
#
# Keep this file Bash-builtins-only: callers source it before invoking any
# external command, so it must still be able to report the missing tool when
# PATH contains no executables at all.

require_commands() {
  local command_name
  local missing=()

  for command_name in "$@"; do
    if ! command -v "$command_name" >/dev/null 2>&1; then
      missing+=("$command_name")
    fi
  done

  if ((${#missing[@]} > 0)); then
    printf 'check-tools: required command(s) unavailable:' >&2
    printf ' %s' "${missing[@]}" >&2
    printf '\n' >&2
    return 127
  fi
}
