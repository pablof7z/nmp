Feature: Native applications select one exact NMP capability set

  A Swift or Kotlin application checks in one feature declaration and prepares
  one native NMP library from it. Adding another protocol family changes data,
  not the build procedure, and NMP never publishes every possible combination.

  Rule: One declaration drives the native library and every projected surface

    # nmp:id=MODULES-NATIVE-SELECTION-001
    # nmp:status=built
    # nmp:evidence=script:repository::scripts/check-native-feature-matrix.sh
    # nmp:evidence=rust:nmp-cli::capability_language_resolves_to_catalog_keys_without_builder_branches
    # nmp:falsifier=Put a feature key or dependency edge in builder code instead of the catalog and Cargo; the genericity and dependency-activation proofs must fail.
    Scenario: An application prepares an arbitrary set of protocol families
      Given an application feature declaration selecting NIP-29 and NIP-65
      When the application runs the generic native preparation command
      Then one native NMP library contains NIP-29 and NIP-65
      And its generated UniFFI and native SDK surfaces contain NIP-29 and NIP-65
      And selecting a different family would use the same command and build machinery

    # nmp:id=MODULES-NATIVE-SELECTION-002
    # nmp:status=built
    # nmp:evidence=script:repository::scripts/check-native-feature-matrix.sh
    # nmp:evidence=rust:nmp-cli::manifest_is_canonical_and_runtime_fields_are_refused
    # nmp:falsifier=Materialize an unselected family source or let core resolve an optional Cargo feature; the exact-output proofs must fail.
    Scenario: An unselected protocol family is absent rather than disabled at runtime
      Given an application feature declaration that does not select NIP-29
      When the application prepares its native NMP library
      Then the native library contains no NIP-29 protocol dependency or symbols
      And the generated UniFFI, Swift, and Kotlin surfaces contain no NIP-29 API
      And application code naming the NIP-29 API does not compile

  Rule: Build selection and runtime operator configuration remain distinct

    # nmp:id=MODULES-NATIVE-SELECTION-003
    # nmp:status=built
    # nmp:evidence=script:repository::scripts/check-native-feature-matrix.sh
    # nmp:evidence=rust:nmp-cli::outbox_routing_native_surface_is_hard_cut_and_feature_gated
    # nmp:falsifier=Accept an empty configured indexer set, inject a hidden indexer, or accept providerless Auto into custody; at least one runtime proof must fail.
    Scenario: Selecting outbox routing does not choose an application's indexers
      Given an application feature declaration selecting outbox routing
      When the application supplies an empty outbox-routing indexer configuration
      Then engine construction is refused
      And when the application omits outbox routing an automatic write is refused before acceptance
      And NMP supplies no hidden indexer relay

    # nmp:id=MODULES-NATIVE-SELECTION-004
    # nmp:status=built
    # nmp:evidence=script:repository::scripts/check-native-feature-matrix.sh
    # nmp:evidence=rust:nmp-cli::source_filter_keeps_only_selected_capability_blocks
    # nmp:evidence=rust:nmp-cli::outbox_routing_native_surface_is_hard_cut_and_feature_gated
    # nmp:falsifier=Leave Auto or outbox-routing configuration in either generated core SDK, or accept Auto through core FFI; the feature-off source and compile proofs must fail.
    Scenario: A native build without outbox routing cannot request automatic routing
      Given an application feature declaration that does not select outbox routing
      When the application prepares its native NMP library
      Then the generated Swift and Kotlin write-routing surfaces expose only explicit routing
      And no automatic write can enter durable custody through the native boundary

    # nmp:id=MODULES-NATIVE-SELECTION-006
    # nmp:status=built
    # nmp:evidence=script:repository::scripts/check-native-outbox-routing-runtime.sh
    # nmp:falsifier=Ignore the configured indexer, query kind 10002 from another relay, contact a relay not learned from the current relay list, or implement discovery in either native consumer; the controlled witness fails.
    Scenario: Prepared native products discover a cold author outbox
      Given prepared Swift and Kotlin products with outbox routing selected
      And the application configures one exact indexer holding an author's relay list
      And no author route is cached
      When each product publishes an automatically routed event
      Then the configured indexer receives the author-scoped kind 10002 request
      And only the relay learned from that response receives the event
      And no undeclared relay is contacted

  Rule: Android changes packaging, not feature selection

    # nmp:id=MODULES-NATIVE-SELECTION-005
    # nmp:status=built
    # nmp:evidence=script:repository::scripts/check-android-feature-matrix.sh
    # nmp:falsifier=Remove one ABI, mismatch the generated binding, materialize an unselected wrapper, or diverge the desktop and Android feature inventories; the Android matrix must fail.
    Scenario: One selected Kotlin surface becomes one Android AAR
      Given an application feature declaration
      When the application prepares Kotlin JVM and Android outputs
      Then Cargo resolves the same feature set for both outputs
      And both outputs contain the same selected feature wrapper inventory
      And the Android output contains one matching NMP library for every declared ABI
      And a clean Android application consumes only com.nmp.sdk from the generated repository
