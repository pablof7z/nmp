package com.nmp.qualification.consumer

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import com.nmp.sdk.NMPDemand
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import java.util.concurrent.atomic.AtomicLong

data class QualificationScreenState(
    val controlledEventIds: Set<String> = emptySet(),
)

/**
 * An ordinary screen-level ViewModel selected by the consuming app.
 *
 * Its one collection survives Activity configuration recreation and temporary
 * backgrounding. Permanent screen removal clears the ViewModel, cancelling
 * `viewModelScope` and therefore the exact cold Flow/native handle.
 */
class QualificationScreenModel(
    val owner: QualificationEngineOwner,
    demand: NMPDemand,
) : ViewModel() {
    val instanceId: Long = nextModelId.incrementAndGet()

    private val _state = MutableStateFlow(QualificationScreenState())
    val state: StateFlow<QualificationScreenState> = _state.asStateFlow()

    init {
        viewModelScope.launch(start = CoroutineStart.UNDISPATCHED) {
            owner.observe(demand).collect { batch ->
                val controlled =
                    batch.rows
                        .filter { it.content == CONTROLLED_EVENT_CONTENT }
                        .mapTo(mutableSetOf()) { it.id }
                if (controlled.isNotEmpty()) {
                    _state.value =
                        _state.value.copy(
                            controlledEventIds = _state.value.controlledEventIds + controlled,
                        )
                }
            }
        }
    }

    private companion object {
        val nextModelId = AtomicLong(0)
        const val CONTROLLED_EVENT_CONTENT = "nmp-android-controlled-relay"
    }
}
