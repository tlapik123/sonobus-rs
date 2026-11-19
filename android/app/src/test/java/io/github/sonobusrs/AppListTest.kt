package io.github.sonobusrs

import org.junit.Assert.assertEquals
import org.junit.Test

class AppListTest {
    @Test
    fun encodeSortsDedupesAndDropsOwnPackage() {
        val apps = listOf(
            AppList.App(10200, "org.example.player", "Player"),
            AppList.App(10201, "io.github.sonobusrs", "SonoBus"),
            AppList.App(10150, "com.example.game", "a\tGame\n"),
            AppList.App(10200, "org.example.player", "Player (second launcher)"),
            AppList.App(10300, "com.example.nolabel", " "),
        )
        val lines = AppList.encode(apps, "io.github.sonobusrs").split("\n")
        assertEquals(
            listOf(
                "10300\tcom.example.nolabel\tcom.example.nolabel",
                "10150\tcom.example.game\ta Game",
                "10200\torg.example.player\tPlayer",
            ),
            lines,
        )
    }
}
