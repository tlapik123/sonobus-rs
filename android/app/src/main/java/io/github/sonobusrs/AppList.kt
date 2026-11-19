package io.github.sonobusrs

import android.content.Context
import android.content.Intent

/** Launchable apps for the "App sound" picker. */
object AppList {
    data class App(val uid: Int, val pkg: String, val label: String)

    fun launchable(ctx: Context): List<App> {
        val pm = ctx.packageManager
        val main = Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER)
        return pm.queryIntentActivities(main, 0).map {
            val ai = it.activityInfo.applicationInfo
            App(ai.uid, ai.packageName, it.loadLabel(pm).toString())
        }
    }

    /** One `uid \t package \t label` line per app: own package dropped, one per package, by label. */
    fun encode(apps: List<App>, own: String): String =
        apps.filter { it.pkg != own }
            .distinctBy { it.pkg }
            .sortedBy { it.label.lowercase() }
            .joinToString("\n") { a ->
                val label = a.label.replace('\t', ' ').replace('\n', ' ').trim().ifEmpty { a.pkg }
                "${a.uid}\t${a.pkg}\t$label"
            }
}
