package com.nmp.qualification.consumer

import android.app.Activity
import android.os.Bundle
import android.util.Log
import android.widget.TextView

/** Ordinary Activity proof; runtime ownership stays in the test worker scope. */
class QualificationActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(TextView(this).apply { text = "NMP Android qualification" })
        Log.i("NMPQualification", "NMP_ANDROID_ACTIVITY_LAUNCHED true")
    }
}
