plugins {
    id("com.android.application")
    kotlin("android")
}

val qualificationRelay =
    providers.gradleProperty("nmpQualificationRelay")
        .orElse("ws://10.0.2.2:47391")
        .get()
val missingRuntimeAar = providers.gradleProperty("nmpMissingRuntimeAar").orNull

android {
    namespace = "com.nmp.qualification.consumer"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.nmp.qualification.consumer"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "1"
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
        buildConfigField("String", "NMP_QUALIFICATION_RELAY", "\"$qualificationRelay\"")
        buildConfigField("boolean", "NMP_EXPECT_NATIVE_LOAD", (missingRuntimeAar == null).toString())
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        buildConfig = true
    }
}

kotlin {
    jvmToolchain(17)
}

dependencies {
    if (missingRuntimeAar == null) {
        implementation("com.nmp:nmp-android:0.0.0-local")
    } else {
        // Deliberate #832 falsifier: this direct AAR is a copy of the exact
        // publication with only lib/x86_64/libnmp_ffi.so removed. Its POM is
        // absent by construction, so declare the same runtime dependencies
        // explicitly. JNA retains its x86_64 slice, allowing install/launch
        // to reach NMP's missing native library instead of failing earlier
        // with an unrelated all-native-libraries ABI rejection.
        implementation(files(missingRuntimeAar))
        implementation("net.java.dev.jna:jna:5.14.0@aar")
        implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.9.0")
        implementation("androidx.annotation:annotation:1.9.1")
    }
    implementation("androidx.activity:activity-ktx:1.10.1")
    implementation("androidx.lifecycle:lifecycle-viewmodel-ktx:2.8.7")
    androidTestImplementation("androidx.test:core-ktx:1.7.0")
    androidTestImplementation("androidx.test.ext:junit:1.3.0")
    androidTestImplementation("androidx.test:runner:1.7.0")
}
