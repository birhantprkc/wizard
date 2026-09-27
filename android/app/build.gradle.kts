plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.compose)
    alias(libs.plugins.kotlin.serialization)
    alias(libs.plugins.paparazzi)
    alias(libs.plugins.baselineprofile)
}

// Release signing comes from the environment so the keystore and its passwords
// never sit in the repository. The release workflow sets all four. A local build
// with none of them set signs with this machine's debug key, as before; a build
// with only some of them set fails, because falling back to the debug key there
// would hide a broken setup until the APK refused to install over the last one.
val releaseSigningVars = listOf(
    "WIZARD_ANDROID_KEYSTORE",
    "WIZARD_ANDROID_KEYSTORE_PASSWORD",
    "WIZARD_ANDROID_KEY_ALIAS",
    "WIZARD_ANDROID_KEY_PASSWORD",
)
val releaseSigning = releaseSigningVars.associateWith { name ->
    providers.environmentVariable(name).orNull?.takeIf { it.isNotEmpty() }
}
val hasReleaseKey = releaseSigning.values.any { it != null }
if (hasReleaseKey) {
    val missing = releaseSigning.filterValues { it == null }.keys
    if (missing.isNotEmpty()) {
        throw GradleException("Release signing is half configured: ${missing.joinToString()} not set")
    }
    val keystore = file(releaseSigning.getValue("WIZARD_ANDROID_KEYSTORE")!!)
    if (!keystore.isFile) {
        throw GradleException("WIZARD_ANDROID_KEYSTORE points at $keystore, which does not exist")
    }
} else {
    logger.lifecycle("WIZARD_ANDROID_KEYSTORE is not set: release builds are signed with the debug key")
}

// The app ships with Wizard, so it carries Wizard's version: -Pwizard.version
// from the release workflow, otherwise the root Cargo.toml's (read the way
// release.yml's tag check reads it), so a local build reports the tree it came
// from. versionCode is derived from it and has to grow with every release, or
// Android refuses the APK as a downgrade; MAJOR*10000 + MINOR*100 + PATCH does,
// as long as minor and patch stay under 100.
val wizardVersion: String = providers.gradleProperty("wizard.version").orNull
    ?: rootDir.resolve("../Cargo.toml").takeIf { it.isFile }?.let { cargo ->
        Regex("""^version\s*=\s*"([^"]+)"""", RegexOption.MULTILINE).find(cargo.readText())?.groupValues?.get(1)
    }
    ?: "0.0.0"
val wizardVersionCode: Int = run {
    val parts = Regex("""(\d+)\.(\d+)\.(\d+)""").matchEntire(wizardVersion)?.groupValues?.drop(1)?.map(String::toInt)
        ?: throw GradleException("Wizard version must be MAJOR.MINOR.PATCH, got \"$wizardVersion\"")
    val (major, minor, patch) = parts
    if (minor > 99 || patch > 99) {
        throw GradleException("Wizard version $wizardVersion does not fit versionCode's MAJOR*10000 + MINOR*100 + PATCH")
    }
    major * 10000 + minor * 100 + patch
}

android {
    namespace = "com.teddytennant.wizard"
    compileSdk = 37
    buildToolsVersion = "36.1.0"

    defaultConfig {
        applicationId = "com.teddytennant.wizard"
        minSdk = 29
        targetSdk = 36
        versionCode = wizardVersionCode
        versionName = wizardVersion
    }

    signingConfigs {
        if (hasReleaseKey) {
            create("release") {
                storeFile = file(releaseSigning.getValue("WIZARD_ANDROID_KEYSTORE")!!)
                storePassword = releaseSigning.getValue("WIZARD_ANDROID_KEYSTORE_PASSWORD")
                keyAlias = releaseSigning.getValue("WIZARD_ANDROID_KEY_ALIAS")
                keyPassword = releaseSigning.getValue("WIZARD_ANDROID_KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            signingConfig = signingConfigs.getByName(if (hasReleaseKey) "release" else "debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        compose = true
    }

    packaging {
        resources {
            excludes += setOf(
                "META-INF/versions/9/OSGI-INF/MANIFEST.MF",
                "META-INF/DEPENDENCIES",
                "META-INF/LICENSE.md",
                "META-INF/NOTICE.md",
            )
        }
    }

    lint {
        warningsAsErrors = false
        abortOnError = true
    }

    testOptions {
        unitTests.isReturnDefaultValues = true
    }
}

dependencies {
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.navigation.compose)
    implementation(libs.androidx.lifecycle.runtime.compose)
    implementation(libs.androidx.lifecycle.viewmodel.compose)
    implementation(libs.androidx.lifecycle.process)
    implementation(libs.androidx.splashscreen)
    implementation(libs.androidx.datastore.preferences)
    implementation(platform(libs.compose.bom))
    implementation(libs.compose.ui)
    implementation(libs.compose.foundation)
    implementation(libs.compose.material3)
    implementation(libs.compose.material.icons)
    implementation(libs.compose.ui.tooling.preview)
    debugImplementation(libs.compose.ui.tooling)
    implementation(libs.kotlinx.serialization.json)
    implementation(libs.kotlinx.coroutines.android)
    implementation(libs.sshj)
    implementation(libs.bouncycastle.prov)
    implementation(libs.bouncycastle.pkix)
    implementation(libs.slf4j.nop)
    implementation(libs.androidx.profileinstaller)
    baselineProfile(project(":baselineprofile"))

    testImplementation(libs.junit)
    testImplementation(libs.kotlinx.coroutines.test)
}
