// Kotlin sources compile with AGP 9's built-in Kotlin support (no kotlin-android plugin).
plugins {
    id("com.android.application")
}

val ndkVer = "30.0.16248370" // NDK r30 LTS, as android/setup-toolchain.sh installs
val cmakeVer = "4.1.2"
val minSdkVer = 29 // AudioPlaybackCapture
val abis = (findProperty("sonobus.abis") as String).split(",").map { it.trim() }.filter { it.isNotEmpty() }
val rustLibs = layout.buildDirectory.dir("rustJniLibs")
val workspace = rootProject.projectDir.parentFile

android {
    namespace = "io.github.sonobusrs"
    compileSdk = 36
    ndkVersion = ndkVer

    defaultConfig {
        applicationId = "io.github.sonobusrs"
        minSdk = minSdkVer
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"
        ndk { abiFilters += abis }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    sourceSets["main"].jniLibs.directories.add(rustLibs.get().asFile.path)

    packaging {
        // libsonobus_android.so must stay uncompressed and page aligned (16 KB devices).
        jniLibs.useLegacyPackaging = false
    }
}

// The Rust side: crates/sonobus-android as libsonobus_android.so per ABI. Always an optimised
// build (a debug engine underruns). Needs `cargo ndk` on PATH (android/setup-toolchain.sh).
val cargoNdk = tasks.register<Exec>("cargoNdk") {
    val sdk = androidComponents.sdkComponents.sdkDirectory.get().asFile
    workingDir = workspace
    environment("ANDROID_HOME", sdk.path)
    environment("ANDROID_NDK_HOME", File(sdk, "ndk/$ndkVer").path)
    environment("PATH", File(sdk, "cmake/$cmakeVer/bin").path + File.pathSeparator + System.getenv("PATH"))
    val args = mutableListOf("cargo", "ndk", "--platform", "$minSdkVer", "-o", rustLibs.get().asFile.path)
    abis.forEach { args += listOf("-t", it) }
    args += listOf("build", "--release", "-p", "sonobus-android")
    commandLine(args)
    inputs.files(fileTree(File(workspace, "crates")) { exclude("**/target/**") }, File(workspace, "Cargo.lock"))
    outputs.dir(rustLibs)
}
tasks.named("preBuild") { dependsOn(cargoNdk) }

dependencies {
    implementation("androidx.games:games-activity:4.4.0")
    implementation("androidx.appcompat:appcompat:1.8.0")
    implementation("androidx.core:core-ktx:1.17.0")
    testImplementation("junit:junit:4.13.2")
}
