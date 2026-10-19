import Foundation

enum ProxyMode {
    case loopback
    case tunnel

    var title: String {
        switch self {
        case .loopback: "Локальный режим".tgLoc
        case .tunnel: "VPN-туннель".tgLoc
        }
    }
}

protocol ProxyEngine: AnyObject {
    var mode: ProxyMode { get }
    var runsInBackground: Bool { get }

    func start(_ configuration: ProxyConfiguration) async -> Int32
    func stop() async
    func stats() async -> ProxyStats
}

final class LoopbackEngine: ProxyEngine {
    let mode: ProxyMode = .loopback
    let runsInBackground = false

    func start(_ configuration: ProxyConfiguration) async -> Int32 {
        await Task.detached(priority: .userInitiated) {
            NativeProxy.start(configuration: configuration)
        }.value
    }

    func stop() async {
        await Task.detached(priority: .userInitiated) {
            NativeProxy.stop()
        }.value
    }

    func stats() async -> ProxyStats {
        let raw = await Task.detached(priority: .utility) {
            NativeProxy.stats()
        }.value
        return ProxyStats(rawValue: raw)
    }

}

enum ProxyEngineFactory {
    static func make() -> ProxyEngine {
#if TGWS_TUNNEL_AVAILABLE
        return TunnelEngine()
#else
        return LoopbackEngine()
#endif
    }
}
