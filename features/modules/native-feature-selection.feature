Feature: Native applications select one exact NMP capability set

  A Swift or Kotlin application checks in one feature declaration and prepares
  one native NMP library from it. Adding another protocol family changes data,
  not the build procedure, and NMP never publishes every possible combination.

  Rule: One declaration drives the native library and every projected surface

    # nmp:id=MODULES-NATIVE-SELECTION-001
    # nmp:status=built
    # nmp:evidence=script:repository::scripts/check-native-feature-matrix.sh
    # nmp:evidence=rust:nmp-ffi::native_feature_selection_tool_contract
    # nmp:falsifier=Put a feature key or dependency edge in builder code instead of the catalog and Cargo; the genericity and dependency-activation proofs must fail.
    # nmp:issue=#824
    Scenario: An application prepares an arbitrary set of protocol families
      Given an application feature declaration selecting NIP-29 and NIP-65
      When the application runs the generic native preparation command
      Then one native NMP library contains NIP-29 and NIP-65
      And its generated UniFFI and native SDK surfaces contain NIP-29 and NIP-65
      And selecting a different family would use the same command and build machinery

    # nmp:id=MODULES-NATIVE-SELECTION-002
    # nmp:status=built
    # nmp:evidence=script:repository::scripts/check-native-feature-matrix.sh
    # nmp:evidence=rust:nmp-ffi::native_feature_selection_tool_contract
    # nmp:falsifier=Materialize an unselected family source or let core resolve an optional Cargo feature; the exact-output proofs must fail.
    # nmp:issue=#824
    Scenario: An unselected protocol family is absent rather than disabled at runtime
      Given an application feature declaration that does not select NIP-29
      When the application prepares its native NMP library
      Then the native library contains no NIP-29 protocol dependency or symbols
      And the generated UniFFI, Swift, and Kotlin surfaces contain no NIP-29 API
      And application code naming the NIP-29 API does not compile

  Rule: Build selection and runtime operator configuration remain distinct

    # nmp:id=MODULES-NATIVE-SELECTION-003
    # nmp:status=built
    # nmp:evidence=rust:nmp-ffi::selected_nip65_refuses_an_empty_runtime_indexer_set
    # nmp:evidence=rust:nmp-ffi::providerless_auto_refuses_before_acceptance_and_leaves_no_residue
    # nmp:evidence=rust:nmp-ffi::selected_nip65_native_engine_discovers_and_publishes_to_the_cold_outbox
    # nmp:falsifier=Accept an empty configured indexer set, inject a hidden indexer, or accept providerless Auto into custody; at least one runtime proof must fail.
    # nmp:issue=#824
    Scenario: Selecting NIP-65 does not choose an application's indexers
      Given an application feature declaration selecting NIP-65
      When the application supplies an empty NIP-65 indexer configuration
      Then engine construction is refused
      And when the application omits the runtime provider an automatic write is refused before acceptance
      And NMP supplies no hidden indexer relay

    # nmp:id=MODULES-NATIVE-SELECTION-004
    # nmp:status=built
    # nmp:evidence=script:repository::scripts/check-native-feature-matrix.sh
    # nmp:evidence=rust:nmp-ffi::native_feature_selection_tool_contract
    # nmp:falsifier=Leave Auto in either generated core SDK or accept it through core FFI; the feature-off source and compile proofs must fail.
    # nmp:issue=#824
    Scenario: A native build without NIP-65 cannot request automatic routing
      Given an application feature declaration that does not select NIP-65
      When the application prepares its native NMP library
      Then the generated Swift and Kotlin write-routing surfaces expose only explicit routing
      And no automatic write can enter durable custody through the native boundary
