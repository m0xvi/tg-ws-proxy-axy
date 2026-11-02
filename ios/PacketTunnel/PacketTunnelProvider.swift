import Foundation
import Network
import NetworkExtension

final class PacketTunnelProvider: NEPacketTunnelProvider {

    private var pathMonitor: NWPathMonitor?
    private let monitorQueue = DispatchQueue(label: "TgWsProxy.TunnelPathMonitor")
    private var lastInterfaceSignature: String?

    // Watchdog внутри самого процесса расширения: iOS держит его в фоне,
    // поэтому именно здесь проверки работают, когда основное приложение выгружено.
    private let healthQueue = DispatchQueue(label: "TgWsProxy.TunnelHealth", qos: .utility)
    private var healthTimer: DispatchSourceTimer?
    private var tunnelConfiguration: ProxyConfiguration?
    private var isStopping = false
    private var isRestarting = false

    override func startTunnel(
        options: [String: NSObject]?,
        completionHandler: @escaping (Error?) -> Void
    ) {
        let configuration = loadConfiguration()
        healthQueue.sync {
            self.tunnelConfiguration = configuration
            self.isStopping = false
            self.isRestarting = false
        }

        let code = NativeProxy.start(configuration: configuration)
        guard code == 0 else {
            completionHandler(
                NSError(
                    domain: "TgWsProxy",
                    code: Int(code),
                    userInfo: [NSLocalizedDescriptionKey: "StartProxy failed (\(code))"]
                )
            )
            return
        }

        let settings = NEPacketTunnelNetworkSettings(tunnelRemoteAddress: "127.0.0.1")
        let ipv4 = NEIPv4Settings(addresses: ["10.7.0.2"], subnetMasks: ["255.255.255.255"])
        ipv4.includedRoutes = []
        ipv4.excludedRoutes = [NEIPv4Route.default()]
        settings.ipv4Settings = ipv4
        settings.mtu = 1500

        setTunnelNetworkSettings(settings) { [weak self] error in
            if error != nil {
                NativeProxy.stop()
            } else {
                self?.startNetworkMonitor()
                self?.startHealthMonitor()
            }
            completionHandler(error)
        }
    }

    override func stopTunnel(
        with reason: NEProviderStopReason,
        completionHandler: @escaping () -> Void
    ) {
        stopNetworkMonitor()
        healthQueue.async { [weak self] in
            guard let self else {
                completionHandler()
                return
            }
            self.isStopping = true
            self.healthTimer?.cancel()
            self.healthTimer = nil
            NativeProxy.stop()
            completionHandler()
        }
    }

    private func startHealthMonitor() {
        healthQueue.async { [weak self] in
            guard let self, !self.isStopping else { return }
            self.healthTimer?.cancel()
            let timer = DispatchSource.makeTimerSource(queue: self.healthQueue)
            timer.schedule(deadline: .now() + 5, repeating: .seconds(10), leeway: .seconds(2))
            timer.setEventHandler { [weak self] in
                self?.checkHealth()
            }
            self.healthTimer = timer
            timer.resume()
            self.checkHealth()
        }
    }

    /// Вызывается только на `healthQueue`.
    private func checkHealth() {
        guard !isStopping, !isRestarting else { return }
        guard !NativeProxy.isRunning() else { return }

        isRestarting = true
        fputs("PacketTunnel: health check failed, restarting native proxy\n", stderr)
        // IsProxyRunning() уже убрал мёртвое состояние в Rust; StopProxy()
        // безопасно возвращает -1, если слушателя больше нет.
        NativeProxy.stop()
        let configuration = tunnelConfiguration ?? ProxyConfiguration()
        let code = NativeProxy.start(configuration: configuration)
        if code == 0 {
            NativeProxy.notifyNetworkChanged()
            fputs("PacketTunnel: native proxy restored\n", stderr)
        } else {
            fputs("PacketTunnel: native proxy restore failed (\(code))\n", stderr)
        }
        isRestarting = false
    }

    private func startNetworkMonitor() {
        stopNetworkMonitor()
        let monitor = NWPathMonitor()
        monitor.pathUpdateHandler = { [weak self] path in
            guard let self, path.status == .satisfied else { return }
            let sig = path.availableInterfaces
                .filter { $0.type != .other }
                .map { "\($0.type):\($0.name)" }
                .joined(separator: ",")
            if let prev = self.lastInterfaceSignature, !prev.isEmpty, prev != sig {
                NativeProxy.notifyNetworkChanged()
                self.healthQueue.async { [weak self] in
                    self?.checkHealth()
                }
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

    override func handleAppMessage(
        _ messageData: Data,
        completionHandler: ((Data?) -> Void)?
    ) {
        guard let command = String(data: messageData, encoding: .utf8) else {
            completionHandler?(nil)
            return
        }
        let response: String
        switch command {
        case "stats":  response = NativeProxy.stats()
        case "secret": response = NativeProxy.secretWithPrefix() ?? ""
        case "health": response = NativeProxy.isRunning() ? "ok" : "dead"
        default:       response = ""
        }
        completionHandler?(response.data(using: .utf8))
    }

    private func loadConfiguration() -> ProxyConfiguration {
        guard
            let proto = protocolConfiguration as? NETunnelProviderProtocol,
            let raw = proto.providerConfiguration?["configuration"] as? Data,
            let decoded = try? JSONDecoder().decode(ProxyConfiguration.self, from: raw)
        else {
            return ProxyConfiguration()
        }
        return decoded
    }
}
