package li.rajeshgo.sm.ui.artifacts

import kotlinx.serialization.json.*
import org.junit.Assert.*
import org.junit.Test

class ArtifactsModelsTest {
    @Test fun fixtureSectionsAreExclusiveSortedAndFiltered() {
        val rows = Json.parseToJsonElement("""[
            {"name":"needs","state":"live","hot":true,"needs_owner":true,"disk_bytes":20,"epic":"one"},
            {"name":"small","state":"live","hot":true,"disk_bytes":10},
            {"name":"large","state":"live","hot":true,"disk_bytes":30},
            {"name":"frozen","state":"frozen","frozen_bytes":5},
            {"name":"deleted","state":"deleted","disk_bytes":50}
        ]""").jsonArray.map { it.jsonObject }
        val groups = artifactGroups(rows, "")
        assertEquals(listOf("Needs you","Hot","Frozen"),groups.map { it.first })
        assertEquals(listOf("large","small"),groups[1].second.map { it.text("name") })
        assertEquals(listOf("needs"),artifactGroups(rows,"one").flatMap { it.second }.map { it.text("name") })
    }
}
