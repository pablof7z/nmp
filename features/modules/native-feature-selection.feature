Feature: Native applications select one exact NMP capability set

  A Swift or Kotlin application checks in one feature declaration and prepares
  one native NMP library from it. Adding another protocol family changes data,
  not the build procedure, and NMP never publishes every possible combination.

  Rule: One declaration drives the native library and every projected surface

    Scenario: An application prepares an arbitrary set of protocol families
      Given an application feature declaration selecting NIP-29 and NIP-65
      When the application runs the generic native preparation command
      Then one native NMP library contains NIP-29 and NIP-65
      And its generated UniFFI and native SDK surfaces contain NIP-29 and NIP-65
      And selecting a different family would use the same command and build machinery

    Scenario: An unselected protocol family is absent rather than disabled at runtime
      Given an application feature declaration that does not select NIP-29
      When the application prepares its native NMP library
      Then the native library contains no NIP-29 protocol dependency or symbols
      And the generated UniFFI, Swift, and Kotlin surfaces contain no NIP-29 API
      And application code naming the NIP-29 API does not compile

  Rule: Build selection and runtime operator configuration remain distinct

    Scenario: Selecting outbox routing does not choose an application's indexers
      Given an application feature declaration selecting outbox routing
      When the application supplies an empty outbox-routing indexer configuration
      Then engine construction is refused
      And when the application omits outbox routing an automatic write is refused before acceptance
      And NMP supplies no hidden indexer relay

    Scenario: A native build without outbox routing cannot request automatic routing
      Given an application feature declaration that does not select outbox routing
      When the application prepares its native NMP library
      Then the generated Swift and Kotlin write-routing surfaces expose only explicit routing
      And no automatic write can enter durable custody through the native boundary

    Scenario: Prepared native products discover a cold author outbox
      Given prepared Swift and Kotlin products with outbox routing selected
      And the application configures one exact indexer holding an author's relay list
      And no author route is cached
      When each product publishes an automatically routed event
      Then the configured indexer receives the author-scoped kind 10002 request
      And only the relay learned from that response receives the event
      And no undeclared relay is contacted

  Rule: Android changes packaging, not feature selection

    Scenario: One selected Kotlin surface becomes one Android AAR
      Given an application feature declaration
      When the application prepares Kotlin JVM and Android outputs
      Then Cargo resolves the same feature set for both outputs
      And both outputs contain the same selected feature wrapper inventory
      And the Android output contains one matching NMP library for every declared ABI
      And a clean Android application consumes only com.nmp.sdk from the generated repository
