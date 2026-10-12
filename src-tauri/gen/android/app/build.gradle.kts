import java.io.File
import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("rust")
}

val tauriProperties = Properties().apply {
    val propFile = file("tauri.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}

// 发布签名参数放 gen/android 下的 local.properties（本机已有）或 keystore.properties，二者都在
// gen/android/.gitignore 里。键名两套都认：本仓在用的 KEY_*，以及 Tauri 文档里的 storeFile/keyAlias/…
val signingProperties = Properties().apply {
    listOf("local.properties", "keystore.properties")
        .map { rootProject.file(it) }
        .filter { it.exists() }
        .forEach { file -> file.inputStream().use { load(it) } }
}

fun signingProp(name: String, vararg aliases: String): String? =
    (listOf(name) + aliases).firstNotNullOfOrNull { key ->
        signingProperties.getProperty(key)?.trim()?.takeIf { it.isNotEmpty() }
    }

val signingStoreFile = signingProp("KEYSTORE_FILE", "storeFile")
val signingStorePassword = signingProp("KEYSTORE_PW", "storePassword", "password")
val signingKeyAlias = signingProp("KEY_ALIAS", "keyAlias")
val signingKeyPassword = signingProp("KEY_PW", "keyPassword", "password")
// 任一项缺失就不建 release 签名配置：debug 构建与未配签名的机器照常能跑，release 产物退化为 unsigned
val hasReleaseSigning = signingStoreFile != null && signingStorePassword != null &&
    signingKeyAlias != null && signingKeyPassword != null

android {
    compileSdk = 36
    namespace = "com.x177plus.orbdrop"
    defaultConfig {
        manifestPlaceholders["usesCleartextTraffic"] = "false"
        applicationId = "com.x177plus.orbdrop"
        minSdk = 24
        targetSdk = 36
        versionCode = tauriProperties.getProperty("tauri.android.versionCode", "1").toInt()
        versionName = tauriProperties.getProperty("tauri.android.versionName", "1.0")
    }
    signingConfigs {
        if (hasReleaseSigning) {
            create("release") {
                // KEYSTORE_FILE 可以是相对 gen/android 的路径（本仓写的就是 keystore/release.jks）
                val storePath = requireNotNull(signingStoreFile)
                val storeFileCandidate = File(storePath)
                storeFile =
                    if (storeFileCandidate.isAbsolute) storeFileCandidate else rootProject.file(storePath)
                storePassword = signingStorePassword
                keyAlias = signingKeyAlias
                keyPassword = signingKeyPassword
            }
        }
    }
    buildTypes {
        getByName("debug") {
            manifestPlaceholders["usesCleartextTraffic"] = "true"
            isDebuggable = true
            isJniDebuggable = true
            isMinifyEnabled = false
            packaging {                jniLibs.keepDebugSymbols.add("*/arm64-v8a/*.so")
                jniLibs.keepDebugSymbols.add("*/armeabi-v7a/*.so")
                jniLibs.keepDebugSymbols.add("*/x86/*.so")
                jniLibs.keepDebugSymbols.add("*/x86_64/*.so")
            }
        }
        getByName("release") {
            if (hasReleaseSigning) {
                signingConfig = signingConfigs.getByName("release")
            }
            isMinifyEnabled = true
            // 库压缩存储（DEFLATE）+ 清单写 `extractNativeLibs="true"`：安装器在装的时候解压到
            // `/data/app/.../lib/<abi>/`（Android 6 以前所有应用的装法），系统安装器与我们的
            // 自更新链路（下 APK → FileProvider → 安装器）都不感知这件事。
            //
            // 换来的是下载体积减半：真机实测库 12.12 MiB → 5.23 MiB（43.2%）、通用包
            // 50.95 → 23.83 MiB、arm64 单包 14.66 → 7.76 MiB。代价只有两样，都量过：
            // 安装多花约 80ms（把 base.apk 拷进 /data 的那点拷贝省回来一部分）；
            // 设备上多一份解压后的库 —— **只有本机 ABI 那一份**会被解压，所以通用包
            // （4 个 ABI 都能压、设备只解压自己那份）连占用也少了 15 MiB；单 ABI 包则多占 5.6 MiB。
            //
            // 反面：Google 的默认建议是不压缩（安装更快、占用更小），哪天走 Play/AAB 那条路要关掉。
            packaging {
                jniLibs.useLegacyPackaging = true
            }
            proguardFiles(
                *fileTree(".") { include("**/*.pro") }
                    .plus(getDefaultProguardFile("proguard-android-optimize.txt"))
                    .toList().toTypedArray()
            )
        }
    }
    kotlinOptions {
        jvmTarget = "1.8"
    }
    buildFeatures {
        buildConfig = true
    }
}

rust {
    rootDirRel = "../../../"
}

dependencies {
    implementation("androidx.webkit:webkit:1.14.0")
    implementation("androidx.appcompat:appcompat:1.7.1")
    implementation("androidx.activity:activity-ktx:1.10.1")
    implementation("com.google.android.material:material:1.12.0")
    implementation("androidx.lifecycle:lifecycle-process:2.10.0")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.1.4")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.5.0")
}

apply(from = "tauri.build.gradle.kts")