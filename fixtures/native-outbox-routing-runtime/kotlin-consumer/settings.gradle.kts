pluginManagement { repositories { gradlePluginPortal(); mavenCentral() } }
dependencyResolutionManagement { repositories { mavenCentral() } }
rootProject.name = "outbox-routing-kotlin-consumer"
includeBuild("../Generated/NMP/kotlin-jvm")
