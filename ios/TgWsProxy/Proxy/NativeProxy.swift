import Foundation

enum NativeProxyError: LocalizedError {
    case startFailed(Int32)

    var errorDescription: String? {
        switch self {
        case .startFailed(let code):
            switch code {
            case -1:
                return "Прокси уже запущен."
            case -3:
                return "Порт занят. Закройте другой прокси или смените порт в настройках."
            case -4:
                return "Нет доступа к кэшу приложения."
            default:
                return "Rust-ядро не запустилось (код \(code))"
            }
        }
    }
}

enum NativeProxy {
    static func start(configuration: ProxyConfiguration) -> Int32 {
        SetPoolSize(Int32(configuration.poolSize))

        guard let cacheURL = FileManager.default.urls(
            for: .cachesDirectory,
            in: .userDomainMask
        ).first else {
            return -4
        }

        cacheURL.path.withCString { SetCfProxyCacheDir($0) }
        configuration.cloudflareDomain.withCString {
            SetCfProxyConfig(configuration.cloudflareEnabled ? 1 : 0, 1, $0)
        }

        let result = configuration.bindAddress.withCString { host in
            configuration.dcAddresses.withCString { addresses in
                configuration.secret.withCString { secret in
                    StartProxy(
                        host,
                        Int32(configuration.port),
                        addresses,
                        secret,
                        configuration.verboseLogging ? 1 : 0
                    )
                }
            }
        }

        return result
    }

    static func stop() {
        _ = StopProxy()
    }

    static func stats() -> String {
        guard let pointer = GetStats() else { return "" }
        defer { FreeString(pointer) }
        return String(cString: pointer)
    }

    static func secretWithPrefix() -> String? {
        guard let pointer = GetSecretWithPrefix() else { return nil }
        defer { FreeString(pointer) }
        return String(cString: pointer)
    }

    static func cloudflareDomains() -> [String] {
        guard let pointer = GetCfProxyDomains() else { return [] }
        defer { FreeString(pointer) }
        return String(cString: pointer)
            .split(separator: "\n")
            .map(String.init)
    }

    static func refreshCloudflareDomains() async -> Bool {
        await Task.detached(priority: .utility) {
            RefreshCfProxyDomains() == 1
        }.value
    }
}
