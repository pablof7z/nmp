plugins {
    kotlin("jvm")
}

repositories {
    mavenCentral()
}

dependencies {
    api(project(":"))
    implementation("net.java.dev.jna:jna:5.14.0")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.9.0")

    testImplementation(kotlin("test"))
    testImplementation("org.junit.jupiter:junit-jupiter:5.10.2")
    testRuntimeOnly("org.junit.platform:junit-platform-launcher")
}

kotlin {
    jvmToolchain(17)
}

tasks.processResources {
    // The verified JNA source tree is read-only by design. Do not preserve
    // that directory mode into Gradle's private build output before its
    // children have been copied.
    dirPermissions {
        unix("755")
    }
}

tasks.test {
    useJUnitPlatform()
}
