package com.amurcanov.tgwsproxy.ui

import android.widget.Toast
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Update
import androidx.compose.material3.Button
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import com.amurcanov.tgwsproxy.AppReleaseInfo
import com.amurcanov.tgwsproxy.RemoteVersionSource
import com.amurcanov.tgwsproxy.downloadUpdateApk
import com.amurcanov.tgwsproxy.promptInstallApk
import kotlinx.coroutines.launch
import java.io.File

@Composable
fun AppUpdateDialog(
    release: AppReleaseInfo,
    onPostpone: () -> Unit,
    onUpdate: () -> Unit
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var isDownloading by remember { mutableStateOf(false) }
    var downloadProgress by remember { mutableIntStateOf(0) }
    var downloadedApk by remember { mutableStateOf<File?>(null) }
    var errorMessage by remember { mutableStateOf<String?>(null) }

    val isTagOnly = release.source == RemoteVersionSource.Tag
    val title = if (isTagOnly) {
        stringResource(com.amurcanov.tgwsproxy.R.string.update_new_tag_title)
    } else {
        stringResource(com.amurcanov.tgwsproxy.R.string.update_available_title)
    }
    val description = if (isTagOnly) {
        stringResource(com.amurcanov.tgwsproxy.R.string.update_new_tag_description, release.versionTag)
    } else {
        stringResource(com.amurcanov.tgwsproxy.R.string.update_available_description, release.versionTag)
    }

    Dialog(
        onDismissRequest = {},
        properties = DialogProperties(
            usePlatformDefaultWidth = false,
            dismissOnBackPress = !isDownloading,
            dismissOnClickOutside = false
        )
    ) {
        Surface(
            shape = RoundedCornerShape(28.dp),
            color = MaterialTheme.colorScheme.surface,
            tonalElevation = 8.dp,
            modifier = Modifier.fillMaxWidth(0.92f)
        ) {
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 22.dp, vertical = 20.dp),
                verticalArrangement = Arrangement.spacedBy(14.dp)
            ) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Icon(
                        imageVector = Icons.Default.Update,
                        contentDescription = null,
                        tint = MaterialTheme.colorScheme.primary,
                        modifier = Modifier.size(20.dp)
                    )
                    Spacer(Modifier.width(10.dp))
                    Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                        Text(
                            text = title,
                            style = MaterialTheme.typography.titleMedium,
                            fontWeight = FontWeight.Bold,
                            color = MaterialTheme.colorScheme.primary
                        )
                        Text(
                            text = release.versionTag,
                            style = MaterialTheme.typography.bodyMedium,
                            fontWeight = FontWeight.SemiBold,
                            color = MaterialTheme.colorScheme.onSurfaceVariant
                        )
                    }
                }

                Text(
                    text = description,
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    lineHeight = 20.sp
                )

                if (isDownloading) {
                    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                        LinearProgressIndicator(
                            progress = { (downloadProgress / 100f).coerceIn(0f, 1f) },
                            modifier = Modifier.fillMaxWidth()
                        )
                        Text(
                            text = "Скачивание APK: $downloadProgress%",
                            style = MaterialTheme.typography.labelMedium,
                            color = MaterialTheme.colorScheme.primary,
                            fontWeight = FontWeight.SemiBold
                        )
                    }
                }

                errorMessage?.let { err ->
                    Text(
                        text = err,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.error
                    )
                }

                HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant.copy(alpha = 0.35f))

                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.spacedBy(12.dp)
                ) {
                    OutlinedButton(
                        onClick = onPostpone,
                        enabled = !isDownloading,
                        modifier = Modifier
                            .weight(1f)
                            .height(50.dp),
                        shape = RoundedCornerShape(22.dp)
                    ) {
                        Text(
                            text = stringResource(com.amurcanov.tgwsproxy.R.string.later),
                            fontWeight = FontWeight.SemiBold
                        )
                    }

                    Button(
                        onClick = {
                            val readyFile = downloadedApk
                            if (readyFile != null && readyFile.exists()) {
                                val started = promptInstallApk(context, readyFile)
                                if (started) {
                                    onUpdate()
                                } else {
                                    Toast.makeText(
                                        context,
                                        "Разрешите установку из этого источника и нажмите «Установить» ещё раз",
                                        Toast.LENGTH_LONG
                                    ).show()
                                }
                            } else if (!isDownloading) {
                                isDownloading = true
                                errorMessage = null
                                downloadProgress = 0
                                scope.launch {
                                    val res = downloadUpdateApk(context, release) { pct ->
                                        downloadProgress = pct
                                    }
                                    isDownloading = false
                                    res.onSuccess { file ->
                                        downloadedApk = file
                                        val started = promptInstallApk(context, file)
                                        if (started) {
                                            onUpdate()
                                        } else {
                                            Toast.makeText(
                                                context,
                                                "Разрешите установку из этого источника и нажмите «Установить»",
                                                Toast.LENGTH_LONG
                                            ).show()
                                        }
                                    }.onFailure {
                                        errorMessage = "Не удалось скачать APK напрямую, открываем страницу релиза…"
                                        openUrlInBrowser(context, release.releaseUrl)
                                        onUpdate()
                                    }
                                }
                            }
                        },
                        enabled = !isDownloading,
                        modifier = Modifier
                            .weight(1f)
                            .height(50.dp),
                        shape = RoundedCornerShape(22.dp)
                    ) {
                        Text(
                            text = when {
                                downloadedApk != null -> "Установить"
                                isDownloading -> "$downloadProgress%"
                                else -> stringResource(com.amurcanov.tgwsproxy.R.string.update_action)
                            },
                            fontWeight = FontWeight.Bold
                        )
                    }
                }

                if (downloadedApk != null || errorMessage != null) {
                    TextButton(
                        onClick = {
                            openUrlInBrowser(context, release.releaseUrl)
                            onUpdate()
                        },
                        modifier = Modifier.align(Alignment.End)
                    ) {
                        Text("Открыть релиз на GitHub")
                    }
                }
            }
        }
    }
}
