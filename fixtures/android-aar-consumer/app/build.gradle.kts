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
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

kotlin {
    jvmToolchain(17)
}

dependencies {
    implementation("com.nmp:nmp-android:0.0.0-local")
}
