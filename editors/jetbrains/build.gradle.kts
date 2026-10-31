plugins {
    id("org.jetbrains.intellij") version "1.17.0"
    kotlin("jvm") version "1.9.0"
}

group = "com.pgweave"
version = "0.1.0"

repositories {
    mavenCentral()
}

intellij {
    version.set("2024.1")
    type.set("IC")  // IntelliJ Community Edition
    plugins.set(listOf(
        "com.redhat.devtools.lsp4ij:0.7.0"  // LSP4IJ plugin
    ))
}

tasks {
    patchPluginXml {
        sinceBuild.set("241")
        untilBuild.set("251.*")
    }
    signPlugin {
        certificateChain.set(System.getenv("CERTIFICATE_CHAIN") ?: "")
        privateKey.set(System.getenv("PRIVATE_KEY") ?: "")
        password.set(System.getenv("PRIVATE_KEY_PASSWORD") ?: "")
    }
    publishPlugin {
        token.set(System.getenv("PUBLISH_TOKEN") ?: "")
    }
}
