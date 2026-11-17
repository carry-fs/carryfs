plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

// Phones are arm64; CI's emulator runs x86_64 (-Pcarryfs.abi=x86_64).
val abi = providers.gradleProperty("carryfs.abi").getOrElse("arm64-v8a")

android {
    namespace = "com.carryfs.android"
    compileSdk = 35
    ndkVersion = "30.0.16248370"

    defaultConfig {
        applicationId = "com.carryfs.android"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        // The product version from the root Cargo.toml (VERSIONING.md).
        versionName = Regex("""\[workspace\.package\]\s*version\s*=\s*"([^"]+)"""")
            .find(rootDir.resolveSibling("Cargo.toml").readText())!!.groupValues[1]
        ndk { abiFilters += abi }
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    buildFeatures { buildConfig = true }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    sourceSets["main"].jniLibs.srcDir(layout.buildDirectory.dir("rust"))
}

kotlin {
    compilerOptions { jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17) }
}

// The native library is always built optimized: the storage core is too
// slow unoptimized to exercise on a device.
// It runs the `cargo` on PATH, so cargo-ndk and the Rust target for `abi`
// must be installed for that toolchain.
val cargoBuild = tasks.register<Exec>("cargoBuild") {
    workingDir = rootDir.resolve("native")
    environment("ANDROID_NDK_HOME", android.ndkDirectory.path)
    commandLine(
        "cargo", "ndk", "-t", abi, "-P", "26",
        "-o", layout.buildDirectory.dir("rust").get().asFile.path,
        "build", "-p", "carryfs-android", "--release",
    )
}

tasks.named("preBuild") { dependsOn(cargoBuild) }

dependencies {
    androidTestImplementation("androidx.test:runner:1.6.2")
    androidTestImplementation("junit:junit:4.13.2")
}
