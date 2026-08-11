package com.nmp.sdk

import uniffi.nmp_ffi.assetSha256Hex as ffiAssetSha256Hex

/** Return the canonical lowercase SHA-256 identity of the exact supplied
 * bytes. No claimed digest or network response participates. */
fun assetSha256Hex(bytes: ByteArray): String = ffiAssetSha256Hex(bytes)
