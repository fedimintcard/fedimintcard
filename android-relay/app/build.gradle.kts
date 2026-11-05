plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "org.fedimintcard.relay"
    compileSdk = 35
    defaultConfig {
        applicationId = "org.fedimintcard.relay"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.2"
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
