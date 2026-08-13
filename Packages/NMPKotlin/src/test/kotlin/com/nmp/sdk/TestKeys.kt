package com.nmp.sdk

internal fun String.hexBytes(): ByteArray {
    require(length % 2 == 0)
    return ByteArray(length / 2) { index ->
        substring(index * 2, index * 2 + 2).toInt(16).toByte()
    }
}

internal fun String.testPublicKey(): NMPPublicKey = NMPPublicKey(hexBytes())

internal fun String.testPrivateKey(): NMPPrivateKey = NMPPrivateKey(hexBytes())
