import java.util.Properties

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)
    alias(libs.plugins.kotlin.atomicfu)
    alias(libs.plugins.gobley.cargo)
    alias(libs.plugins.gobley.uniffi)
}

// Release signing: put storeFile / storePassword / keyAlias / keyPassword in
// android/keystore.properties (git-ignored). Absent → the release build falls
// back to the debug key, which Play rejects on upload — a loud, safe failure.
val keystorePropsFile = rootProject.file("keystore.properties")
val keystoreProps = Properties().apply {
    if (keystorePropsFile.exists()) keystorePropsFile.inputStream().use { load(it) }
}
val hasReleaseKeystore = keystoreProps.getProperty("storeFile") != null

android {
    // namespace = the source package + generated R/BuildConfig; unrelated to
    // applicationId and left as-is to avoid churning every Kotlin file.
    // applicationId is the on-device / store identity: renamed rest.fortis.wallet
    // -> com.fortistechlabs.wallet at v0.2.0 (the fortis.rest -> fortistechlabs.com
    // move). A new applicationId is a new app — installs do not cross-update.
    namespace = "com.fortis.wallet"
    compileSdk = 37

    defaultConfig {
        applicationId = "com.fortistechlabs.wallet"
        minSdk = 26
        targetSdk = 37
        versionCode = 4
        versionName = "0.2.0"
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }

    signingConfigs {
        if (hasReleaseKeystore) {
            create("release") {
                storeFile = file(keystoreProps.getProperty("storeFile"))
                storePassword = keystoreProps.getProperty("storePassword")
                keyAlias = keystoreProps.getProperty("keyAlias")
                keyPassword = keystoreProps.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        getByName("release") {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            signingConfig = signingConfigs.getByName(if (hasReleaseKeystore) "release" else "debug")
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlin { jvmToolchain(17) }
    buildFeatures {
        compose = true
        buildConfig = true
    }

    packaging {
        jniLibs { useLegacyPackaging = false }
        resources.excludes += setOf("/META-INF/AL2.0", "/META-INF/LGPL2.1")
    }
}

// Gobley: cross-compile wallet-ffi into jniLibs (ABIs from ndk.abiFilters),
// then generate the Kotlin bindings (package uniffi.wallet_ffi) in library mode.
cargo {
    packageDirectory = layout.projectDirectory.dir("../../crates/wallet-ffi")
}

// Gobley wires a workspace-wide `cargo clean` into `gradlew clean`. This machine
// also runs the fortis-edge / fortis-index services straight out of
// `target/release/`, so that clean dies trying to delete the locked `.exe`s.
// Swap it for a clean scoped to this module's crate — the rest of `target/`
// (and anything running from it) is left alone.
tasks.matching { it.name == "cargoClean" }.configureEach { enabled = false }
val cargoCleanFfi by tasks.registering(Exec::class) {
    group = "cargo"
    description = "cargo clean, scoped to the wallet-ffi crate"
    workingDir = layout.projectDirectory.dir("../../crates/wallet-ffi").asFile
    val cargoHome = System.getenv("CARGO_HOME") ?: "${System.getProperty("user.home")}/.cargo"
    val sep = System.getProperty("path.separator")
    environment("PATH", "$cargoHome/bin$sep${System.getenv("PATH") ?: ""}")
    commandLine("cargo", "clean", "--package", "wallet-ffi")
    isIgnoreExitValue = true // never let `gradlew clean` fail on this
}
tasks.named("clean") { dependsOn(cargoCleanFfi) }

uniffi {
    generateFromLibrary {
        packageName = "uniffi.wallet_ffi"
    }
}

dependencies {
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.lifecycle.runtime.ktx)
    implementation(libs.androidx.lifecycle.viewmodel.compose)
    implementation(libs.androidx.activity.compose)
    implementation(platform(libs.compose.bom))
    implementation(libs.compose.ui)
    implementation(libs.compose.ui.graphics)
    implementation(libs.compose.ui.tooling.preview)
    implementation(libs.compose.material3)
    implementation(libs.compose.material.icons.extended)
    implementation(libs.navigation.compose)
    implementation(libs.datastore.preferences)
    implementation(libs.biometric)
    // Force fragment forward off biometric-alpha's stale 1.2.5 — that
    // FragmentActivity crashes the Activity Result API (QR scanner) launch with
    // "Can only use lower 16 bits for requestCode".
    implementation(libs.androidx.fragment)
    implementation(libs.okhttp)
    implementation(libs.zxing.android.embedded)
    debugImplementation(libs.compose.ui.tooling)
}
