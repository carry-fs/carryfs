plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.carryfs.android"
    compileSdk = 35
    ndkVersion = "30.0.16248370"

    defaultConfig {
        applicationId = "com.carryfs.android"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.2.0"
        ndk { abiFilters += "arm64-v8a" }
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
// It runs the `cargo` on PATH, so cargo-ndk and the aarch64-linux-android
// target must be installed for that toolchain.
val cargoBuild = tasks.register<Exec>("cargoBuild") {
    workingDir = rootDir.resolve("native")
    environment("ANDROID_NDK_HOME", android.ndkDirectory.path)
    commandLine(
        "cargo", "ndk", "-t", "arm64-v8a", "-P", "26",
        "-o", layout.buildDirectory.dir("rust").get().asFile.path,
        "build", "-p", "carryfs-android", "--release",
    )
}

tasks.named("preBuild") { dependsOn(cargoBuild) }

dependencies {
    androidTestImplementation("androidx.test:runner:1.6.2")
    androidTestImplementation("junit:junit:4.13.2")
}
