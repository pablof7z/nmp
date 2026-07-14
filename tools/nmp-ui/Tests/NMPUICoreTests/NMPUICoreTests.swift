import Foundation
import XCTest
@testable import NMPUICore

final class NMPUICoreTests: XCTestCase {
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
