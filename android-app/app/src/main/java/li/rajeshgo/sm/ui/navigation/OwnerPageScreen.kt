package li.rajeshgo.sm.ui.navigation

import android.widget.Toast
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import li.rajeshgo.sm.data.repository.SettingsRepository
import li.rajeshgo.sm.ui.watch.DocReaderOverlay
import li.rajeshgo.sm.ui.watch.ReaderPage
import li.rajeshgo.sm.ui.watch.loadDocReaderAuth

/**
 * A menu page shown through the reader until it has a native screen
 * (History and the Guestbook, sm#1660 and sm#1661). Back returns to the tab it
 * was opened from.
 */
@Composable
fun OwnerPageScreen(page: ReaderPage, onBack: () -> Unit) {
    val context = LocalContext.current
    val settingsRepository = remember { SettingsRepository(context) }
    Box(
        Modifier.fillMaxSize().statusBarsPadding().navigationBarsPadding()
            .background(MaterialTheme.colorScheme.background),
    ) {
        DocReaderOverlay(
            page = page,
            loadAuth = { loadDocReaderAuth(settingsRepository) },
            onClose = onBack,
            onCopyLink = { link ->
                val clipboard = context.getSystemService(android.content.ClipboardManager::class.java)
                clipboard?.setPrimaryClip(android.content.ClipData.newPlainText("sm link", link))
                Toast.makeText(context, "Link copied", Toast.LENGTH_SHORT).show()
            },
        )
    }
}
