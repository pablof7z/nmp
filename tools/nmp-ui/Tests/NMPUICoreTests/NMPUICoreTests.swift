import Foundation
import XCTest
@testable import NMPUICore

final class NMPUICoreTests: XCTestCase {
    private enum InjectedFailure: Error, Equatable {
        case mutation(ProjectFileSystem.MutationPoint)
    }

    private var temporaryDirectories: [URL] = []

    override func tearDownWithError() throws {
        for directory in temporaryDirectories { try? FileManager.default.removeItem(at: directory) }
        temporaryDirectories.removeAll()
    }

    func testBundledCatalogHasDeterministicDependencyClosure() throws {
        let installer = try NMPUIInstaller(catalog: .bundled(), projectRoot: temporaryDirectory())
        XCTAssertEqual(
            try installer.dependencyClosure(for: "article-medium-card").map(\.name),
            ["action-surface", "article-medium-card"]
        )
        XCTAssertEqual(installer.list().lines.map { $0.split(separator: "\t")[0] }, ["action-surface", "article-medium-card"])
    }

    func testCheckedInSampleFixtureMatchesRegistryAndLinksOnlyPublicProducts() throws {
        let packageRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
        let fixture = packageRoot.appendingPathComponent("Fixtures/SampleApp", isDirectory: true)
        let installer = try NMPUIInstaller(catalog: .bundled(), projectRoot: fixture)

        XCTAssertFalse(try installer.diff("action-surface").hasDifferences)
        XCTAssertFalse(try installer.diff("article-medium-card").hasDifferences)

        let lock = try ProjectFileSystem(root: fixture).loadLock(registryVersion: "2026.07.1")
        XCTAssertEqual(Set(lock.components.keys), ["action-surface", "article-medium-card"])
        XCTAssertEqual(lock.components["article-medium-card"]?.dependencies, ["action-surface"])

        let manifest = try String(contentsOf: fixture.appendingPathComponent("Package.swift"))
        XCTAssertTrue(manifest.contains(".product(name: \"NMPContent\", package: \"NMP\")"))
        XCTAssertTrue(manifest.contains(".product(name: \"NMPUI\", package: \"NMP\")"))
        XCTAssertFalse(manifest.contains("NMPFFI"))

        let installed = try FileManager.default.contentsOfDirectory(
            at: fixture.appendingPathComponent("Components/NMPUI"),
            includingPropertiesForKeys: nil
        ).map(\.lastPathComponent).sorted()
        XCTAssertEqual(installed, ["ActionSurface.swift", "ArticleMediumCard.swift"])
    }

    func testAddInstallsExactClosureAndInspectableLock() throws {
        let root = temporaryDirectory()
        let installer = try NMPUIInstaller(catalog: .bundled(), projectRoot: root)

        let report = try installer.add("article-medium-card")

        XCTAssertEqual(report.lines, ["installed action-surface@1.0.0", "installed article-medium-card@1.0.0"])
        XCTAssertTrue(FileManager.default.fileExists(atPath: root.appendingPathComponent("Components/NMPUI/ActionSurface.swift").path))
        XCTAssertTrue(FileManager.default.fileExists(atPath: root.appendingPathComponent("Components/NMPUI/ArticleMediumCard.swift").path))
        let lockText = try String(contentsOf: root.appendingPathComponent(ProjectFileSystem.lockPath))
        XCTAssertTrue(lockText.contains("\"upstreamBase\""))
        XCTAssertTrue(lockText.contains("\"upstreamHash\""))
        XCTAssertTrue(lockText.contains("article-medium-card"))
    }

    func testUnknownComponentAndUnsafeRegistryPathAreRejected() throws {
        let root = temporaryDirectory()
        let installer = try NMPUIInstaller(catalog: .bundled(), projectRoot: root)
        XCTAssertThrowsError(try installer.add("not-real")) { error in
            XCTAssertEqual(error as? NMPUIError, .unknownComponent("not-real"))
        }

        let unsafe = catalog(version: "v1", components: [
            component("unsafe", version: "1", source: "unsafe.swift", destination: "../outside.swift")
        ], templates: ["unsafe.swift": "unsafe\n"])
        XCTAssertThrowsError(try NMPUIInstaller(catalog: unsafe, projectRoot: root)) { error in
            XCTAssertEqual(error as? NMPUIError, .unsafePath("../outside.swift"))
        }
        XCTAssertFalse(FileManager.default.fileExists(atPath: root.deletingLastPathComponent().appendingPathComponent("outside.swift").path))
    }

    func testLateCollisionDoesNotInstallEarlierDependencyOrLock() throws {
        let root = temporaryDirectory()
        let catalog = catalog(version: "v1", components: [
            component("dependency", version: "1", source: "dep.swift", destination: "Components/Dep.swift"),
            component("root", version: "1", source: "root.swift", destination: "Components/Root.swift", dependencies: ["dependency"]),
        ], templates: ["dep.swift": "dependency\n", "root.swift": "root\n"])
        try FileManager.default.createDirectory(at: root.appendingPathComponent("Components"), withIntermediateDirectories: true)
        try Data("unmanaged\n".utf8).write(to: root.appendingPathComponent("Components/Root.swift"))
        let installer = try NMPUIInstaller(catalog: catalog, projectRoot: root)

        XCTAssertThrowsError(try installer.add("root")) { error in
            XCTAssertEqual(error as? NMPUIError, .collision("Components/Root.swift"))
        }
        XCTAssertFalse(FileManager.default.fileExists(atPath: root.appendingPathComponent("Components/Dep.swift").path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: root.appendingPathComponent(ProjectFileSystem.lockPath).path))
        XCTAssertEqual(try String(contentsOf: root.appendingPathComponent("Components/Root.swift")), "unmanaged\n")
    }

    func testCLIRejectsSymlinkedManagedDirectoryWithoutAnyMutation() throws {
        let root = temporaryDirectory()
        let outside = temporaryDirectory()
        try FileManager.default.createSymbolicLink(
            at: root.appendingPathComponent("Components"),
            withDestinationURL: outside
        )

        let result = try runCLI(root: root, arguments: ["add", "article-medium-card"])

        XCTAssertNotEqual(result.status, 0)
        XCTAssertTrue(result.standardError.contains("unsafe path: Components/NMPUI/ActionSurface.swift"))
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: outside.path), [])
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: root.path), ["Components"])
        XCTAssertEqual(
            try FileManager.default.destinationOfSymbolicLink(atPath: root.appendingPathComponent("Components").path),
            outside.path
        )
        XCTAssertFalse(FileManager.default.fileExists(atPath: root.appendingPathComponent(ProjectFileSystem.lockPath).path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: outside.appendingPathComponent("NMPUI/ActionSurface.swift").path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: outside.appendingPathComponent("NMPUI/ArticleMediumCard.swift").path))
    }

    func testDirectorySwapBetweenPlanningAndCommitCannotEscapeRoot() throws {
        let root = temporaryDirectory()
        let outside = temporaryDirectory()
        try FileManager.default.createDirectory(at: root.appendingPathComponent("Components"), withIntermediateDirectories: true)
        var swapped = false
        let installer = try NMPUIInstaller(
            catalog: pairedCatalog(registry: "v1", dependency: "dep\n", root: "root\n", version: "1"),
            projectRoot: root
        ) { point in
            guard point == .write("Components/Dep.swift"), !swapped else { return }
            swapped = true
            try FileManager.default.removeItem(at: root.appendingPathComponent("Components"))
            try FileManager.default.createSymbolicLink(
                at: root.appendingPathComponent("Components"),
                withDestinationURL: outside
            )
        }

        XCTAssertThrowsError(try installer.add("root")) { error in
            XCTAssertEqual(error as? NMPUIError, .unsafePath("Components/Dep.swift"))
        }
        XCTAssertTrue(swapped)
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: outside.path), [])
        XCTAssertFalse(FileManager.default.fileExists(atPath: root.appendingPathComponent(ProjectFileSystem.lockPath).path))
    }

    func testEveryManagedFileOperationRejectsFinalSymlink() throws {
        let root = temporaryDirectory()
        let outside = temporaryDirectory().appendingPathComponent("outside.swift")
        try Data("outside\n".utf8).write(to: outside)
        let link = root.appendingPathComponent("Managed.swift")
        try FileManager.default.createSymbolicLink(at: link, withDestinationURL: outside)
        let fileSystem = try ProjectFileSystem(root: root)

        for operation in [
            { _ = try fileSystem.exists("Managed.swift") },
            { _ = try fileSystem.read("Managed.swift") },
            { try fileSystem.write("replacement\n", to: "Managed.swift") },
            { try fileSystem.remove("Managed.swift") },
        ] {
            XCTAssertThrowsError(try operation()) { error in
                XCTAssertEqual(error as? NMPUIError, .unsafePath("Managed.swift"))
            }
        }
        XCTAssertEqual(try String(contentsOf: outside), "outside\n")
        XCTAssertEqual(try FileManager.default.destinationOfSymbolicLink(atPath: link.path), outside.path)
    }

    func testLateAddFileFailureRollsBackWholeClosureAndCreatedDirectories() throws {
        let root = temporaryDirectory()
        let installer = try failingInstaller(
            catalog: pairedCatalog(registry: "v1", dependency: "dep\n", root: "root\n", version: "1"),
            root: root,
            at: .write("Components/Root.swift")
        )

        XCTAssertThrowsError(try installer.add("root")) { error in
            XCTAssertEqual(error as? InjectedFailure, .mutation(.write("Components/Root.swift")))
        }
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: root.path), [])
    }

    func testLateAddLockFailureRollsBackWholeClosureAndCreatedDirectories() throws {
        let root = temporaryDirectory()
        let installer = try failingInstaller(
            catalog: pairedCatalog(registry: "v1", dependency: "dep\n", root: "root\n", version: "1"),
            root: root,
            at: .write(ProjectFileSystem.lockPath)
        )

        XCTAssertThrowsError(try installer.add("root")) { error in
            XCTAssertEqual(error as? InjectedFailure, .mutation(.write(ProjectFileSystem.lockPath)))
        }
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: root.path), [])
    }

    func testNameMaxFailureAfterParentCreationLeavesRootAndLockUntouched() throws {
        let root = temporaryDirectory()
        let finalComponent = String(repeating: "x", count: 256)
        let catalog = catalog(
            version: "v1",
            components: [
                component(
                    "long-name",
                    version: "1",
                    source: "long.swift",
                    destination: "New/\(finalComponent)"
                ),
            ],
            templates: ["long.swift": "content\n"]
        )
        let installer = try NMPUIInstaller(catalog: catalog, projectRoot: root)

        XCTAssertThrowsError(try installer.add("long-name"))
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: root.path), [])
        XCTAssertFalse(FileManager.default.fileExists(atPath: root.appendingPathComponent("New").path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: root.appendingPathComponent(ProjectFileSystem.lockPath).path))
    }

    func testRegularFileCreatedAfterPlanningIsNeverOverwritten() throws {
        let root = temporaryDirectory()
        let target = root.appendingPathComponent("Components/Root.swift")
        var injected = false
        let installer = try NMPUIInstaller(
            catalog: pairedCatalog(registry: "v1", dependency: "dep\n", root: "root\n", version: "1"),
            projectRoot: root
        ) { point in
            guard point == .write("Components/Root.swift"), !injected else { return }
            injected = true
            try Data("unmanaged concurrent file\n".utf8).write(to: target)
        }

        XCTAssertThrowsError(try installer.add("root")) { error in
            XCTAssertEqual(error as? NMPUIError, .collision("Components/Root.swift"))
        }
        XCTAssertTrue(injected)
        XCTAssertEqual(try String(contentsOf: target), "unmanaged concurrent file\n")
        XCTAssertFalse(FileManager.default.fileExists(atPath: root.appendingPathComponent("Components/Dep.swift").path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: root.appendingPathComponent(ProjectFileSystem.lockPath).path))
    }

    func testDuplicateDestinationWithinComponentIsTypedBeforeMutation() throws {
        let root = temporaryDirectory()
        let duplicate = RegistryComponent(
            name: "duplicate",
            version: "1",
            summary: "duplicate",
            files: [
                RegistryFile(source: "one.swift", destination: "Components/Same.swift"),
                RegistryFile(source: "two.swift", destination: "Components/Same.swift"),
            ]
        )
        let duplicateCatalog = catalog(
            version: "v1",
            components: [duplicate],
            templates: ["one.swift": "one\n", "two.swift": "two\n"]
        )

        XCTAssertThrowsError(try NMPUIInstaller(catalog: duplicateCatalog, projectRoot: root)) { error in
            XCTAssertEqual(
                error as? NMPUIError,
                .invalidRegistry("duplicate destination Components/Same.swift in component duplicate")
            )
        }
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: root.path), [])
    }

    func testDuplicateDestinationAcrossDependencyClosureIsTypedBeforeMutation() throws {
        let root = temporaryDirectory()
        let duplicateCatalog = catalog(
            version: "v1",
            components: [
                component(
                    "dependency",
                    version: "1",
                    source: "dep.swift",
                    destination: "Components/Shared.swift"
                ),
                component(
                    "root",
                    version: "1",
                    source: "root.swift",
                    destination: "Components/Shared.swift",
                    dependencies: ["dependency"]
                ),
            ],
            templates: ["dep.swift": "dep\n", "root.swift": "root\n"]
        )

        XCTAssertThrowsError(try NMPUIInstaller(catalog: duplicateCatalog, projectRoot: root)) { error in
            XCTAssertEqual(
                error as? NMPUIError,
                .invalidRegistry(
                    "destination Components/Shared.swift belongs to both dependency and root"
                )
            )
        }
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: root.path), [])
    }

    func testDiffReportsCleanModifiedAndMissingFiles() throws {
        let root = temporaryDirectory()
        let installer = try NMPUIInstaller(catalog: .bundled(), projectRoot: root)
        _ = try installer.add("article-medium-card")
        XCTAssertFalse(try installer.diff("article-medium-card").hasDifferences)

        let article = root.appendingPathComponent("Components/NMPUI/ArticleMediumCard.swift")
        try Data("local edit\n".utf8).write(to: article)
        let modified = try installer.diff("article-medium-card")
        XCTAssertTrue(modified.hasDifferences)
        XCTAssertTrue(modified.text.contains("M Components/NMPUI/ArticleMediumCard.swift"))

        try FileManager.default.removeItem(at: article)
        let missing = try installer.diff("article-medium-card")
        XCTAssertTrue(missing.text.contains("D Components/NMPUI/ArticleMediumCard.swift"))
    }

    func testCleanUpdateFastForwardsFileAndLock() throws {
        let root = temporaryDirectory()
        let v1 = singleFileCatalog(registry: "v1", componentVersion: "1", content: "one\n")
        let v2 = singleFileCatalog(registry: "v2", componentVersion: "2", content: "two\n")
        _ = try NMPUIInstaller(catalog: v1, projectRoot: root).add("card")

        let report = try NMPUIInstaller(catalog: v2, projectRoot: root).update("card")

        XCTAssertEqual(report.lines, ["updated card@2"])
        XCTAssertEqual(try String(contentsOf: root.appendingPathComponent("Components/Card.swift")), "two\n")
        let lock = try ProjectFileSystem(root: root).loadLock(registryVersion: "v2")
        XCTAssertEqual(lock.components["card"]?.version, "2")
        XCTAssertEqual(lock.components["card"]?.files["Components/Card.swift"]?.upstreamBase, "two\n")
    }

    func testUpdateOnUninstalledComponentIsTypedAndDoesNotInstall() throws {
        let root = temporaryDirectory()
        let installer = try NMPUIInstaller(
            catalog: singleFileCatalog(registry: "v1", componentVersion: "1", content: "one\n"),
            projectRoot: root
        )

        XCTAssertThrowsError(try installer.update("card")) { error in
            XCTAssertEqual(error as? NMPUIError, .notInstalled("card"))
        }
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: root.path), [])
    }

    func testLateUpdateFileFailureRestoresEveryFileAndLockAndRemovesNewFiles() throws {
        try assertUpdateRollback(at: .write("Components/Root.swift"))
    }

    func testLateUpdateLockFailureRestoresEveryFileAndLockAndRemovesNewFiles() throws {
        try assertUpdateRollback(at: .write(ProjectFileSystem.lockPath))
    }

    func testManagedFileEditedAfterPlanningIsRestoredAndNeverOverwritten() throws {
        let root = temporaryDirectory()
        let v1 = pairedCatalog(registry: "v1", dependency: "dep one\n", root: "root one\n", version: "1")
        _ = try NMPUIInstaller(catalog: v1, projectRoot: root).add("root")
        let dependencyURL = root.appendingPathComponent("Components/Dep.swift")
        let rootURL = root.appendingPathComponent("Components/Root.swift")
        let lockURL = root.appendingPathComponent(ProjectFileSystem.lockPath)
        let lockBefore = try Data(contentsOf: lockURL)
        var injected = false
        let v2 = pairedCatalog(registry: "v2", dependency: "dep two\n", root: "root two\n", version: "2")
        let installer = try NMPUIInstaller(catalog: v2, projectRoot: root) { point in
            guard point == .write("Components/Root.swift"), !injected else { return }
            injected = true
            try Data("root concurrent\n".utf8).write(to: rootURL)
        }

        XCTAssertThrowsError(try installer.update("root")) { error in
            XCTAssertEqual(error as? NMPUIError, .concurrentModification("Components/Root.swift"))
        }
        XCTAssertTrue(injected)
        XCTAssertEqual(try String(contentsOf: rootURL), "root concurrent\n")
        XCTAssertEqual(try String(contentsOf: dependencyURL), "dep one\n")
        XCTAssertEqual(try Data(contentsOf: lockURL), lockBefore)
    }

    func testRollbackNeverOverwritesEditMadeAfterSuccessfulMutation() throws {
        let root = temporaryDirectory()
        let v1 = pairedCatalog(registry: "v1", dependency: "dep one\n", root: "root one\n", version: "1")
        _ = try NMPUIInstaller(catalog: v1, projectRoot: root).add("root")
        let dependencyURL = root.appendingPathComponent("Components/Dep.swift")
        let rootURL = root.appendingPathComponent("Components/Root.swift")
        let lockURL = root.appendingPathComponent(ProjectFileSystem.lockPath)
        let lockBefore = try Data(contentsOf: lockURL)
        let v2 = pairedCatalog(registry: "v2", dependency: "dep two\n", root: "root two\n", version: "2")
        let installer = try NMPUIInstaller(catalog: v2, projectRoot: root) { point in
            guard point == .write("Components/Root.swift") else { return }
            try Data("dependency concurrent\n".utf8).write(to: dependencyURL)
            throw InjectedFailure.mutation(point)
        }

        XCTAssertThrowsError(try installer.update("root")) { error in
            guard case .transactionFailed(let message) = error as? NMPUIError else {
                return XCTFail("expected transactionFailed, got \(error)")
            }
            XCTAssertTrue(message.contains("managed path changed concurrently: Components/Dep.swift"))
        }
        XCTAssertEqual(try String(contentsOf: dependencyURL), "dependency concurrent\n")
        XCTAssertEqual(try String(contentsOf: rootURL), "root one\n")
        XCTAssertEqual(try Data(contentsOf: lockURL), lockBefore)
    }

    func testDisjointLocalAndUpstreamEditsMerge() throws {
        let root = temporaryDirectory()
        let base = "title: old\nbody: same\nfooter: old\n"
        _ = try NMPUIInstaller(catalog: singleFileCatalog(registry: "v1", componentVersion: "1", content: base), projectRoot: root).add("card")
        try Data("title: local\nbody: same\nfooter: old\n".utf8)
            .write(to: root.appendingPathComponent("Components/Card.swift"))
        let v2 = singleFileCatalog(
            registry: "v2", componentVersion: "2",
            content: "title: old\nbody: same\nfooter: upstream\n"
        )

        _ = try NMPUIInstaller(catalog: v2, projectRoot: root).update("card")

        XCTAssertEqual(
            try String(contentsOf: root.appendingPathComponent("Components/Card.swift")),
            "title: local\nbody: same\nfooter: upstream\n"
        )
    }

    func testConflictWritesEvidenceAndLeavesLockByteIdentical() throws {
        let root = temporaryDirectory()
        let v1 = singleFileCatalog(registry: "v1", componentVersion: "1", content: "color: blue\n")
        _ = try NMPUIInstaller(catalog: v1, projectRoot: root).add("card")
        let lockURL = root.appendingPathComponent(ProjectFileSystem.lockPath)
        let lockBefore = try Data(contentsOf: lockURL)
        try Data("color: red\n".utf8).write(to: root.appendingPathComponent("Components/Card.swift"))
        let v2 = singleFileCatalog(registry: "v2", componentVersion: "2", content: "color: green\n")

        XCTAssertThrowsError(try NMPUIInstaller(catalog: v2, projectRoot: root).update("card")) { error in
            XCTAssertEqual(error as? NMPUIError, .updateConflict(["Components/Card.swift"]))
        }

        XCTAssertEqual(try Data(contentsOf: lockURL), lockBefore)
        let conflicted = try String(contentsOf: root.appendingPathComponent("Components/Card.swift"))
        XCTAssertTrue(conflicted.contains("<<<<<<< local"))
        XCTAssertTrue(conflicted.contains("||||||| upstream base"))
        XCTAssertTrue(conflicted.contains(">>>>>>> upstream"))
        XCTAssertTrue(FileManager.default.fileExists(atPath: root.appendingPathComponent(ProjectFileSystem.conflictPath).path))
    }

    func testLateRootConflictDoesNotPartiallyAdvanceDependency() throws {
        let root = temporaryDirectory()
        let v1 = pairedCatalog(registry: "v1", dependency: "dep one\n", root: "root one\n", version: "1")
        _ = try NMPUIInstaller(catalog: v1, projectRoot: root).add("root")
        let lockURL = root.appendingPathComponent(ProjectFileSystem.lockPath)
        let lockBefore = try Data(contentsOf: lockURL)
        try Data("root local\n".utf8).write(to: root.appendingPathComponent("Components/Root.swift"))
        let v2 = pairedCatalog(registry: "v2", dependency: "dep two\n", root: "root upstream\n", version: "2")

        XCTAssertThrowsError(try NMPUIInstaller(catalog: v2, projectRoot: root).update("root"))

        XCTAssertEqual(try String(contentsOf: root.appendingPathComponent("Components/Dep.swift")), "dep one\n")
        XCTAssertEqual(try Data(contentsOf: lockURL), lockBefore)
    }

    func testSHA256MatchesPublishedVector() {
        XCTAssertEqual(SHA256.digest("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    }

    private func temporaryDirectory() -> URL {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent("nmp-ui-tests-\(UUID().uuidString)")
        try! FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        temporaryDirectories.append(url)
        return url
    }

    private func failingInstaller(
        catalog: ComponentCatalog,
        root: URL,
        at failurePoint: ProjectFileSystem.MutationPoint
    ) throws -> NMPUIInstaller {
        try NMPUIInstaller(catalog: catalog, projectRoot: root) { point in
            if point == failurePoint { throw InjectedFailure.mutation(point) }
        }
    }

    private func assertUpdateRollback(at failurePoint: ProjectFileSystem.MutationPoint) throws {
        let root = temporaryDirectory()
        let v1 = pairedCatalog(registry: "v1", dependency: "dep one\n", root: "root one\n", version: "1")
        _ = try NMPUIInstaller(catalog: v1, projectRoot: root).add("root")
        let dependencyURL = root.appendingPathComponent("Components/Dep.swift")
        let rootURL = root.appendingPathComponent("Components/Root.swift")
        let lockURL = root.appendingPathComponent(ProjectFileSystem.lockPath)
        let dependencyBefore = try Data(contentsOf: dependencyURL)
        let rootBefore = try Data(contentsOf: rootURL)
        let lockBefore = try Data(contentsOf: lockURL)

        let dependency = RegistryComponent(
            name: "dependency",
            version: "2",
            summary: "dependency",
            files: [
                RegistryFile(source: "dep.swift", destination: "Components/Dep.swift"),
                RegistryFile(source: "extra.swift", destination: "Components/Extra.swift"),
            ]
        )
        let rootComponent = component(
            "root",
            version: "2",
            source: "root.swift",
            destination: "Components/Root.swift",
            dependencies: ["dependency"]
        )
        let v2 = catalog(
            version: "v2",
            components: [dependency, rootComponent],
            templates: ["dep.swift": "dep two\n", "extra.swift": "extra\n", "root.swift": "root two\n"]
        )
        let installer = try failingInstaller(catalog: v2, root: root, at: failurePoint)

        XCTAssertThrowsError(try installer.update("root")) { error in
            XCTAssertEqual(error as? InjectedFailure, .mutation(failurePoint))
        }
        XCTAssertEqual(try Data(contentsOf: dependencyURL), dependencyBefore)
        XCTAssertEqual(try Data(contentsOf: rootURL), rootBefore)
        XCTAssertEqual(try Data(contentsOf: lockURL), lockBefore)
        XCTAssertFalse(FileManager.default.fileExists(atPath: root.appendingPathComponent("Components/Extra.swift").path))
        XCTAssertEqual(
            try FileManager.default.contentsOfDirectory(atPath: root.appendingPathComponent("Components").path).sorted(),
            ["Dep.swift", "Root.swift"]
        )
    }

    private func runCLI(root: URL, arguments: [String]) throws -> (status: Int32, standardError: String) {
        let packageRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
        let executable = packageRoot.appendingPathComponent(".build/debug/nmp-ui")
        let errorPipe = Pipe()
        let process = Process()
        process.executableURL = executable
        process.arguments = ["--root", root.path] + arguments
        process.standardOutput = Pipe()
        process.standardError = errorPipe
        try process.run()
        process.waitUntilExit()
        let error = String(data: errorPipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8) ?? ""
        return (process.terminationStatus, error)
    }

    private func singleFileCatalog(registry: String, componentVersion: String, content: String) -> ComponentCatalog {
        catalog(
            version: registry,
            components: [component("card", version: componentVersion, source: "card.swift", destination: "Components/Card.swift")],
            templates: ["card.swift": content]
        )
    }

    private func pairedCatalog(registry: String, dependency: String, root: String, version: String) -> ComponentCatalog {
        catalog(version: registry, components: [
            component("dependency", version: version, source: "dep.swift", destination: "Components/Dep.swift"),
            component("root", version: version, source: "root.swift", destination: "Components/Root.swift", dependencies: ["dependency"]),
        ], templates: ["dep.swift": dependency, "root.swift": root])
    }

    private func component(
        _ name: String,
        version: String,
        source: String,
        destination: String,
        dependencies: [String] = []
    ) -> RegistryComponent {
        RegistryComponent(
            name: name,
            version: version,
            summary: name,
            dependencies: dependencies,
            files: [RegistryFile(source: source, destination: destination)]
        )
    }

    private func catalog(
        version: String,
        components: [RegistryComponent],
        templates: [String: String]
    ) -> ComponentCatalog {
        ComponentCatalog(
            registry: ComponentRegistry(registryVersion: version, components: components),
            templates: templates
        )
    }
}
