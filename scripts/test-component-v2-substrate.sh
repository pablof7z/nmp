#!/usr/bin/env bash
# #952 structural falsifier for the core-anchored native component substrate.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

fail() {
  echo "component-v2-substrate: $*" >&2
  exit 1
}

[[ -f crates/nmp-component-interface/Cargo.toml ]] ||
  fail "the shared component interface package is missing"
[[ -f scripts/verify-component-manifests.py ]] ||
  fail "the exact component manifest-set verifier is missing"

if grep -qF 'nip46-provider-component' crates/nmp-ffi/Cargo.toml; then
  fail "the package-set-specific core feature still exists"
fi
if sed -n '/^\[dependencies\]/,/^\[/p' crates/nmp-nip46-ffi/Cargo.toml |
  grep -Eq '^(nmp|nmp-ffi)[[:space:]]*='; then
  fail "the optional NIP-46 artifact still links the core facade"
fi

grep -qF 'nmp-component-interface' crates/nmp-nip46-ffi/Cargo.toml ||
  fail "the optional NIP-46 artifact does not use the shared interface"
[[ $(grep -cF \
  'cargo:rustc-link-arg-cdylib=-Wl,--exclude-libs,ALL' \
  crates/nmp-nip46-ffi/build.rs) == 1 ]] ||
  fail "the optional ELF provider does not hide dependency-owned symbols at link time"
grep -qF 'plan-authoritative-callables' scripts/build-component-release.sh ||
  fail "the optional provider does not derive callable authority from its audited static archive"
grep -qF -- '--require-callables "$AUTHORITATIVE_CALLABLES"' \
  scripts/build-component-release.sh ||
  fail "the final provider witness does not require the audited callable set"
interface_source=crates/nmp-component-interface/src/signer.rs
core_source=crates/nmp-ffi/src/signer.rs
provider_source=crates/nmp-nip46-ffi/src/signer.rs

grep -qF 'pub struct FfiSignerAdapter' "$interface_source" ||
  fail "the shared take-once signer adapter is missing"
grep -qF 'pub fn new_signer_adapter(' "$interface_source" ||
  fail "the provider cannot prepare an opaque signer adapter"
grep -qF 'pub struct SignerAdapterRuntime' "$interface_source" ||
  fail "the core-minted contextual runtime capability is missing"
grep -qF 'let _entered = handle.enter();' "$interface_source" ||
  fail "provider futures are not entered in their linked Tokio context on every poll"
if grep -qE 'pub fn from_core|pub unsafe|core-owner|[Mm]ailbox|CoreSigner(Port|Lease)' \
  "$interface_source" "$core_source" "$provider_source"; then
  fail "deleted unsafe/mailbox authority or a public runtime minting door survives"
fi
[[ $(awk '{ total += gsub(/\.take_for_install\(\)/, "") } END { print total + 0 }' \
  "$core_source") == 1 ]] ||
  fail "the core must consume the provider adapter at exactly one installation site"
grep -qF 'pub(crate) fn install_signer_adapter(' "$core_source" ||
  fail "the core-owned adapter installation door is missing"
if grep -qE 'Handle::current[[:space:]]*\(|tokio::spawn[[:space:]]*\(|runtime::Builder::new' \
  "$provider_source"; then
  fail "the separately linked provider regained ambient or provider-owned runtime authority"
fi
grep -qF 'Arc<dyn nmp_signer::Nip46TaskRuntime>' "$provider_source" ||
  fail "provider child tasks do not use the core-minted contextual scheduler"

# Positive controls: each deleted design, if restored, must match the exact
# structural refusal above instead of relying on reviewer memory.
legacy_mailbox_mutation='pub unsafe fn assemble_core_signer_mailbox() -> FfiSignerMailbox'
ambient_runtime_mutation='let runtime = tokio::runtime::Handle::current(); tokio::spawn(async {})'
unsealed_elf_mutation='cargo:rustc-link-arg-cdylib=-Wl,--export-dynamic'
printf '%s\n' "$legacy_mailbox_mutation" |
  grep -qE 'pub unsafe|[Mm]ailbox' ||
  fail "legacy mailbox mutation positive control escaped"
printf '%s\n' "$ambient_runtime_mutation" |
  grep -qE 'Handle::current[[:space:]]*\(|tokio::spawn[[:space:]]*\(' ||
  fail "ambient runtime mutation positive control escaped"
if printf '%s\n' "$unsealed_elf_mutation" |
  grep -qF 'cargo:rustc-link-arg-cdylib=-Wl,--exclude-libs,ALL'; then
  fail "unsealed ELF mutation positive control escaped"
fi
grep -qF 'nmp-core-component-v2' crates/nmp-ffi/build.rs ||
  fail "the core identity is not v2"

for manifest in crates/nmp-ffi/Cargo.toml crates/nmp-nip46-ffi/Cargo.toml; do
  grep -qF '[package.metadata.nmp-component]' "$manifest" ||
    fail "$manifest has no generic component metadata"
done

if grep -qE 'PACKAGE_SET|PROVIDER_ONLY_CRATES|nmp-core-component-v1' \
  scripts/build-component-release.sh crates/nmp-ffi/build.rs; then
  fail "the v1 pair/package-set identity path still survives"
fi

run_elf_artifact_falsifiers() (
  [[ $(uname -s) == Linux ]] ||
    fail "ELF artifact falsifiers must execute on Linux"
  command -v rustup >/dev/null 2>&1 ||
    fail "rustup is required for the real ELF artifact falsifiers"
  rustup component add llvm-tools-preview >/dev/null

  local proof target core provider release_dir witness forbidden callables
  local missing_callables missing_output unsealed_output unsealed_status
  proof=$(mktemp -d "${TMPDIR:-/tmp}/nmp-component-elf-falsifier.XXXXXX")

  target=$(rustc -vV | sed -n 's/^host: //p')
  [[ $target == *-linux-* ]] ||
    fail "Rust host target is not Linux: $target"
  cargo fetch --locked >/dev/null

  core=$(
    scripts/build-component-release.sh "$proof/target" "$target" nmp-ffi
  )
  provider=$(
    scripts/build-component-release.sh "$proof/target" "$target" \
      --core-artifact "$core/libnmp_ffi.so" nmp-nip46-ffi
  )
  release_dir="$proof/target/nmp-component-build-v2/nmp-nip46/$target/release"
  witness="$proof/target/nmp-component-artifact-witness-tool/release/nmp-component-artifact-witness"
  forbidden="$proof/target/nmp-component-build-v2/nmp-nip46/.nmp-component-build-v2/$target.interface-symbols.nul"
  callables="$proof/target/nmp-component-build-v2/nmp-nip46/.nmp-component-build-v2/$target.authoritative-callables.nul"
  [[ -f $release_dir/libnmp_nip46_ffi.a && -x $witness &&
    -f $forbidden && -f $callables ]] ||
    fail "the real sealed ELF build did not retain its audited proof inputs"

  "$witness" witness \
    --artifact "$provider/libnmp_nip46_ffi.so" \
    --target "$target" \
    --component-key nmp-nip46 \
    --attestation-symbol NMP_NIP46_COMPONENT_ATTESTATION_V2 \
    --forbid-symbols "$forbidden" \
    --require-callables "$callables" >/dev/null

  # The final dynamic metadata is not allowed to define its own completeness.
  # Add one independently required callable to the static-derived authority;
  # the unchanged real provider must be refused for the exact missing member.
  missing_callables="$proof/missing-authoritative-callable.nul"
  cp "$callables" "$missing_callables"
  chmod u+w "$missing_callables"
  printf '%s\0' ffi_nmp_nip46_ffi_deliberately_missing >>"$missing_callables"
  missing_output="$proof/missing-callable.out"
  if "$witness" witness \
    --artifact "$provider/libnmp_nip46_ffi.so" \
    --target "$target" \
    --component-key nmp-nip46 \
    --attestation-symbol NMP_NIP46_COMPONENT_ATTESTATION_V2 \
    --forbid-symbols "$forbidden" \
    --require-callables "$missing_callables" >"$missing_output" 2>&1; then
    fail "the real provider accepted a callable missing from dynamic metadata and exports"
  fi
  grep -qF 'missing=["ffi_nmp_nip46_ffi_deliberately_missing"]' \
    "$missing_output" ||
    fail "the missing-callable mutation failed for an unrelated reason"

  # Disable the one ELF link mechanism in this isolated tracked-tree copy.
  # Reusing the same target leaves the sealed core fixed and rebuilds the
  # actual provider; its final witness must now observe the interface leak.
  [[ $(grep -cF \
    'println!("cargo:rustc-link-arg-cdylib=-Wl,--exclude-libs,ALL");' \
    crates/nmp-nip46-ffi/build.rs) == 1 ]] ||
    fail "the isolated ELF mutation could not locate the exact link mechanism"
  perl -0pi -e \
    's/[[:space:]]*println!\(\"cargo:rustc-link-arg-cdylib=-Wl,--exclude-libs,ALL\"\);//' \
    crates/nmp-nip46-ffi/build.rs
  if grep -qF 'cargo:rustc-link-arg-cdylib=-Wl,--exclude-libs,ALL' \
    crates/nmp-nip46-ffi/build.rs; then
    fail "the isolated ELF mutation did not disable the link mechanism"
  fi
  unsealed_output="$proof/unsealed-provider.out"
  set +e
  scripts/build-component-release.sh "$proof/target" "$target" \
    --core-artifact "$core/libnmp_ffi.so" nmp-nip46-ffi \
    >"$unsealed_output" 2>&1
  unsealed_status=$?
  set -e
  [[ $unsealed_status -ne 0 ]] ||
    fail "the actual unsealed ELF provider passed its final artifact witness"
  grep -qF 'artifact still publicly defines forbidden exact symbols' \
    "$unsealed_output" ||
    fail "the unsealed ELF mutation failed before the final symbol-leak assertion"

  echo "component-v2-substrate: real sealed ELF provider passed; missing-callable and linker-disable mutations were refused"
  chmod -R u+w "$proof" 2>/dev/null || true
  rm -r "$proof"
)

run_elf_falsifiers_in_isolated_copy() (
  local isolated started status temporary_root
  if [[ $(uname -s) == Darwin ]]; then
    # Keep the copy beside the checked-out worktree: Docker Desktop already
    # shares this /Users path, while a host temporary directory may not be
    # visible inside its Linux VM.
    temporary_root=${ROOT%/*}
  else
    temporary_root=${TMPDIR:-/tmp}
  fi
  isolated=$(mktemp -d "$temporary_root/nmp-component-elf-copy.XXXXXX")
  mkdir "$isolated/repo"
  git ls-files -z |
    tar --null -T - -cf - |
    tar -xf - -C "$isolated/repo"
  started=$SECONDS
  set +e
  case "$(uname -s)" in
    Linux)
      (
        cd "$isolated/repo"
        NMP_COMPONENT_ELF_FALSIFIER_INNER=1 \
          bash scripts/test-component-v2-substrate.sh
      )
      ;;
    Darwin)
      command -v docker >/dev/null 2>&1 ||
        fail "Docker is required to run the real ELF artifact falsifiers on macOS"
      docker run --rm \
        --mount "type=bind,src=$isolated,dst=/work" \
        rustlang/rust:nightly \
        bash -c '
          set -euo pipefail
          cd /work/repo
          NMP_COMPONENT_ELF_FALSIFIER_INNER=1 \
            bash scripts/test-component-v2-substrate.sh
        '
      ;;
    *)
      fail "real ELF artifact falsifiers support Linux directly or Docker on macOS"
      ;;
  esac
  status=$?
  set -e
  chmod -R u+w "$isolated" 2>/dev/null || true
  rm -r "$isolated"
  [[ $status -eq 0 ]] || return "$status"
  echo "component-v2-substrate: ELF artifact falsifiers completed in $((SECONDS - started))s"
)

# The core and the provider are independent Cargo resolutions under separate
# target directories, so each links its OWN compilation of Tokio. The shared
# interface moves a `tokio::runtime::Handle` by value and `tokio::sync`
# channels across that seam, so the two compilations must be the same one.
#
# No identity can see this: every component identity is computed from an
# isolated unit graph of `nmp-component-interface` alone, which is invariant
# to how Tokio feature-unifies inside `nmp-ffi` versus `nmp-nip46-ffi`.
# `interface_dependency_digest` is the only value derived from the resolution
# the consuming build actually performs.
#
# Mutation: give the core graph one Tokio feature the provider graph cannot
# have (`nmp-ffi` is forbidden in the provider's own graph). `fs` adds no
# package, so `Cargo.lock` stays exact and `--frozen` still holds; the only
# thing that moves is Tokio's resolved feature set on one side of the seam.
run_interface_dependency_falsifier() (
  local isolated repo output started
  local core_tokio='tokio = { version = "1", features = ["rt", "time", "net"] }'
  local diverged='tokio = { version = "1", features = ["rt", "time", "net", "fs"] }'
  local refusal='the shared component interface resolved differently in this provider'

  isolated=$(mktemp -d "${TMPDIR:-/tmp}/nmp-interface-dependency-falsifier.XXXXXX")
  repo="$isolated/repo"
  mkdir "$repo"
  git ls-files -z | tar --null -T - -cf - | tar -xf - -C "$repo"
  started=$SECONDS

  [[ $(grep -cF "$core_tokio" "$repo/crates/nmp-ffi/Cargo.toml") == 1 ]] ||
    fail "the falsifier could not locate the core's exact Tokio dependency"
  perl -0pi -e "s/\Q$core_tokio\E/$diverged/" "$repo/crates/nmp-ffi/Cargo.toml"
  grep -qF "$diverged" "$repo/crates/nmp-ffi/Cargo.toml" ||
    fail "the falsifier did not diverge the core's Tokio feature set"

  output="$isolated/diverged.out"
  set +e
  (
    cd "$repo"
    CARGO_TARGET_DIR="$isolated/target" cargo check --frozen -p nmp-nip46-ffi
  ) >"$output" 2>&1
  local diverged_status=$?
  set -e
  [[ $diverged_status -ne 0 ]] ||
    fail "the provider accepted a core that resolved the shared interface's Tokio differently"
  grep -qF "$refusal" "$output" ||
    fail "the diverged-Tokio build failed for an unrelated reason: $(tail -20 "$output")"

  # Positive control in the same tree and the same target directory: restore
  # the one line and the identical command must build. Without this, a build
  # that refused for any reason at all would look like a passing falsifier.
  perl -0pi -e "s/\Q$diverged\E/$core_tokio/" "$repo/crates/nmp-ffi/Cargo.toml"
  grep -qF "$core_tokio" "$repo/crates/nmp-ffi/Cargo.toml" ||
    fail "the falsifier could not restore the core's Tokio dependency"
  output="$isolated/converged.out"
  set +e
  (
    cd "$repo"
    CARGO_TARGET_DIR="$isolated/target" cargo check --frozen -p nmp-nip46-ffi
  ) >"$output" 2>&1
  local converged_status=$?
  set -e
  [[ $converged_status -eq 0 ]] ||
    fail "the unmutated provider was refused: $(tail -20 "$output")"

  chmod -R u+w "$isolated" 2>/dev/null || true
  rm -r "$isolated"
  echo "component-v2-substrate: a core/provider Tokio feature divergence was refused, and the unmutated pair still builds ($((SECONDS - started))s)"
)

if [[ ${NMP_COMPONENT_ELF_FALSIFIER_INNER:-} == 1 ]]; then
  run_elf_artifact_falsifiers
else
  run_elf_falsifiers_in_isolated_copy
  run_interface_dependency_falsifier
  echo "component-v2-substrate: take-once adapter and contextual core runtime present"
fi
