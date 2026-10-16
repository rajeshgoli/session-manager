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
        val displayed = if (showMarkdown) {
            val row = fixture["artifacts"]!!.let { (it as kotlinx.serialization.json.JsonArray)[0] }.jsonObject
            val updated = kotlinx.serialization.json.JsonObject(row + ("description" to kotlinx.serialization.json.JsonPrimitive("## Retention\n\n**Keep** [source](https://example.com/source) and https://example.com/report.\n\n- Saved research inputs\n- Retain through the current experiment\n\n| File | Use |\n| --- | --- |\n| data | training |")))
            li.rajeshgo.sm.ui.artifacts.ArtifactsNavigation.name = "2050_labels"
            kotlinx.serialization.json.JsonObject(fixture + ("artifacts" to kotlinx.serialization.json.JsonArray(listOf(updated))))
        } else fixture
        setContent { SessionManagerTheme {
            androidx.compose.material3.Surface(color = androidx.compose.material3.MaterialTheme.colorScheme.background) {
            ArtifactsContent(ArtifactsState(data = displayed), { finish() }, AppMenuActions({}, {}, {}, {}, {}, {}), {}, { _, _ -> }, {})
            }
        } }
    }
}
