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
            {"path":"/Users/example/artifacts","role":"root","bytes":4000000000000,"free":1050000000000,"floor":200000000000,"mounted":true},
            {"path":"/Volumes/Research/artifacts","role":"cold","bytes":4000000000000,"free":2210000000000,"floor":200000000000,"mounted":true}],
          "artifacts":[
            {"name":"2050_labels","state":"frozen","data_bytes":29394172947,"frozen_bytes":25600427493,"saved_bytes":3793745454,"thaw_eta_seconds":18,"note":"Trade labels · 2011–2025",
             "placement":{"disk":"cold","disk_path":"/Volumes/Research/artifacts","path":"/Volumes/Research/artifacts/.frozen/2050_labels"},
             "last_thaw":{"at":"2026-10-18T20:11:11Z","seconds":21.4,"data_bytes":29394172947}},
            {"name":"1978_labels","state":"frozen","frozen_bytes":12820191018,"note":"Earlier trade labels · 2011–2024"},
            {"name":"2002_labels","state":"frozen","frozen_bytes":4322501491,"note":"CME futures research"},
            {"name":"small_receipt","state":"live","disk_bytes":100000,"note":"Run receipt"},
            {"name":"unknown","state":"unrecorded"}]
        }""").jsonObject
        val showMarkdown = intent.getBooleanExtra("markdown", false)
        // Freezes in flight, one just done, history and savings (sm#2325).
        val activity = Json.parseToJsonElement("""{
          "operations":[
            {"id":59,"kind":"freezing","phase":"done","state":"done","names":["2051_money"],"seconds":531,"data_bytes":344195152884,"frozen_bytes":37642525705,"actor":"71a9e65a","actor_name":"far-2203","created_at":"2026-10-19T00:28:15Z","finished_at":"2026-10-19T00:37:06Z"},
            {"id":60,"kind":"freezing","phase":"copying","state":"running","names":["1977_chains"],"behind":1,"expected_seconds":900,"elapsed_seconds":528,"remaining_seconds":372,"data_bytes":293667123301,"actor":"71a9e65a","actor_name":"far-2203","created_at":"2026-10-19T00:38:15Z"},
            {"id":61,"kind":"thawing","phase":"queued","state":"waiting","names":["2002_labels"],"ahead":1,"starts_in_seconds":372,"remaining_seconds":400,"data_bytes":4942934971,"actor":"owner@example.com","actor_name":"Rajesh","created_at":"2026-10-19T00:39:02Z"}],
          "history":[
            {"id":59,"kind":"freezing","state":"done","names":["2051_money"],"seconds":531,"data_bytes":344195152884,"frozen_bytes":37642525705,"actor":"71a9e65a","actor_name":"far-2203","finished_at":"2026-10-19T00:37:06Z"},
            {"id":58,"kind":"freezing","state":"done","names":["1978_summaries"],"seconds":402,"data_bytes":209408626304,"frozen_bytes":183648591631,"actor":"71a9e65a","actor_name":"far-2203","finished_at":"2026-10-19T00:35:09Z"},
            {"id":52,"kind":"thawing","state":"done","names":["2002_chains"],"seconds":173,"data_bytes":372581394417,"actor":"71a9e65a","actor_name":"far-2203","finished_at":"2026-10-18T20:14:24Z"},
            {"id":44,"kind":"freezing","state":"done","names":["2140_e5"],"seconds":6,"data_bytes":7066222375,"frozen_bytes":1156350135,"actor":"sm-nightly","actor_name":"sm nightly","finished_at":"2026-10-17T11:17:25Z"},
            {"id":43,"kind":"thawing","state":"failed","names":["1975_chains"],"error":"cold store is not connected","actor":"sm","actor_name":"sm","finished_at":"2026-10-16T23:45:38Z"},
            {"id":42,"kind":"thawing","state":"done","names":["1962_chains"],"seconds":61,"data_bytes":137796537522,"actor":"f167d142","finished_at":"2026-10-16T23:38:45Z"}],
          "savings":{"folders":41,"data_bytes":3016153560409,"frozen_bytes":476830448887,"saved_bytes":2539323111522,
            "roots":[{"root":"/Users/example/artifacts","cold_store":"/Volumes/Research/artifacts","folders":41}]}
        }""").jsonObject
        // The Backup card with a remote disk report (#2276).
        val backup = Json.parseToJsonElement("""{"enabled":true,"confirmed_at":"2026-10-17T09:35:00Z",
          "fetch_rate":18750000.0,"apply_rate":100000000.0,"probe_rate":25000000.0,"effective_rate":18750000.0,
          "card":{"nights":[{"date":"2026-10-11","ok":true},{"date":"2026-10-12","ok":true},{"date":"2026-10-13","ok":false,"error":"backup disk full: needs 5000000000 bytes"},
            {"date":"2026-10-14","ok":true},{"date":"2026-10-15","ok":true},{"date":"2026-10-16","ok":true},{"date":"2026-10-17","ok":true}],
            "last_night":{"bytes":2400000000,"seconds":1260,"confirmed_at":"2026-10-17T09:35:00Z"},
            "weekly_check":{"date":"2026-10-12","ok":true,"files_checked":1204,"bad":0},"measured":{"fetch":true,"apply":true,"probe":true},
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
            ArtifactsContent(ArtifactsState(data = displayed, activity = activity), { finish() }, AppMenuActions({}, {}, {}, {}, {}, {}), {}, { _, _ -> }, {})
            }
        } }
    }
}
