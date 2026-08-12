Feature: Bounded inspection of durable write obligations

  The app can inspect retained write obligations without loading the complete
  queue, and can find every still-active obligation for one exact event id.

  Rule: Queue inspection is bounded, and a row reaches its own obligations

    # nmp:id=WRITES-QUEUE-001
    # nmp:status=built
    # nmp:evidence=rust:nmp::queue_pages_are_disjoint_and_no_call_can_return_more_than_u8_allows
    # nmp:evidence=rust:nmp::an_event_id_reaches_every_matching_active_receipt_before_and_after_restart
    # nmp:falsifier=Restore the zero-argument whole-queue snapshot, or make event lookup scan that snapshot and choose one matching receipt; a 300-entry backlog then crosses one public call, and two live obligations for identical event bytes either hide one receipt or require scanning unrelated history after restart.
    Scenario: A query row reaches every one of its live write obligations through bounded pages
      Given my publish queue holds more entries than one inspection call can return
      And two active obligations own the exact same event id
      When I inspect the queue in receipt-id pages
      Then no page contains more than the public limit
      And consecutive pages are disjoint
      When I inspect active obligations for that event id
      Then both receipt ids are returned without unrelated queue entries
      And the same exact lookup works after NMP reopens its durable store
      When one matching obligation becomes terminal
      Then exact lookup excludes it
      And the general retained queue still reports it until the app removes it
