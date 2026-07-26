package com.nmp.qualification.consumer

import android.app.Activity
import android.os.Bundle
import android.widget.TextView

/**
 * Deliberately ordinary host Activity for #832's install/launch proof.
 *
 * It owns no engine or coroutine scope; the instrumentation test owns the
 * short qualification engine explicitly. #833 separately proves a durable
 * app-owned lifecycle holder across Android recreation/background events.
 */
class QualificationActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(
            TextView(this).apply {
                text = "NMP Android runtime qualification"
            },
        )
    }
}
