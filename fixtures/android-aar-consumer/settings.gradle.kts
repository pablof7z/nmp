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
        google()
        mavenCentral()
        val qualificationRepository =
            providers.gradleProperty("nmpAndroidRepository").orNull
                ?: error("pass -PnmpAndroidRepository=<absolute path>")
        maven {
            url = uri(qualificationRepository)
        }
    }
}

rootProject.name = "nmp-android-aar-consumer"
include(":app")
