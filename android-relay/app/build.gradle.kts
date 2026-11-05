plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
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
        versionCode = 1
        versionName = "0.3.0"
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

dependencies {
    // Pure-Java QR decoding for "Pair with till" (no Play Services on the test phones).
    implementation("com.google.zxing:core:3.5.2")
    // The pairing scanner's camera: CameraX focuses and frames properly where the platform
    // android.hardware.Camera preview did not (11/4, Pixel 8 Pro). No Play Services needed.
    implementation("androidx.activity:activity:1.9.3")
    implementation("androidx.camera:camera-camera2:1.4.2")
    implementation("androidx.camera:camera-lifecycle:1.4.2")
    implementation("androidx.camera:camera-view:1.4.2")
    testImplementation("junit:junit:4.13.2")
    testImplementation("org.json:json:20240303")
}
