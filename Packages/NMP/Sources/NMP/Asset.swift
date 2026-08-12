import Foundation
import NMPFFI

/// Return the canonical lowercase SHA-256 identity of the exact supplied
/// bytes. No claimed digest or network response participates.
public func assetSHA256Hex(of bytes: Data) -> String {
    NMPFFI.assetSha256Hex(bytes: bytes)
}
