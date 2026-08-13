plugins {
    kotlin("jvm") version "2.0.21"
    application
}

dependencies {
    implementation("com.nmp:nmp-kotlin:0.0.0")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.9.0")
}

kotlin { jvmToolchain(17) }
application { mainClass.set("com.nmp.qualification.OutboxRoutingKotlinConsumerKt") }
