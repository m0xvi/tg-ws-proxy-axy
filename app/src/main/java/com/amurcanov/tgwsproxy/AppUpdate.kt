package com.amurcanov.tgwsproxy

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.provider.Settings
import android.util.Log
import androidx.core.content.FileProvider
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.io.FileOutputStream
import java.net.HttpURLConnection
import java.net.URL

const val UPDATE_CHECK_NEVER = -1
const val DEFAULT_UPDATE_CHECK_INTERVAL_HOURS = 12
private const val UPDATE_LOG_TAG = "TgWsProxy"
private const val GITHUB_RELEASES_URL = "https://api.github.com/repos/m0xvi/tg-ws-proxy-axy/releases?per_page=30"
private const val GITHUB_LATEST_RELEASE_URL = "https://api.github.com/repos/m0xvi/tg-ws-proxy-axy/releases/latest"
private const val GITHUB_LATEST_RELEASE_WEB_URL = "https://github.com/m0xvi/tg-ws-proxy-axy/releases/latest"
private const val GITHUB_RELEASE_TAG_URL_PREFIX = "https://github.com/m0xvi/tg-ws-proxy-axy/releases/tag/"
private const val GITHUB_RELEASE_DOWNLOAD_PREFIX = "https://github.com/m0xvi/tg-ws-proxy-axy/releases/download/"
private const val GITHUB_TAGS_URL = "https://api.github.com/repos/m0xvi/tg-ws-proxy-axy/tags?per_page=100"
private const val GITHUB_TAG_TREE_URL_PREFIX = "https://github.com/m0xvi/tg-ws-proxy-axy/tree/"
private const val GITHUB_API_RATE_LIMIT_FALLBACK_MS = 30L * 60L * 1000L
private val VERSION_NUMBER_REGEX = Regex("\\d+(?:\\.\\d+)*")
@Volatile
private var githubApiCooldownUntilMs = 0L

fun updateIntervalHoursToMillis(hours: Int): Long? = when {
    hours <= 0 -> null
    else -> hours * 60L * 60L * 1000L
}

fun updateIntervalLabel(context: Context, hours: Int): String = when (hours) {
    7 -> context.getString(R.string.interval_7h)
    24 -> context.getString(R.string.interval_24h)
    48 -> context.getString(R.string.interval_48h)
    UPDATE_CHECK_NEVER -> context.getString(R.string.interval_never)
    else -> context.getString(R.string.interval_hours, hours)
}

fun updateIntervalLabel(hours: Int): String = when (hours) {
    7 -> "7 h"
    24 -> "24 h"
    48 -> "48 h"
    UPDATE_CHECK_NEVER -> "Never"
    else -> "$hours h"
}

fun defaultApkDownloadUrl(versionTag: String): String {
    val tag = normalizeVersionTag(versionTag)
    val preferredApk = if (Build.SUPPORTED_64_BIT_ABIS.isNotEmpty()) {
        "app-arm64-release.apk"
    } else {
        "app-universal-release.apk"
    }
    return "$GITHUB_RELEASE_DOWNLOAD_PREFIX$tag/$preferredApk"
}

data class AppReleaseInfo(
    val versionTag: String,
    val releaseUrl: String,
    val source: RemoteVersionSource,
    val apkDownloadUrl: String = defaultApkDownloadUrl(versionTag)
)

enum class RemoteVersionSource {
    Release,
    Tag
}

const val UPDATE_DIALOG_ACTION_POSTPONED = "postponed"
const val UPDATE_DIALOG_ACTION_UPDATE = "update"

suspend fun fetchLatestReleaseInfo(localVersion: String? = null): AppReleaseInfo? = withContext(Dispatchers.IO) {
    val latestRelease = fetchReleaseFromLatestEndpoint()
        ?: fetchReleaseFromLatestWebRedirect()
        ?: fetchLatestStableReleaseFromList()
    val latestTag = fetchLatestTagFromList()

    when {
        latestRelease == null -> latestTag
        latestTag == null -> latestRelease
        isNewerVersion(latestRelease.versionTag, latestTag.versionTag) -> latestTag
        else -> latestRelease
    }
}

suspend fun downloadUpdateApk(
    context: Context,
    release: AppReleaseInfo,
    onProgress: (Int) -> Unit
): Result<File> = withContext(Dispatchers.IO) {
    val updatesDir = File(context.cacheDir, "updates").apply { mkdirs() }
    val safeTag = release.versionTag.replace(Regex("[^a-zA-Z0-9._-]"), "_")
    val targetFile = File(updatesDir, "tg-ws-proxy-axy-$safeTag.apk")
    val tempFile = File(updatesDir, "tg-ws-proxy-axy-$safeTag.apk.part")

    val candidateUrls = listOf(
        release.apkDownloadUrl,
        "$GITHUB_RELEASE_DOWNLOAD_PREFIX${normalizeVersionTag(release.versionTag)}/app-universal-release.apk",
        "$GITHUB_RELEASE_DOWNLOAD_PREFIX${normalizeVersionTag(release.versionTag)}/app-arm64-release.apk"
    ).distinct().filter { it.isNotBlank() }

    var lastError: Throwable? = null
    for (candidateUrl in candidateUrls) {
        try {
            downloadFileWithRedirects(candidateUrl, tempFile, onProgress)
            if (tempFile.exists() && tempFile.length() > 100_000L) {
                if (targetFile.exists()) targetFile.delete()
                tempFile.renameTo(targetFile)
                onProgress(100)
                return@withContext Result.success(targetFile)
            }
        } catch (e: Throwable) {
            lastError = e
            Log.w(UPDATE_LOG_TAG, "[WARN] Update APK download failed from $candidateUrl", e)
        } finally {
            if (tempFile.exists()) tempFile.delete()
        }
    }

    Result.failure(lastError ?: IllegalStateException("Не удалось скачать APK обновления"))
}

private fun downloadFileWithRedirects(
    initialUrl: String,
    destFile: File,
    onProgress: (Int) -> Unit
) {
    var currentUrl = initialUrl
    var redirects = 0
    while (redirects < 8) {
        val conn = (URL(currentUrl).openConnection() as HttpURLConnection).apply {
            instanceFollowRedirects = false
            requestMethod = "GET"
            setRequestProperty("Accept", "application/octet-stream,*/*")
            setRequestProperty("User-Agent", "TGWSProxyAXY/${BuildConfig.VERSION_NAME}")
            connectTimeout = 15_000
            readTimeout = 30_000
        }
        try {
            val code = conn.responseCode
            if (code in listOf(301, 302, 303, 307, 308)) {
                val loc = conn.getHeaderField("Location")
                    ?: throw IllegalStateException("Redirect without Location header")
                currentUrl = URL(URL(currentUrl), loc).toString()
                redirects++
                continue
            }
            if (code !in 200..299) {
                throw IllegalStateException("HTTP $code при скачивании APK")
            }
            val totalBytes = conn.contentLengthLong
            var downloadedBytes = 0L
            var lastReportedPct = -1
            conn.inputStream.use { input ->
                FileOutputStream(destFile).use { output ->
                    val buffer = ByteArray(16 * 1024)
                    while (true) {
                        val read = input.read(buffer)
                        if (read <= 0) break
                        output.write(buffer, 0, read)
                        downloadedBytes += read
                        if (totalBytes > 0L) {
                            val pct = ((downloadedBytes * 100L) / totalBytes).toInt().coerceIn(0, 99)
                            if (pct != lastReportedPct) {
                                lastReportedPct = pct
                                onProgress(pct)
                            }
                        }
                    }
                    output.flush()
                }
            }
            return
        } finally {
            conn.disconnect()
        }
    }
    throw IllegalStateException("Слишком много перенаправлений при скачивании APK")
}

fun promptInstallApk(context: Context, apkFile: File): Boolean {
    return try {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O && !context.packageManager.canRequestPackageInstalls()) {
            val permIntent = Intent(
                Settings.ACTION_MANAGE_UNKNOWN_APP_SOURCES,
                Uri.parse("package:${context.packageName}")
            ).apply {
                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            }
            context.startActivity(permIntent)
            false
        } else {
            val apkUri = FileProvider.getUriForFile(
                context,
                "${context.packageName}.fileprovider",
                apkFile
            )
            val installIntent = Intent(Intent.ACTION_VIEW).apply {
                setDataAndType(apkUri, "application/vnd.android.package-archive")
                addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
            }
            context.startActivity(installIntent)
            true
        }
    } catch (e: Exception) {
        Log.e(UPDATE_LOG_TAG, "Failed to launch APK installer", e)
        false
    }
}

fun isNewerVersion(local: String, remote: String): Boolean {
    val localParts = versionParts(local)
    val remoteParts = versionParts(remote)
    if (remoteParts.isEmpty()) return false

    val maxLen = maxOf(localParts.size, remoteParts.size)

    for (i in 0 until maxLen) {
        val l = localParts.getOrElse(i) { 0 }
        val r = remoteParts.getOrElse(i) { 0 }
        if (r > l) return true
        if (r < l) return false
    }

    return false
}

private fun fetchLatestStableReleaseFromList(): AppReleaseInfo? {
    val response = fetchGitHubApi(GITHUB_RELEASES_URL) ?: return null
    val releases = try {
        JSONArray(response)
    } catch (e: Exception) {
        Log.w(UPDATE_LOG_TAG, "[WARN] Update check: failed to parse releases list", e)
        return null
    }

    var bestRelease: AppReleaseInfo? = null
    for (i in 0 until releases.length()) {
        val json = releases.optJSONObject(i) ?: continue
        if (json.optBoolean("draft") || json.optBoolean("prerelease")) continue

        val release = json.toAppReleaseInfo() ?: continue
        if (bestRelease == null || isNewerVersion(bestRelease.versionTag, release.versionTag)) {
            bestRelease = release
        }
    }

    return bestRelease
}

private fun fetchLatestTagFromList(): AppReleaseInfo? {
    val response = fetchGitHubApi(GITHUB_TAGS_URL) ?: return null
    val tags = try {
        JSONArray(response)
    } catch (e: Exception) {
        Log.w(UPDATE_LOG_TAG, "[WARN] Update check: failed to parse tags list", e)
        return null
    }

    var bestTag: AppReleaseInfo? = null
    for (i in 0 until tags.length()) {
        val json = tags.optJSONObject(i) ?: continue
        val tagName = normalizeVersionTag(json.optString("name"))
        if (tagName.isBlank()) continue

        val tag = AppReleaseInfo(
            versionTag = tagName,
            releaseUrl = "$GITHUB_TAG_TREE_URL_PREFIX$tagName",
            source = RemoteVersionSource.Tag
        )
        if (bestTag == null || isNewerVersion(bestTag.versionTag, tag.versionTag)) {
            bestTag = tag
        }
    }

    return bestTag
}

private fun fetchReleaseFromLatestEndpoint(): AppReleaseInfo? {
    val response = fetchGitHubApi(GITHUB_LATEST_RELEASE_URL) ?: return null
    val json = try {
        JSONObject(response)
    } catch (e: Exception) {
        Log.w(UPDATE_LOG_TAG, "[WARN] Update check: failed to parse latest release", e)
        return null
    }

    return json.toAppReleaseInfo()
}

private fun fetchReleaseFromLatestWebRedirect(): AppReleaseInfo? {
    var conn: HttpURLConnection? = null
    return try {
        conn = URL(GITHUB_LATEST_RELEASE_WEB_URL).openConnection() as HttpURLConnection
        applyNoCacheHeaders(conn)
        conn.instanceFollowRedirects = false
        conn.requestMethod = "GET"
        conn.setRequestProperty("Accept", "text/html,*/*")
        conn.setRequestProperty("User-Agent", "TGWSProxyAXY/${BuildConfig.VERSION_NAME}")
        conn.connectTimeout = 8_000
        conn.readTimeout = 8_000

        val responseCode = conn.responseCode
        val location = conn.getHeaderField("Location")
        if (!location.isNullOrBlank()) {
            val releaseUrl = URL(URL(GITHUB_LATEST_RELEASE_WEB_URL), location).toString()
            val versionTag = extractTagFromReleaseUrl(releaseUrl)
            if (!versionTag.isNullOrBlank()) {
                return AppReleaseInfo(
                    versionTag = versionTag,
                    releaseUrl = releaseUrl,
                    source = RemoteVersionSource.Release,
                    apkDownloadUrl = defaultApkDownloadUrl(versionTag)
                )
            }
        }

        if (responseCode in 200..299) {
            val response = conn.inputStream.bufferedReader().use { it.readText() }
            val versionTag = Regex("/releases/tag/([^\"?#<]+)").find(response)?.groupValues?.getOrNull(1)
            if (!versionTag.isNullOrBlank()) {
                return AppReleaseInfo(
                    versionTag = versionTag,
                    releaseUrl = "$GITHUB_RELEASE_TAG_URL_PREFIX$versionTag",
                    source = RemoteVersionSource.Release,
                    apkDownloadUrl = defaultApkDownloadUrl(versionTag)
                )
            }
        }

        Log.w(UPDATE_LOG_TAG, "[WARN] Update check: GitHub web fallback returned $responseCode")
        null
    } catch (e: Exception) {
        Log.w(UPDATE_LOG_TAG, "[WARN] Update check: GitHub web fallback failed", e)
        null
    } finally {
        conn?.disconnect()
    }
}

private fun fetchGitHubApi(url: String): String? {
    val now = System.currentTimeMillis()
    if (now < githubApiCooldownUntilMs) {
        return null
    }

    return fetchHttpText(
        url = url,
        sourceLabel = "GitHub API",
        accept = "application/vnd.github+json",
        isGitHubApi = true
    )
}

private fun fetchHttpText(
    url: String,
    sourceLabel: String,
    accept: String,
    isGitHubApi: Boolean = false
): String? {
    var conn: HttpURLConnection? = null
    return try {
        conn = URL(url).openConnection() as HttpURLConnection
        applyNoCacheHeaders(conn)
        conn.requestMethod = "GET"
        conn.setRequestProperty("Accept", accept)
        if (isGitHubApi) {
            conn.setRequestProperty("X-GitHub-Api-Version", "2022-11-28")
        }
        conn.setRequestProperty("User-Agent", "TGWSProxyAXY/${BuildConfig.VERSION_NAME}")
        conn.connectTimeout = 8_000
        conn.readTimeout = 8_000

        val responseCode = conn.responseCode
        val stream = if (responseCode in 200..299) conn.inputStream else conn.errorStream
        val response = stream?.bufferedReader()?.use { it.readText() }.orEmpty()

        if (responseCode in 200..299) {
            if (isGitHubApi) {
                githubApiCooldownUntilMs = 0L
            }
            response
        } else {
            if (isGitHubApi) {
                noteGitHubApiCooldown(conn, responseCode, response)
            }
            Log.w(
                UPDATE_LOG_TAG,
                "[WARN] Update check: $sourceLabel returned $responseCode ${response.take(300)}"
            )
            null
        }
    } catch (e: Exception) {
        Log.w(UPDATE_LOG_TAG, "[WARN] Update check: $sourceLabel request failed", e)
        null
    } finally {
        conn?.disconnect()
    }
}

private fun applyNoCacheHeaders(conn: HttpURLConnection) {
    conn.useCaches = false
    conn.setRequestProperty("Cache-Control", "no-cache, no-store, max-age=0")
    conn.setRequestProperty("Pragma", "no-cache")
    conn.setRequestProperty("Expires", "0")
}

private fun noteGitHubApiCooldown(conn: HttpURLConnection, responseCode: Int, response: String) {
    if (responseCode != HttpURLConnection.HTTP_FORBIDDEN && responseCode != 429) {
        return
    }

    val now = System.currentTimeMillis()
    val retryAfterUntil = conn
        .getHeaderField("Retry-After")
        ?.trim()
        ?.toLongOrNull()
        ?.takeIf { it > 0L }
        ?.let { now + it * 1000L }
    val rateLimitResetUntil = conn
        .getHeaderField("X-RateLimit-Reset")
        ?.trim()
        ?.toLongOrNull()
        ?.takeIf { it > 0L }
        ?.let { it * 1000L }
    val fallbackUntil = now + if (response.contains("rate limit", ignoreCase = true)) {
        GITHUB_API_RATE_LIMIT_FALLBACK_MS
    } else {
        5L * 60L * 1000L
    }

    val cooldownUntil = listOfNotNull(retryAfterUntil, rateLimitResetUntil)
        .filter { it > now }
        .minOrNull()
        ?: fallbackUntil

    if (cooldownUntil > githubApiCooldownUntilMs) {
        githubApiCooldownUntilMs = cooldownUntil
        Log.w(
            UPDATE_LOG_TAG,
            "[WARN] Update check: GitHub API cooldown ${(cooldownUntil - now) / 1000}s after HTTP $responseCode"
        )
    }
}

private fun JSONObject.toAppReleaseInfo(): AppReleaseInfo? {
    val versionTag = normalizeVersionTag(optString("tag_name"))
    val releaseUrl = optString("html_url").trim()
    if (versionTag.isBlank() || releaseUrl.isBlank()) return null

    var selectedApkUrl: String? = null
    val preferArm64 = Build.SUPPORTED_64_BIT_ABIS.isNotEmpty()
    val assets = optJSONArray("assets")
    if (assets != null) {
        val assetUrls = mutableMapOf<String, String>()
        for (i in 0 until assets.length()) {
            val asset = assets.optJSONObject(i) ?: continue
            val name = asset.optString("name").trim()
            val url = asset.optString("browser_download_url").trim()
            if (name.endsWith(".apk", ignoreCase = true) && url.isNotBlank()) {
                assetUrls[name] = url
            }
        }
        selectedApkUrl = if (preferArm64) {
            assetUrls["app-arm64-release.apk"]
                ?: assetUrls["app-universal-release.apk"]
                ?: assetUrls.values.firstOrNull()
        } else {
            assetUrls["app-arm32-release.apk"]
                ?: assetUrls["app-universal-release.apk"]
                ?: assetUrls.values.firstOrNull()
        }
    }

    return AppReleaseInfo(
        versionTag = versionTag,
        releaseUrl = releaseUrl,
        source = RemoteVersionSource.Release,
        apkDownloadUrl = selectedApkUrl ?: defaultApkDownloadUrl(versionTag)
    )
}

private fun versionParts(version: String): List<Int> {
    val normalized = VERSION_NUMBER_REGEX.find(version.trim())?.value ?: return emptyList()
    return normalized.split(".").mapNotNull { it.toIntOrNull() }
}

private fun normalizeVersionTag(version: String): String {
    val trimmed = version.trim()
    if (trimmed.isBlank()) return ""
    return if (trimmed.startsWith("v", ignoreCase = true)) trimmed else "v$trimmed"
}

private fun extractTagFromReleaseUrl(releaseUrl: String): String? {
    val marker = "/releases/tag/"
    val index = releaseUrl.indexOf(marker)
    if (index < 0) return null

    return releaseUrl
        .substring(index + marker.length)
        .substringBefore("?")
        .substringBefore("#")
        .substringBefore("/")
        .takeIf { it.isNotBlank() }
        ?.let(::normalizeVersionTag)
}
