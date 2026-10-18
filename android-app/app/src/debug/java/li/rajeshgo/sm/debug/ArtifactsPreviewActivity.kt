package li.rajeshgo.sm.debug

import android.os.Bundle
import androidx.activity.enableEdgeToEdge
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.jsonObject
import li.rajeshgo.sm.ui.artifacts.ArtifactsContent
import li.rajeshgo.sm.ui.artifacts.ArtifactsState
import li.rajeshgo.sm.ui.navigation.AppMenuActions
import li.rajeshgo.sm.ui.theme.SessionManagerTheme

/** Offline visual fixture. No credentials, network writes, or production artifact actions. */
class ArtifactsPreviewActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge(statusBarStyle = androidx.activity.SystemBarStyle.dark(android.graphics.Color.TRANSPARENT), navigationBarStyle = androidx.activity.SystemBarStyle.dark(android.graphics.Color.TRANSPARENT))
        val fixture = Json.parseToJsonElement("""{
          "can_act":false,"backup":{"enabled":false},
          "filesystems":[
            {"path":"/Users/example/artifacts","bytes":4000000000000,"free":2300000000000,"floor":200000000000,"mounted":true},
            {"path":"/Volumes/Research/artifacts","bytes":4000000000000,"free":1900000000000,"floor":200000000000,"mounted":true}],
          "artifacts":[
            {"name":"2050_labels","state":"frozen","frozen_bytes":25600427493,"note":"Trade labels · 2011–2025"},
            {"name":"1978_labels","state":"frozen","frozen_bytes":12820191018,"note":"Earlier trade labels · 2011–2024"},
            {"name":"2002_labels","state":"frozen","frozen_bytes":4322501491,"note":"CME futures research"},
            {"name":"small_receipt","state":"live","disk_bytes":100000,"note":"Run receipt"},
            {"name":"unknown","state":"unrecorded"}]
        }""").jsonObject
        val showMarkdown = intent.getBooleanExtra("markdown", false)
        // The Backup card with a remote disk report (#2276).
        val backup = Json.parseToJsonElement("""{"enabled":true,"confirmed_at":"2026-10-17T09:35:00Z",
          "fetch_rate":18750000.0,"apply_rate":100000000.0,"probe_rate":25000000.0,"effective_rate":18750000.0,
          "card":{"nights":[{"date":"2026-10-11","ok":true},{"date":"2026-10-12","ok":true},{"date":"2026-10-13","ok":false,"error":"backup disk full: needs 5000000000 bytes"},
            {"date":"2026-10-14","ok":true},{"date":"2026-10-15","ok":true},{"date":"2026-10-16","ok":true},{"date":"2026-10-17","ok":true}],
            "last_night":{"bytes":2400000000,"seconds":1260,"confirmed_at":"2026-10-17T09:35:00Z"},
            "weekly_check":{"date":"2026-10-12","ok":true,"files_checked":1204,"bad":0},
            "disk":{"total_bytes":4000000000000,"free_bytes":1100000000000,"holding_bytes":310000000000,"holding_oldest":"2026-08-19","health":"failing","pruned_on":"2026-10-13"}}}""").jsonObject
        val base = if (intent.getBooleanExtra("backup", false)) kotlinx.serialization.json.JsonObject(fixture + ("backup" to backup)) else fixture
        val displayed = if (showMarkdown) {
            val row = fixture["artifacts"]!!.let { (it as kotlinx.serialization.json.JsonArray)[0] }.jsonObject
            val updated = kotlinx.serialization.json.JsonObject(row + ("description" to kotlinx.serialization.json.JsonPrimitive("## Retention\n\n**Keep** [source](https://example.com/source) and https://example.com/report.\n\n- Saved research inputs\n- Retain through the current experiment\n\n| File | Use |\n| --- | --- |\n| data | training |")))
            li.rajeshgo.sm.ui.artifacts.ArtifactsNavigation.name = "2050_labels"
            kotlinx.serialization.json.JsonObject(fixture + ("artifacts" to kotlinx.serialization.json.JsonArray(listOf(updated))))
        } else base
        setContent { SessionManagerTheme {
            androidx.compose.material3.Surface(color = androidx.compose.material3.MaterialTheme.colorScheme.background) {
            ArtifactsContent(ArtifactsState(data = displayed), { finish() }, AppMenuActions({}, {}, {}, {}, {}, {}), {}, { _, _ -> }, {})
            }
        } }
    }
}
