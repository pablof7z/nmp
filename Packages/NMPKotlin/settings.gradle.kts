// #40: the Kotlin/Flow falsifier's Gradle root. Single module, JVM target
// only -- the falsifier's job is to prove the two-noun surface ports to
// `Flow` cleanly, not to ship an Android AAR (that's the M6 gate, see
// README.md in this directory).
rootProject.name = "nmp-kotlin"
