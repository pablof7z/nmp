// #40: the Kotlin/Flow facade remains this JVM-only root project. #198 adds an
// optional desktop-JVM Compose library as a separate child project so Compose
// never becomes a dependency of the core SDK. Neither project is an Android
// AAR or an Android runtime qualification (see README.md).
rootProject.name = "nmp-kotlin"

include(":ui")
// The provider is a physically selectable component: a core-only checkout or
// package-removal test does not configure it. Its build script generates this
// ignored binding before consumers invoke Gradle.
if (file("nip46/src/main/kotlin/uniffi/nmp_nip46_ffi/nmp_nip46_ffi.kt").isFile) {
    include(":nip46")
}
