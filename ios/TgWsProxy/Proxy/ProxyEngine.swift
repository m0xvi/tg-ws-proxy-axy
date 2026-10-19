import Foundation
import Network

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
    private var pathMonitor: NWPathMonitor?
    private let monitorQueue = DispatchQueue(label: "TgWsProxy.LoopbackPathMonitor")
    private var lastInterfaceSignature: String?

    func start(_ configuration: ProxyConfiguration) async -> Int32 {
        let code = await Task.detached(priority: .userInitiated) {
            NativeProxy.start(configuration: configuration)
        }.value
        if code == 0 {
            startNetworkMonitor()
        }
        return code
    }

    func stop() async {
        stopNetworkMonitor()
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

    private func startNetworkMonitor() {
        stopNetworkMonitor()
        let monitor = NWPathMonitor()
        monitor.pathUpdateHandler = { [weak self] path in
            guard let self, path.status == .satisfied else { return }
            let sig = path.availableInterfaces.map { "\($0.type):\($0.name)" }.joined(separator: ",")
            if let prev = self.lastInterfaceSignature, !prev.isEmpty, prev != sig {
                NativeProxy.notifyNetworkChanged()
            }
            self.lastInterfaceSignature = sig
        }
        pathMonitor = monitor
        monitor.start(queue: monitorQueue)
    }

    private func stopNetworkMonitor() {
        pathMonitor?.cancel()
        pathMonitor = nil
        lastInterfaceSignature = nil
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
