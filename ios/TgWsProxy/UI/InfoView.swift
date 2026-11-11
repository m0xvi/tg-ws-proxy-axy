import SwiftUI

struct InfoView: View {
    private var version: String {
        let v = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "1.0.0"
        let b = Bundle.main.infoDictionary?["CFBundleVersion"] as? String ?? "1"
        return "\(v) (\(b))"
    }

    var body: some View {
        ScrollView {
            VStack(spacing: 16) {
                about
                updateCheck
                links
                Text("GPLv3 (ядро) · MIT (оригинал)".tgLoc)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
            .padding(.horizontal, 16)
            .padding(.bottom, 24)
        }
        .scrollIndicators(.hidden)
    }

    private var about: some View {
        VStack(spacing: 10) {
            Image("AppLogo")
                .resizable()
                .scaledToFit()
            .frame(width: 64, height: 64)
            .clipShape(RoundedRectangle(cornerRadius: 18, style: .continuous))

            Text("KiiXY")
                .font(.title3.bold())
            Text(verbatim: version)
                .font(.caption.monospacedDigit())
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity)
        .card(padding: 20)
    }

    @ObservedObject private var updates = UpdateChecker.shared
    @Environment(\.openURL) private var openURL

    private var updateCheck: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Label("Обновления".tgLoc, systemImage: "arrow.triangle.2.circlepath")
                    .font(.subheadline.weight(.semibold))
                Spacer()
                if let date = updates.lastCheckedAt {
                    Text(date.formatted(date: .omitted, time: .standard))
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }

            Text(updateStatusText)
                .font(.footnote)
                .foregroundStyle(updateStatusColor)

            HStack {
                Button {
                    Task { await updates.check(force: true) }
                } label: {
                    Label("Проверить обновления".tgLoc, systemImage: "arrow.clockwise")
                        .font(.footnote)
                }
                .buttonStyle(.bordered)
                .disabled(updates.status == .checking)

                Spacer(minLength: 8)

                Link(destination: UpdateChecker.releasesPageURL) {
                    Label("Все релизы".tgLoc, systemImage: "arrow.up.right")
                        .font(.footnote)
                }
            }

            if let update = updates.visibleUpdate {
                Button {
                    openURL(update.ipaURL ?? update.htmlURL)
                } label: {
                    Label("Скачать новую версию".tgLoc, systemImage: "arrow.down.circle")
                        .font(.footnote)
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(.borderedProminent)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .card(padding: 14)
    }

    private var updateStatusText: String {
        switch updates.status {
        case .idle: "—".tgLoc
        case .checking: "Проверка…".tgLoc
        case .upToDate: "Установлена последняя версия".tgLoc
        case .available(let release): "Доступна версия".tgLoc + " " + release.version
        case .failed: "Проверка не удалась, попробуйте позже".tgLoc
        }
    }

    private var updateStatusColor: Color {
        switch updates.status {
        case .available: .tgAccent
        case .failed: .orange
        default: .secondary
        }
    }

    private var links: some View {
        VStack(spacing: 0) {
            LinkRow(
                title: "Репозиторий (Android & iOS)",
                subtitle: "m0xvi/tg-ws-proxy-axy",
                systemImage: "iphone",
                url: "https://github.com/m0xvi/tg-ws-proxy-axy"
            )
            Divider()
            LinkRow(
                title: "Оригинальный проект",
                subtitle: "Flowseal/tg-ws-proxy",
                systemImage: "shippingbox",
                url: "https://github.com/Flowseal/tg-ws-proxy"
            )
            Divider()
            LinkRow(
                title: "Upstream Android / iOS",
                subtitle: "amurcanov · IMDelewer",
                systemImage: "cpu",
                url: "https://github.com/amurcanov/tg-ws-proxy-android"
            )
            Divider()
            LinkRow(
                title: "FAQ",
                subtitle: "Flowseal/tg-ws-proxy · issue #389",
                systemImage: "questionmark.circle",
                url: "https://github.com/Flowseal/tg-ws-proxy/issues/389"
            )
        }
        .card(padding: 6)
    }
}

private struct LinkRow: View {
    let title: String
    let subtitle: String
    let systemImage: String
    let url: String

    var body: some View {
        Link(destination: URL(string: url)!) {
            HStack(spacing: 12) {
                Image(systemName: systemImage)
                    .font(.callout)
                    .foregroundStyle(Color.tgAccent)
                    .frame(width: 26)
                VStack(alignment: .leading, spacing: 2) {
                    Text(verbatim: title.tgLoc)
                        .font(.callout)
                        .foregroundStyle(.primary)
                    Text(verbatim: subtitle)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer(minLength: 8)
                Image(systemName: "arrow.up.right")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            .padding(.vertical, 10)
            .padding(.horizontal, 10)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}
