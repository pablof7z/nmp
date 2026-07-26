package com.nmp.qualification.consumer

import android.content.Intent
import android.os.Bundle
import android.widget.TextView
import androidx.activity.ComponentActivity
import androidx.lifecycle.ViewModel
import androidx.lifecycle.ViewModelProvider
import com.nmp.sdk.NMPAccessContext
import com.nmp.sdk.NMPCacheMode
import com.nmp.sdk.NMPDemand
import com.nmp.sdk.NMPFilter
import com.nmp.sdk.NMPFreshness
import com.nmp.sdk.NMPSourceAuthority

/**
 * Deliberately ordinary host Activity for #832's install/launch proof.
 *
 * It owns no engine or coroutine scope; the instrumentation test owns the
 * short qualification engine explicitly. #833 separately proves a durable
 * app-owned lifecycle holder across Android recreation/background events.
 */
class QualificationActivity : ComponentActivity() {
    lateinit var qualificationScreenModel: QualificationScreenModel
        private set

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(
            TextView(this).apply {
                text = "NMP Android runtime qualification"
            },
        )
        if (intent.action == ACTION_LIFECYCLE_QUALIFICATION) {
            val owner = (application as QualificationApplication).qualificationOwner()
            val demand =
                NMPDemand(
                    selection = NMPFilter(kinds = listOf(1u.toUShort())),
                    source =
                        NMPSourceAuthority.Pinned(
                            setOf(BuildConfig.NMP_QUALIFICATION_RELAY),
                        ),
                    access = NMPAccessContext.Public,
                    cache = NMPCacheMode.Strict,
                    freshness = NMPFreshness.Live,
                )
            qualificationScreenModel =
                ViewModelProvider(
                    this,
                    QualificationScreenModelFactory(owner, demand),
                )[QualificationScreenModel::class.java]
        }
    }

    private class QualificationScreenModelFactory(
        private val owner: QualificationEngineOwner,
        private val demand: NMPDemand,
    ) : ViewModelProvider.Factory {
        @Suppress("UNCHECKED_CAST")
        override fun <T : ViewModel> create(modelClass: Class<T>): T {
            check(modelClass == QualificationScreenModel::class.java)
            return QualificationScreenModel(owner, demand) as T
        }
    }

    companion object {
        const val ACTION_LIFECYCLE_QUALIFICATION =
            "com.nmp.qualification.consumer.LIFECYCLE_QUALIFICATION"

        fun lifecycleQualificationIntent(): Intent =
            Intent(ACTION_LIFECYCLE_QUALIFICATION)
                .setClassName(
                    "com.nmp.qualification.consumer",
                    QualificationActivity::class.java.name,
                )
    }
}
