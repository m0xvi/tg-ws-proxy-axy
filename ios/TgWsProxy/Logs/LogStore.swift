import Foundation
import UIKit

@MainActor
final class LogStore: ObservableObject {
    static let shared = LogStore()

    @Published private(set) var lines: [String] = []
    private let maxLines = 800
    private var pipe: Pipe?
    private var originalStderr: Int32?
    private let stamp: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "HH:mm:ss.SSS"
        return f
    }()

    func startCapturing() {
        guard pipe == nil else { return }
        let p = Pipe()
        pipe = p
        setvbuf(stderr, nil, _IONBF, 0)
        originalStderr = dup(STDERR_FILENO)
        dup2(p.fileHandleForWriting.fileDescriptor, STDERR_FILENO)

        p.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty, let chunk = String(data: data, encoding: .utf8) else { return }
            let newLines = chunk
                .split(whereSeparator: \.isNewline)
                .map(String.init)
            guard !newLines.isEmpty else { return }
            Task { @MainActor in self?.append(newLines) }
        }
    }

    private func append(_ new: [String]) {
        let now = stamp.string(from: Date())
        for line in new {
            lines.append("[\(now)] \(line)")
        }
        if lines.count > maxLines {
            lines.removeFirst(lines.count - maxLines)
        }
    }

    func stopCapturing() {
        guard let pipe else { return }
        if let originalStderr {
            dup2(originalStderr, STDERR_FILENO)
            close(originalStderr)
            self.originalStderr = nil
        }
        pipe.fileHandleForReading.readabilityHandler = nil
        self.pipe = nil
    }

    func clear() { lines = [] }

    func joined() -> String { lines.joined(separator: "\n") }

    func report(context: String) -> String {
        let device = UIDevice.current
        let info = Bundle.main.infoDictionary
        let version = info?["CFBundleShortVersionString"] as? String ?? "?"
        let build = info?["CFBundleVersion"] as? String ?? "?"
        let bundleID = Bundle.main.bundleIdentifier ?? "?"
        let iso = ISO8601DateFormatter().string(from: Date())

        var report = "=== TG WS Proxy — отчёт об ошибке ===\n"
        report += "Дата: \(iso)\n"
        report += "Версия: \(version) (\(build))\n"
        report += "Bundle: \(bundleID)\n"
        report += "Устройство: \(device.model), iOS \(device.systemVersion)\n"
        report += "Системный язык: \(Locale.preferredLanguages.first ?? "?")\n"
        report += "\n--- Контекст ---\n\(context)\n"
        report += "\n--- Логи ядра (\(lines.count)) ---\n"
        report += lines.isEmpty ? "(пусто)" : joined()
        return report
    }
}
