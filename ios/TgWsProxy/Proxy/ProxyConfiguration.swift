import Foundation
import Security

struct ProxyConfiguration: Codable, Equatable, Sendable {
    static let allowedPoolSizes = [2, 4, 6]
    var bindAddress = "127.0.0.1"
    var port = 1443
    var secret = ProxyConfiguration.makeSecret()
    var dcAddresses = ""
    var poolSize = 4
    var cloudflareEnabled = true
    var cloudflareDomain = ""
    var verboseLogging = false

    mutating func normalize() {
        poolSize = Self.allowedPoolSizes.min(by: {
            abs($0 - poolSize) < abs($1 - poolSize)
        }) ?? 4
    }

    static func makeSecret() -> String {
        var bytes = [UInt8](repeating: 0, count: 16)
        let status = bytes.withUnsafeMutableBytes { buffer in
            SecRandomCopyBytes(kSecRandomDefault, buffer.count, buffer.baseAddress!)
        }
        guard status == errSecSuccess else {
            return UUID().uuidString
                .replacingOccurrences(of: "-", with: "")
                .lowercased()
                .prefix(32)
                .description
        }
        return bytes.map { String(format: "%02x", $0) }.joined()
    }
}
