import Foundation
@testable import NMP

func testPublicKey(_ hex: String) throws -> NMPPublicKey {
    try NMPPublicKey(bytes: decodedHex(hex))
}

func testPrivateKey(_ hex: String) throws -> NMPPrivateKey {
    try NMPPrivateKey(bytes: decodedHex(hex))
}

func testHex(_ publicKey: NMPPublicKey) -> String {
    publicKey.bytes.map { String(format: "%02x", $0) }.joined()
}

private func decodedHex(_ hex: String) -> Data {
    precondition(hex.count.isMultiple(of: 2))
    var bytes = Data()
    bytes.reserveCapacity(hex.count / 2)
    var index = hex.startIndex
    while index < hex.endIndex {
        let next = hex.index(index, offsetBy: 2)
        bytes.append(UInt8(hex[index..<next], radix: 16)!)
        index = next
    }
    return bytes
}
