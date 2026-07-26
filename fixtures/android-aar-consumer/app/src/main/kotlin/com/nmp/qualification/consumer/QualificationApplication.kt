package com.nmp.qualification.consumer

import android.app.Application

/**
 * The qualification app's ordinary dependency-injection boundary.
 *
 * NMP does not require this Application type. Instrumentation constructs one
 * [QualificationEngineOwner] explicitly, installs that exact app-owned value,
 * and removes it by identity after teardown. The Activity may only borrow the
 * installed owner; it can never create a hidden replacement.
 */
class QualificationApplication : Application() {
    private sealed interface OwnerSlot {
        data object Empty : OwnerSlot
        data class Installed(val owner: QualificationEngineOwner) : OwnerSlot
    }

    private val ownerLock = Any()
    private var ownerSlot: OwnerSlot = OwnerSlot.Empty

    fun installQualificationOwner(owner: QualificationEngineOwner) {
        synchronized(ownerLock) {
            check(ownerSlot is OwnerSlot.Empty) {
                "a qualification engine owner is already installed"
            }
            ownerSlot = OwnerSlot.Installed(owner)
        }
    }

    fun qualificationOwner(): QualificationEngineOwner =
        synchronized(ownerLock) {
            when (val slot = ownerSlot) {
                OwnerSlot.Empty -> error("no qualification engine owner is installed")
                is OwnerSlot.Installed -> slot.owner
            }
        }

    fun removeQualificationOwner(owner: QualificationEngineOwner) {
        synchronized(ownerLock) {
            val installed = ownerSlot as? OwnerSlot.Installed
                ?: error("no qualification engine owner is installed")
            check(installed.owner === owner) {
                "only the exact installed engine owner may remove itself"
            }
            ownerSlot = OwnerSlot.Empty
        }
    }
}
