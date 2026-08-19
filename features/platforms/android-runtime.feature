Feature: A core-only Android product runs as an ordinary application

  The Android package is useful only if a clean external app can load it,
  observe real Nostr data through the supported facade, survive a process
  restart, and release every native owner without blocking the UI thread.

  Rule: Package success means supported-facade runtime success

    Scenario: A clean Android app observes and withdraws controlled relay data
      Given a core-only product prepared from the app's committed .nmp.toml
      And its AAR is consumed only from the generated Maven repository
      And a controlled host relay reachable through the emulator host alias
      When an ordinary Activity opens a pinned live observation through com.nmp.sdk
      Then the app receives the relay's valid signed event and scoped evidence
      And collection cancellation sends the owned withdrawal
      And closing the engine releases its store and returns typed EngineClosed

    Scenario: App-private cached rows reopen in a fresh process
      Given the first app process persisted a controlled relay row under noBackupFilesDir
      When that process exits and a different process opens the same store offline
      Then a CacheOnly observation returns the exact persisted row
      And no relay completeness or global synchronized state is invented

  Rule: Failure is scoped evidence and recovery is observable

    Scenario: A never-connected relay reports Error before it recovers
      Given a required relay refuses a fixed number of WebSocket handshakes
      When its live worker remains responsible for retrying
      Then the exact query source changes from Connecting to Error
      And a later connection clears that failure and delivers the real row
      And an unrelated permanently-offline source remains a scoped Error only

  Rule: Android adoption stays bounded under realistic collector load

    Scenario: Sixty-four collectors do not multiply native execution owners
      Given one engine and sixty-four cold Flow collectors on API 35
      When the collectors remain idle for 120 display frames and then cancel
      Then they add no more than four threads compared with one collector
      And process CPU, main-dispatch p99, cache p95, and native heap stay within the issue contract
      And teardown returns thread and native-heap usage to the declared bounds
