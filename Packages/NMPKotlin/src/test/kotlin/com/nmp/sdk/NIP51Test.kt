package com.nmp.sdk

import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.runBlocking
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Test
import uniffi.nmp_ffi.FfiRelayListActionFailure
import uniffi.nmp_ffi.FfiRelayListActionStatus

class NIP51Test {
    @Test
    fun relayListActionMapsTypedFailures() {
        assertEquals(
            RelayListActionStatus.Failed(RelayListActionFailure.InvalidRelay("not-a-relay")),
            RelayListActionStatus.from(
                FfiRelayListActionStatus.Failed(
                    FfiRelayListActionFailure.InvalidRelay("not-a-relay"),
                ),
            ),
        )
    }

    @Test
    fun signedOutActionClosesAfterTypedFailure() =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                assertEquals(
                    listOf(
                        RelayListActionStatus.Acquiring,
                        RelayListActionStatus.Failed(RelayListActionFailure.SignedOut),
                    ),
                    engine.addSimpleGroupRelay("wss://relay.example").status.toList(),
                )
            }
        }
}
