plugins {
    id("com.android.application")
    kotlin("android")
}

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

        val successRelay =
            providers.gradleProperty("nmpQualificationRelay").orNull
                ?: "ws://10.0.2.2:47391"
        val recoveryRelay =
            providers.gradleProperty("nmpQualificationRecoveryRelay").orNull
                ?: "ws://10.0.2.2:47392"
        val offlineRelay =
            providers.gradleProperty("nmpQualificationOfflineRelay").orNull
                ?: "ws://10.0.2.2:47393"
        buildConfigField("String", "NMP_QUALIFICATION_RELAY", "\"$successRelay\"")
        buildConfigField("String", "NMP_QUALIFICATION_RECOVERY_RELAY", "\"$recoveryRelay\"")
        buildConfigField("String", "NMP_QUALIFICATION_OFFLINE_RELAY", "\"$offlineRelay\"")
        buildConfigField(
            "boolean",
            "NMP_EXPECT_NATIVE_LOAD",
            providers.gradleProperty("nmpExpectNativeLoad").orNull ?: "true",
        )
    }

    testBuildType = "release"

    buildTypes {
        getByName("release") {
            signingConfig = signingConfigs.getByName("debug")
        }
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
    if (providers.gradleProperty("nmpCompileUnselectedControl").orNull == "true") {
        sourceSets.getByName("main").kotlin.srcDir("src/negative/kotlin")
    }
}

dependencies {
    implementation(
        providers.gradleProperty("nmpQualificationCoordinate").orNull
            ?: "com.nmp:nmp-android:0.0.0",
    )

    androidTestImplementation("androidx.test:core-ktx:1.6.1")
    androidTestImplementation("androidx.test:runner:1.6.2")
    androidTestImplementation("androidx.test.ext:junit-ktx:1.2.1")
}
