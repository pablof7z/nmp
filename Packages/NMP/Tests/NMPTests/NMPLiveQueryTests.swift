// #1189: `NMPLiveQuery`'s identity is the CANONICAL branch set, exactly as it
// is in Rust -- not the order an app happened to type the branches in, and not
// a list a duplicate can hide in until the boundary silently drops it.
//
// #1108 required this and nothing named `LiveQuery` was ever tested natively,
// which is how an ordered-list value type with public memberwise construction
// shipped on both SDKs. These tests are that missing proof. No network: every
// assertion here is about declaration, so the one observation opened below
// declares a cache-only branch and reads its first delivered evidence.

import XCTest
@testable import NMP

final class NMPLiveQueryTests: XCTestCase {
    private func branch(
        _ relay: String,
        freshness: NMPFreshness = .live
    ) -> NMPDemand {
        NMPDemand(
            selection: NMPFilter(kinds: [1]),
            routing: .explicit([relay]),
            freshness: freshness
        )
    }

    /// The same two branches typed in either order are ONE query. An app that
    /// memoizes or diffs on the declaration must not reopen an observation NMP
    /// considers unchanged.
    func testDeclarationOrderDoesNotChangeIdentity() throws {
        let a = NMPLiveQuery.single(branch("wss://a.example.com"))
        let b = NMPLiveQuery.single(branch("wss://b.example.com"))

        let oneWay = try NMPLiveQuery.union([a, b])
        let otherWay = try NMPLiveQuery.union([b, a])

        XCTAssertEqual(oneWay, otherWay)
        XCTAssertEqual(oneWay.hashValue, otherWay.hashValue)
        XCTAssertEqual(oneWay.branches, otherWay.branches)
        XCTAssertEqual(oneWay.branches.count, 2)
    }

    /// A branch declared twice owns one branch -- as it does in Rust, where it
    /// also owns one evidence entry and one refcount claim.
    func testDuplicateBranchAppearsOnce() throws {
        let a = NMPLiveQuery.single(branch("wss://a.example.com"))

        let query = try NMPLiveQuery.union([a, a])

        XCTAssertEqual(query.branches.count, 1)
        XCTAssertEqual(query, a)
    }

    /// Nested input flattens rather than nesting, so an ergonomically grouped
    /// declaration is the same value as a flat one.
    func testNestedInputFlattensIntoOneCanonicalSet() throws {
        let a = NMPLiveQuery.single(branch("wss://a.example.com"))
        let b = NMPLiveQuery.single(branch("wss://b.example.com"))
        let c = NMPLiveQuery.single(branch("wss://c.example.com"))

        let flat = try NMPLiveQuery.union([a, b, c])
        let nested = try NMPLiveQuery.union([c, NMPLiveQuery.union([b, a, b])])

        XCTAssertEqual(flat, nested)
        XCTAssertEqual(flat.branches.count, 3)
    }

    /// The aggregate bound is part of the value, and a single branch never
    /// carries one.
    func testTheAggregateBoundIsPartOfTheValue() throws {
        let a = NMPLiveQuery.single(branch("wss://a.example.com"))

        XCTAssertNil(a.aggregateResultLimit)
        XCTAssertEqual(try NMPLiveQuery.union([a], aggregateResultLimit: 7).aggregateResultLimit, 7)
        XCTAssertNotEqual(try NMPLiveQuery.union([a], aggregateResultLimit: 7), a)
    }

    /// The count the app reads off its own declaration is the count of
    /// evidence entries the observation delivers. A duplicate that survived
    /// locally would make these disagree.
    func testBranchCountMatchesDeliveredEvidenceCount() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        let a = NMPLiveQuery.single(branch("wss://a.example.com", freshness: .cacheOnly))
        let b = NMPLiveQuery.single(branch("wss://b.example.com", freshness: .cacheOnly))
        let query = try NMPLiveQuery.union([a, b, a])

        let observation = try engine.observe(query)
        defer { observation.cancel() }

        var delivered: RowBatch?
        for try await batch in observation {
            delivered = batch
            break
        }

        XCTAssertEqual(query.branches.count, 2)
        XCTAssertEqual(delivered?.evidence.count, query.branches.count)
    }

    /// Each unobservable declaration is refused as its own typed error, at
    /// construction -- before a handle, a graph claim or a wire request exists.
    func testEveryRefusalIsItsOwnTypedError() throws {
        let a = NMPLiveQuery.single(branch("wss://a.example.com"))

        XCTAssertThrowsError(try NMPLiveQuery.union([])) { error in
            XCTAssertEqual(error as? NMPError, .emptyQueryUnion)
        }
        XCTAssertThrowsError(try NMPLiveQuery.union([a], aggregateResultLimit: 0)) { error in
            XCTAssertEqual(error as? NMPError, .aggregateResultLimitZero)
        }
        let bounded = try NMPLiveQuery.union([a], aggregateResultLimit: 3)
        XCTAssertThrowsError(try NMPLiveQuery.union([bounded])) { error in
            XCTAssertEqual(error as? NMPError, .nestedAggregateResultLimit)
        }

        let ceiling = Int(NMPLiveQuery.maxBranches)
        let overCap = (0...ceiling).map {
            NMPLiveQuery.single(branch("wss://relay-\($0).example.com"))
        }
        XCTAssertThrowsError(try NMPLiveQuery.union(overCap)) { error in
            XCTAssertEqual(
                error as? NMPError,
                .tooManyQueryBranches(requested: UInt64(ceiling + 1), maximum: UInt64(ceiling))
            )
        }
    }
}
