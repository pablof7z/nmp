# NMP's generated UniFFI bindings perform their own contract/checksum
# verification when the native library is first loaded. The qualification AAR
# is intentionally unminified; consumer shrinker behavior is a later release
# concern and must not weaken those checks.
