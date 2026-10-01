package li.rajeshgo.sm.ui.theme

import androidx.compose.ui.unit.TextUnit
import androidx.compose.ui.unit.sp
import li.rajeshgo.sm.data.repository.normalizeTextScale
import org.junit.Assert.assertEquals
import org.junit.Assert.assertSame
import org.junit.Test

/** Settings › Appearance › Text size (spec 1782 A1). */
class TextScaleTest {
    @Test
    fun scaleIsEightyFiveToOneThirtyInFivePercentStepsDefaultingToOneHundred() {
        assertEquals(1.0f, normalizeTextScale(null))
        assertEquals(1.0f, normalizeTextScale(Float.NaN))
        assertEquals(1.0f, normalizeTextScale(0.5f))
        assertEquals(1.0f, normalizeTextScale(1.4f))
        assertEquals(0.85f, normalizeTextScale(0.85f))
        assertEquals(1.3f, normalizeTextScale(1.3f))
        assertEquals(1.15f, normalizeTextScale(1.1612f))
    }

    @Test
    fun everyStyleScalesItsSizeAndLineHeight() {
        val scaled = scaledTypography(SessionManagerTypography, 1.3f)
        assertEquals(14.sp * 1.3f, scaled.bodyMedium.fontSize)
        assertEquals(20.sp * 1.3f, scaled.bodyMedium.lineHeight)
        assertEquals(11.sp * 1.3f, scaled.labelSmall.fontSize)
        // A style without a line height keeps none.
        assertEquals(TextUnit.Unspecified, scaled.labelSmall.lineHeight)
        // Styles the app leaves at Material's defaults scale too.
        assertEquals(SessionManagerTypography.titleSmall.fontSize * 1.3f, scaled.titleSmall.fontSize)
        assertEquals(10.sp * 0.85f, scaledTypography(SessionManagerTypography.copy(labelSmall = SessionManagerTypography.labelSmall.copy(fontSize = 10.sp)), 0.85f).labelSmall.fontSize)
        assertSame(SessionManagerTypography, scaledTypography(SessionManagerTypography, 1f))
    }
}
