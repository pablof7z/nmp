// #40: the Kotlin/Flow facade remains this JVM-only root project. #198 adds an
// optional desktop-JVM Compose library as a separate child project so Compose
// never becomes a dependency of the core SDK. Neither project is an Android
// AAR or an Android runtime qualification (see README.md).
rootProject.name = "nmp-kotlin"

include(":ui")
