import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.serialization")
    id("org.jetbrains.kotlin.plugin.compose")
}

fun Project.stringProp(name: String, default: String = ""): String {
    val localDefaults = Properties().apply {
        val file = rootProject.file("local.defaults.properties")
        if (file.isFile) {
            file.inputStream().use { stream -> this.load(stream) }
        }
    }
    val fromLocalDefaults = (localDefaults.getProperty(name) ?: "").trim()
    if (fromLocalDefaults.isNotBlank()) {
        return fromLocalDefaults
    }
    return (findProperty(name) as String?)?.trim() ?: default
}

fun String.toBuildConfigString(): String = '"' + replace("\\", "\\\\").replace("\"", "\\\"") + '"'

android {
    namespace = "li.rajeshgo.sm"
    compileSdk = 35

    defaultConfig {
        applicationId = "li.rajeshgo.sm"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"

        buildConfigField("String", "SM_DEFAULT_SERVER_URL", project.stringProp("SM_DEFAULT_SERVER_URL").toBuildConfigString())
        buildConfigField("String", "SM_GOOGLE_SERVER_CLIENT_ID", project.stringProp("SM_GOOGLE_SERVER_CLIENT_ID").toBuildConfigString())
        buildConfigField("String", "SM_APK_HASH", project.stringProp("SM_APK_HASH").toBuildConfigString())
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }

    buildFeatures {
        compose = true
        buildConfig = true
    }
}

dependencies {
    val composeBom = platform("androidx.compose:compose-bom:2024.12.01")
    implementation(composeBom)
    implementation("com.google.android.material:material:1.12.0")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-extended")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    debugImplementation("androidx.compose.ui:ui-tooling")

    implementation("androidx.navigation:navigation-compose:2.8.5")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.7")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.8.7")
    implementation("androidx.activity:activity-compose:1.9.3")

    implementation("androidx.credentials:credentials:1.2.2")
    implementation("androidx.credentials:credentials-play-services-auth:1.2.2")
    implementation("com.google.android.libraries.identity.googleid:googleid:1.1.1")

    implementation("com.squareup.retrofit2:retrofit:2.11.0")
    implementation("com.squareup.okhttp3:okhttp:4.12.0")
    implementation("com.squareup.okhttp3:logging-interceptor:4.12.0")
    implementation("com.jakewharton.retrofit:retrofit2-kotlinx-serialization-converter:1.0.0")

    implementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.7.3")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.9.0")

    implementation("androidx.datastore:datastore-preferences:1.1.1")
    implementation("androidx.browser:browser:1.8.0")

    testImplementation("junit:junit:4.13.2")
}
