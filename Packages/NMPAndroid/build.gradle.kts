import com.android.build.gradle.LibraryExtension
import org.gradle.api.publish.maven.MavenPublication

plugins {
    id("com.android.library") version "8.7.3"
    kotlin("android") version "2.0.21"
    `maven-publish`
}

group = "com.nmp"
version = "0.0.0-local"

extensions.configure<LibraryExtension> {
    namespace = "com.nmp.sdk"
    compileSdk = 35
    ndkVersion = "27.2.12479018"

    defaultConfig {
        // java.nio.file.Files is part of the deliberately insecure development
        // checkpoint included in the common facade and is available from 26.
        minSdk = 26
        aarMetadata {
            minCompileSdk = 35
        }
        consumerProguardFiles("consumer-rules.pro")
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    sourceSets {
        getByName("main") {
            jniLibs.srcDirs("src/main/jniLibs")
        }
    }

    publishing {
        singleVariant("release") {
            withSourcesJar()
        }
    }
}

kotlin {
    jvmToolchain(17)
    sourceSets.getByName("main").kotlin {
        // One hand-written facade, compiled for two products. The Android
        // package does not copy or fork com.nmp.sdk; it consumes the exact
        // Kotlin/JVM sources and adds only its generated Android bindings.
        srcDir("../NMPKotlin/src/main/kotlin")

        // These two providers are explicitly desktop-JVM JCEKS/password
        // implementations. Shipping them under Android would make a false
        // platform-security claim; #834 owns their Android Keystore peers.
        exclude("com/nmp/sdk/SecureKeyStoreAccountStore.kt")
        exclude("com/nmp/sdk/SecureKeyStoreNip46SessionCheckpointStore.kt")
    }
}

dependencies {
    // UniFFI 0.29's Kotlin backend uses JNA. Android must consume the AAR
    // variant so its own native dispatch support is available at runtime.
    implementation("net.java.dev.jna:jna:5.14.0@aar")
    api("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.9.0")
    implementation("androidx.annotation:annotation:1.9.1")
}

publishing {
    repositories {
        maven {
            name = "qualification"
            url = uri(layout.buildDirectory.dir("qualification-repository").get().asFile)
        }
    }
}

afterEvaluate {
    publishing {
        publications {
            register<MavenPublication>("release") {
                from(components["release"])
                groupId = project.group.toString()
                artifactId = "nmp-android"
                version = project.version.toString()
            }
        }
    }
}
