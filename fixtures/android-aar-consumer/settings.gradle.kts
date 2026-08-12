pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        val generatedRepository =
            providers.gradleProperty("nmpAndroidRepository").orNull
                ?: error("pass -PnmpAndroidRepository=<absolute path>")
        maven { url = uri(generatedRepository) }
        google()
        mavenCentral()
    }
}

rootProject.name = "nmp-android-aar-consumer"
include(":app")
