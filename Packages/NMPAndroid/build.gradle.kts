import com.android.build.gradle.LibraryExtension
import org.gradle.api.publish.maven.MavenPublication
import org.gradle.api.tasks.bundling.AbstractArchiveTask

plugins {
    id("com.android.library") version "8.7.3"
    kotlin("android") version "2.0.21"
    `maven-publish`
}

val nmpGroup = providers.gradleProperty("nmpGroup").get()
val nmpVersion = providers.gradleProperty("nmpVersion").get()
val nmpArtifactId = providers.gradleProperty("nmpArtifactId").get()

group = nmpGroup
version = nmpVersion

extensions.configure<LibraryExtension> {
    namespace = providers.gradleProperty("nmpNamespace").get()
    compileSdk = providers.gradleProperty("nmpCompileSdk").get().toInt()
    ndkVersion = providers.gradleProperty("nmpNdkVersion").get()

    defaultConfig {
        minSdk = providers.gradleProperty("nmpMinSdk").get().toInt()
        aarMetadata {
            minCompileSdk = providers.gradleProperty("nmpCompileSdk").get().toInt()
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
}

dependencies {
    implementation("net.java.dev.jna:jna:5.14.0@aar")
    api("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.9.0")
    implementation("androidx.annotation:annotation:1.9.1")
}

tasks.withType<AbstractArchiveTask>().configureEach {
    isPreserveFileTimestamps = false
    isReproducibleFileOrder = true
}

publishing {
    repositories {
        maven {
            name = "nmpNative"
            url = uri(providers.gradleProperty("nmpRepository").get())
        }
    }
}

afterEvaluate {
    publishing {
        publications {
            register<MavenPublication>("release") {
                from(components["release"])
                groupId = nmpGroup
                artifactId = nmpArtifactId
                version = nmpVersion
            }
        }
    }
}
