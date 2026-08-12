package com.nmp.qualification.consumer

import com.nmp.sdk.NMPRelayScope

/** Must not compile against a core-only or normal-client AAR. */
fun unselectedNip29Surface(): NMPRelayScope = NMPRelayScope.on(setOf("wss://example.test"))
