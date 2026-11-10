plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

// The phone's offline core (host/pos-core, docs/POS-OFFLINE.md § Android), built from this checkout
// by the script CI runs: its Kotlin bindings before every compile, its arm64-v8a and x86_64
// libraries before packaging. Neither is committed: the bindings and the libraries must come from
// one tree, or UniFFI's checksums refuse to load the library (offline is then off).
val host: File = rootProject.file("../host")
val cargoTarget: File = System.getenv("CARGO_TARGET_DIR")?.let { host.resolve(it) } ?: host.resolve("target")
val coreOut: File = cargoTarget.resolve("pos-core-mobile")
val coreBindings: File = coreOut.resolve("bindings/kotlin")
val coreJniLibs: File = coreOut.resolve("android/jniLibs")

/** One step of host/scripts/pos-core-mobile.sh, rerun when the core, the card crate or the build changes. */
fun Exec.coreStep(vararg args: String) {
    group = "pos-core"
    workingDir = host
    commandLine(listOf(host.resolve("scripts/pos-core-mobile.sh").path) + args)
    // An IDE's Gradle daemon may not have rustup's cargo (and cargo-ndk) on its PATH.
    val cargoBin = (System.getenv("CARGO_HOME") ?: System.getProperty("user.home") + "/.cargo") + "/bin"
    environment("PATH", cargoBin + File.pathSeparator + System.getenv("PATH"))
    inputs.files(
        fileTree(host.resolve("pos-core")),
        fileTree(host.resolve("card")),
        host.resolve("Cargo.lock"),
        host.resolve("Cargo.toml"),
        host.resolve("rust-toolchain.toml"),
        host.resolve("scripts/pos-core-mobile.sh"),
    ).withPropertyName("core").withPathSensitivity(PathSensitivity.RELATIVE)
}

val posCoreBindings = tasks.register<Exec>("posCoreBindings") {
    description = "The pos-core Kotlin bindings (UniFFI), from a host build of the library"
    coreStep("bindings", "kotlin")
    outputs.dir(coreBindings)
}

val posCoreAndroid = tasks.register<Exec>("posCoreAndroid") {
    description = "The pos-core libraries for arm64-v8a and x86_64 (cargo-ndk, an NDK r27 or newer in ANDROID_NDK_HOME)"
    coreStep("android")
    outputs.dir(coreJniLibs)
}

android {
    namespace = "org.fedimintcard.relay"
    compileSdk = 36
    defaultConfig {
        // The store id matches iOS (org.fedimintcard.pos); the Kotlin namespace keeps the old
        // relay name so no source moves.
        applicationId = "org.fedimintcard.pos"
        minSdk = 26
        targetSdk = 36
        versionCode = 2
        versionName = "0.3.0"
        // The ABIs the core is built for (and JNA's libraries are kept for): phones and the emulator.
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }
    sourceSets {
        getByName("main") {
            java.srcDir(coreBindings)
            jniLibs.srcDir(coreJniLibs)
        }
    }
    signingConfigs {
        create("release") {
            // The Fedimint Card upload key, shared with the provisioner (apps/provisioner): the
            // path and alias from the environment, the passwords too (read from the macOS Keychain
            // by scripts/release-android.sh). Never a committed keystore.
            System.getenv("FEDIMINTCARD_ANDROID_UPLOAD_KEYSTORE")?.let { storeFile = file(it) }
            storePassword = System.getenv("FEDIMINTCARD_ANDROID_UPLOAD_STORE_PASSWORD")
            keyAlias = System.getenv("FEDIMINTCARD_ANDROID_UPLOAD_KEY_ALIAS")
            keyPassword = System.getenv("FEDIMINTCARD_ANDROID_UPLOAD_KEY_PASSWORD")
        }
    }
    buildTypes {
        getByName("release") {
            signingConfig = signingConfigs.getByName("release")
            isMinifyEnabled = false
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
}

tasks.named("preBuild") { dependsOn(posCoreBindings) }
tasks.matching { it.name.startsWith("merge") && it.name.endsWith("JniLibFolders") }.configureEach { dependsOn(posCoreAndroid) }

// PosCoreAdapterTest loads the host build of the core that posCoreBindings makes (from the same
// tree as the bindings), through JNA's jar, which carries the host's own JNA library.
tasks.withType<Test>().configureEach {
    systemProperty("jna.library.path", cargoTarget.resolve("debug").path)
    systemProperty("jna.tmpdir", layout.buildDirectory.dir("jna").get().asFile.path)
}

dependencies {
    // Pure-Java QR decoding for "Pair with till" (no Play Services on the test phones).
    implementation("com.google.zxing:core:3.5.2")
    // The pairing scanner's camera: CameraX focuses and frames properly where the platform
    // android.hardware.Camera preview did not (11/4, Pixel 8 Pro). No Play Services needed.
    implementation("androidx.activity:activity:1.9.3")
    implementation("androidx.camera:camera-camera2:1.4.2")
    implementation("androidx.camera:camera-lifecycle:1.4.2")
    implementation("androidx.camera:camera-view:1.4.2")
    // The generated bindings call the core through JNA (its Android libraries, 16 KB aligned).
    implementation("net.java.dev.jna:jna:5.17.0@aar")
    testImplementation("net.java.dev.jna:jna:5.17.0")
    testImplementation("junit:junit:4.13.2")
    testImplementation("org.json:json:20240303")
}
