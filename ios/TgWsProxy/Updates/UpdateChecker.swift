import Foundation
import SwiftUI

/// Встроенная проверка обновлений.
///
/// Приложение опрашивает последний GitHub Release репозитория
/// `m0xvi/tg-ws-proxy-axy` и, если там новее текущей версии, показывает
/// карточку на главном экране с прямой ссылкой на подходящий `.ipa`
/// (free/vpn — в зависимости от сборки).
///
/// Сэндбокс не позволяет приложению заменить самого себя, поэтому
/// установка нового `.ipa` выполняется вашим sideload-инструментом
/// (Sideloadly / AltStore / TrollStore). Именно так обновления идут
/// без кабеля: ссылка → скачали файл → установили по Wi-Fi.
@MainActor
final class UpdateChecker: ObservableObject {
    static let shared = UpdateChecker()

    struct Release: Codable, Equatable {
        let tag: String       // "v2.0.14"
        let version: String   // "2.0.14"
        let htmlURL: URL      // страница релиза
        let ipaURL: URL?      // прямая ссылка на .ipa для этой сборки
    }

    enum Status: Equatable {
        case idle
        case checking
        case upToDate
        case available(Release)
        case failed
    }

    @Published private(set) var status: Status = .idle
    @Published private(set) var lastCheckedAt: Date?

    static let repository = "m0xvi/tg-ws-proxy-axy"
    static let releasesPageURL = URL(string: "https://github.com/\(repository)/releases")!
    private static let latestReleaseURL = URL(string: "https://api.github.com/repos/\(repository)/releases/latest")!

    private enum Keys {
        static let etag = "update.check.etag"
        static let snapshot = "update.check.snapshot"
        static let snoozed = "update.check.snoozed"
    }

    private var etag: String? {
        get { UserDefaults.standard.string(forKey: Keys.etag) }
        set { UserDefaults.standard.set(newValue, forKey: Keys.etag) }
    }

    private var snoozedVersion: String? {
        get { UserDefaults.standard.string(forKey: Keys.snoozed) }
        set { UserDefaults.standard.set(newValue, forKey: Keys.snoozed) }
    }

    var currentVersion: String {
        (Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String) ?? "0"
    }

    /// Обновление, которое надо показывать: новая версия есть,
    /// а пользователь ещё не нажал «Позже» на неё.
    var visibleUpdate: Release? {
        guard case .available(let release) = status else { return nil }
        return release.version == snoozedVersion ? nil : release
    }

    /// `force` — ручная проверка (кнопка в «Информации»).
    /// Автопроверки не чаще раза в час.
    func check(force: Bool = false) async {
        if case .checking = status { return }
        if !force, let last = lastCheckedAt, Date().timeIntervalSince(last) < 60 * 60 { return }

        status = .checking
        var request = URLRequest(url: Self.latestReleaseURL, timeoutInterval: 12)
        request.setValue("KiiXY-iOS/UpdateChecker", forHTTPHeaderField: "User-Agent")
        request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
        request.setValue("2022-11-28", forHTTPHeaderField: "X-GitHub-Api-Version")
        if let etag, !etag.isEmpty {
            request.setValue(etag, forHTTPHeaderField: "If-None-Match")
        }

        do {
            let (data, response) = try await URLSession.shared.data(for: request)
            guard let http = response as? HTTPURLResponse else {
                fail()
                return
            }
            switch http.statusCode {
            case 304:
                // Релиз не изменился: восстанавливаем состояние из снапшота.
                if let release = readSnapshot() {
                    transition(to: release)
                } else {
                    status = .upToDate
                }
            case 200:
                if let newETag = http.allHeaderFields["ETag"] as? String {
                    etag = newETag
                }
                if let release = Self.parse(data) {
                    saveSnapshot(release)
                    transition(to: release)
                } else {
                    fail()
                }
            default:
                fail()
            }
        } catch {
            fail()
        }
    }

    /// «Позже»: карточка скроется, пока не выйдет версия новее.
    func snoozeVisibleUpdate() {
        if case .available(let release) = status {
            snoozedVersion = release.version
        }
    }

    // MARK: - Private

    private func fail() {
        lastCheckedAt = Date()
        status = .failed
    }

    private func transition(to release: Release) {
        lastCheckedAt = Date()
        if Self.isNewer(release.version, than: currentVersion) {
            status = .available(release)
        } else {
            status = .upToDate
        }
    }

    private func saveSnapshot(_ release: Release) {
        if let data = try? JSONEncoder().encode(release) {
            UserDefaults.standard.set(data, forKey: Keys.snapshot)
        }
    }

    private func readSnapshot() -> Release? {
        guard let data = UserDefaults.standard.data(forKey: Keys.snapshot) else { return nil }
        return try? JSONDecoder().decode(Release.self, from: data)
    }

    /// Разбор ответа `/releases/latest`. Ссылка ведёт на `.ipa`,
    /// соответствующий этой сборке (vpn-сборка — vpn-файл).
    private static func parse(_ data: Data) -> Release? {
        struct Asset: Decodable {
            let name: String
            let browser_download_url: URL
        }
        struct Payload: Decodable {
            let tag_name: String
            let html_url: URL
            let assets: [Asset]?
        }

        guard let payload = try? JSONDecoder().decode(Payload.self, from: data) else { return nil }
        var tag = payload.tag_name
        if tag.hasPrefix("v") { tag.removeFirst() }
        guard !tag.isEmpty else { return nil }

        let wanted: String
        #if TGWS_TUNNEL_AVAILABLE
        wanted = "TgWsProxy-vpn.ipa"
        #else
        wanted = "TgWsProxy-free.ipa"
        #endif
        let assets = payload.assets ?? []
        let ipa = assets.first { $0.name == wanted }?.browser_download_url
            ?? assets.last { $0.name.hasSuffix(".ipa") }?.browser_download_url

        return Release(
            tag: payload.tag_name,
            version: tag,
            htmlURL: payload.html_url,
            ipaURL: ipa
        )
    }

    /// Численное сравнение версий по компонентам: "2.0.9" < "2.0.14".
    static func isNewer(_ candidate: String, than current: String) -> Bool {
        let components: (String) -> [Int] = { version in
            version
                .split(separator: ".")
                .prefix(4)
                .map { Int($0.split(separator: "-").first.map(String.init) ?? "") ?? 0 }
        }
        let a = components(candidate)
        let b = components(current)
        for i in 0..<max(a.count, b.count) {
            let x = i < a.count ? a[i] : 0
            let y = i < b.count ? b[i] : 0
            if x != y { return x > y }
        }
        return false
    }
}
