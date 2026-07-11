Feature: What the cache says offline is the truth
  @ledger-7 @wip
  Scenario: A synced feed stays trustworthy with no network at all
    # Genuine gap, reported rather than faked green (approach doc Appendix
    # item 5): proving this for real needs the world to (1) run a first
    # engine session against an on-disk `nmp_store::RedbStore` long enough
    # to prove coverage, (2) cleanly shut it down, then (3) relaunch a
    # SECOND engine session against the SAME store file with every relay
    # unreachable, and assert the previously-synced rows still show with
    # coverage "complete as of the last sync". `NmpWorld::ensure_started`
    # only ever spawns one `MemoryStore`-backed session per scenario today
    # (see `nmp-engine/tests/integration_capstone.rs::
    # watermark_cold_start_offline` for the headless proof this scenario
    # would re-express) -- persisted-store restart is real and valuable,
    # and not yet wired into this world.
    Given my feed fully synced yesterday and the app was closed
    When I relaunch the app with no network
    Then my feed shows the previously synced notes immediately
    And the query reports its results are complete as of the last sync
