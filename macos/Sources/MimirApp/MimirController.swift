import AppKit
import Foundation
import MimirCore

struct MimirSearchHit: Identifiable, Decodable {
    let utterance_id: String
    let session_id: String
    let session_title: String?
    let speaker_id: String
    let speaker_label: String
    let start_ms: UInt64
    let text: String
    var id: String { utterance_id }
}

@MainActor
final class MimirController: ObservableObject {
    @Published var isRecording = false
    @Published var diarizeMic = false
    @Published var saveWav = false
    @Published var captureSystemAudio = true
    @Published var micActivity: Double = 0
    @Published var sysActivity: Double = 0
    @Published var sessionTitle = ""
    @Published var keywordsText = ""
    @Published var liveLines: [String] = []
    @Published var sessions: [MimirSessionSummary] = []
    @Published var showArchived = false
    @Published var selectedSessionId: String?
    @Published var selectedTranscript = ""
    @Published var selectedTitleEdit = ""
    @Published var searchQuery = ""
    @Published var searchResults: [MimirSearchHit] = []
    @Published var speakerIdEdit = ""
    @Published var speakerLabelEdit = ""
    @Published var statusMessage = "Ready"
    @Published var connectReport = ""
    @Published var updateMessage = ""
    @Published var updateDownloadURL: URL?

    private var recordProcess: Process?
    private var stdinPipe: Pipe?
    private var systemTap: SystemAudioTap?
    private var liveStdoutBuffer = ""
    private var localKeyMonitor: Any?
    private var globalKeyMonitor: Any?

    /// Resolves the `mimir` CLI binary: prefers bundled `Contents/MacOS/mimir-cli` inside `.app`,
    /// falling back to repo `target/release/mimir` during development.
    var mimirBinaryURL: URL {
        if let execDir = Bundle.main.executableURL?.deletingLastPathComponent() {
            let bundled = execDir.appendingPathComponent("mimir-cli")
            if FileManager.default.isExecutableFile(atPath: bundled.path) {
                return bundled
            }
        }
        let devRelease = URL(fileURLWithPath: "/Users/joepeterson/development/Mimir/target/release/mimir")
        if FileManager.default.isExecutableFile(atPath: devRelease.path) {
            return devRelease
        }
        return URL(fileURLWithPath: "/usr/local/bin/mimir")
    }

    init() {
        refreshSessions()
        installHotkeys()
    }

    private func installHotkeys() {
        let handleKey: (NSEvent) -> Bool = { [weak self] event in
            let flags = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
            guard flags == [.command, .shift] else { return false }
            switch event.charactersIgnoringModifiers?.lowercased() {
            case "m":
                Task { @MainActor [weak self] in self?.toggleRecording() }
                return true
            case "c":
                Task { @MainActor [weak self] in self?.copyLatestTranscript() }
                return true
            default:
                return false
            }
        }
        localKeyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { ev in
            handleKey(ev) ? nil : ev
        }
        globalKeyMonitor = NSEvent.addGlobalMonitorForEvents(matching: .keyDown) { ev in
            _ = handleKey(ev)
        }
    }

    func toggleRecording() {
        if isRecording {
            stopRecording()
        } else {
            startRecording()
        }
    }

    func startRecording() {
        guard !isRecording else { return }
        liveLines.removeAll()

        let proc = Process()
        proc.executableURL = mimirBinaryURL
        var args = ["start", "--tail", "--serve"]
        if !sessionTitle.trimmingCharacters(in: .whitespaces).isEmpty {
            args += ["--title", sessionTitle.trimmingCharacters(in: .whitespaces)]
        }
        if diarizeMic {
            args.append("--diarize-mic")
        }
        if saveWav {
            args.append("--save-wav")
        }
        for kw in keywordsText.split(separator: ",").map({ $0.trimmingCharacters(in: .whitespaces) }) where !kw.isEmpty {
            args += ["--keyword", kw]
        }
        if captureSystemAudio {
            args.append("--system-pcm-stdin")
        }
        proc.arguments = args

        let outPipe = Pipe()
        let errPipe = Pipe()
        proc.standardOutput = outPipe
        proc.standardError = errPipe

        if captureSystemAudio {
            let inPipe = Pipe()
            proc.standardInput = inPipe
            self.stdinPipe = inPipe
        }

        liveStdoutBuffer = ""
        outPipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty, let text = String(data: data, encoding: .utf8) else { return }
            Task { @MainActor [weak self] in
                self?.appendStdoutChunk(text)
            }
        }

        errPipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty, let text = String(data: data, encoding: .utf8) else { return }
            let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !trimmed.isEmpty else { return }
            Task { @MainActor [weak self] in
                self?.statusMessage = trimmed.components(separatedBy: "\n").last ?? trimmed
            }
        }

        proc.terminationHandler = { [weak self] _ in
            outPipe.fileHandleForReading.readabilityHandler = nil
            errPipe.fileHandleForReading.readabilityHandler = nil
            Task { @MainActor [weak self] in
                self?.isRecording = false
                self?.micActivity = 0
                self?.sysActivity = 0
                self?.recordProcess = nil
                self?.stdinPipe = nil
                self?.refreshSessions()
            }
        }

        do {
            try proc.run()
            self.recordProcess = proc
            self.isRecording = true
            self.micActivity = 0.65
            self.statusMessage = "Recording locally via Whistle AI…"

            if captureSystemAudio, let inPipe = self.stdinPipe {
                let handle = inPipe.fileHandleForWriting
                let tap = SystemAudioTap(
                    onPcmData: { [weak self] data in
                        try? handle.write(contentsOf: data)
                        let lvl = Self.pcm16NormalizedLevel(data)
                        Task { @MainActor [weak self] in
                            self?.sysActivity = lvl
                        }
                    },
                    onError: { [weak self] msg in
                        Task { @MainActor [weak self] in
                            self?.statusMessage = "System audio tap: \(msg)"
                        }
                    }
                )
                self.systemTap = tap
                Task {
                    do {
                        try await tap.start()
                    } catch {
                        await MainActor.run {
                            self.statusMessage = "Mic-only (System audio tap unavailable: \(error.localizedDescription))"
                        }
                    }
                }
            }
        } catch {
            statusMessage = "Failed to start mimir: \(error.localizedDescription)"
        }
    }

    func stopRecording() {
        Task {
            await systemTap?.stop()
            systemTap = nil
            try? stdinPipe?.fileHandleForWriting.close()
            stdinPipe = nil
        }
        if let proc = recordProcess, proc.isRunning {
            kill(proc.processIdentifier, SIGINT)
        }
        micActivity = 0
        sysActivity = 0
        statusMessage = "Finalizing transcript…"
    }

    nonisolated private static func pcm16NormalizedLevel(_ data: Data) -> Double {
        let count = data.count / 2
        guard count > 0 else { return 0 }
        var sumSq: Double = 0
        data.withUnsafeBytes { raw in
            let samples = raw.bindMemory(to: Int16.self)
            for i in 0..<min(count, 512) {
                let f = Double(samples[i]) / Double(Int16.max)
                sumSq += f * f
            }
        }
        let rms = sqrt(sumSq / Double(min(count, 512)))
        return min(max(rms * 8.0, 0), 1)
    }

    func runSearch() {
        let q = searchQuery.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !q.isEmpty else {
            searchResults = []
            return
        }
        Task {
            let out = await runCli(["search", q, "--limit", "20"])
            if let data = out.data(using: .utf8),
               let hits = try? JSONDecoder().decode([MimirSearchHit].self, from: data) {
                self.searchResults = hits
                self.statusMessage = "Found \(hits.count) matching utterance(s)"
            }
        }
    }

    func clearSearch() {
        searchQuery = ""
        searchResults = []
    }

    func renameSpeakerInline() {
        guard let sessionId = selectedSessionId else { return }
        let spk = speakerIdEdit.trimmingCharacters(in: .whitespacesAndNewlines)
        let lbl = speakerLabelEdit.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !spk.isEmpty, !lbl.isEmpty else { return }
        Task {
            _ = await runCli(["rename-speaker", sessionId, spk, lbl])
            self.speakerIdEdit = ""
            self.speakerLabelEdit = ""
            self.statusMessage = "Renamed \(spk) -> \(lbl)"
            self.selectSession(sessionId)
        }
    }

    private func appendStdoutChunk(_ text: String) {
        liveStdoutBuffer += text
        while let nl = liveStdoutBuffer.firstIndex(of: "\n") {
            let line = String(liveStdoutBuffer[..<nl]).trimmingCharacters(in: .newlines)
            if !line.isEmpty {
                liveLines.append(line)
            }
            liveStdoutBuffer.removeSubrange(...nl)
        }
    }

    func refreshSessions() {
        let includeArchived = showArchived
        Task {
            var args = ["sessions", "--limit", "50"]
            if includeArchived {
                args.append("--include-archived")
            }
            let output = await runCli(args)
            let parsed = MimirSessionSummary.parseCliLines(output)
            self.sessions = parsed
            if let currentId = selectedSessionId,
               let match = parsed.first(where: { $0.id == currentId }) {
                self.selectedTitleEdit = match.title == "Untitled" ? "" : match.title
            } else if let first = parsed.first {
                selectSession(first.id)
            } else {
                selectedSessionId = nil
                selectedTranscript = ""
                selectedTitleEdit = ""
            }
        }
    }

    func selectSession(_ id: String) {
        selectedSessionId = id
        if let match = sessions.first(where: { $0.id == id }) {
            selectedTitleEdit = match.title == "Untitled" ? "" : match.title
        }
        Task {
            let md = await runCli(["export", id, "--format", "md"])
            self.selectedTranscript = md
        }
    }

    func renameSelectedSession() {
        guard let id = selectedSessionId else { return }
        let title = selectedTitleEdit.trimmingCharacters(in: .whitespacesAndNewlines)
        Task {
            _ = await runCli(["rename-session", id, title])
            self.statusMessage = title.isEmpty ? "Cleared session title" : "Renamed session to \"\(title)\""
            refreshSessions()
            selectSession(id)
        }
    }

    func toggleArchiveSelectedSession() {
        guard let id = selectedSessionId,
              let current = sessions.first(where: { $0.id == id }) else { return }
        let unarchive = current.archived
        Task {
            var args = ["archive", id]
            if unarchive {
                args.append("--unarchive")
            }
            _ = await runCli(args)
            self.statusMessage = unarchive ? "Unarchived session" : "Archived session"
            if !unarchive && !showArchived {
                selectedSessionId = nil
                selectedTranscript = ""
            }
            refreshSessions()
        }
    }

    func deleteSelectedSession() {
        guard let id = selectedSessionId else { return }
        Task {
            _ = await runCli(["delete", id])
            self.selectedSessionId = nil
            self.selectedTranscript = ""
            self.selectedTitleEdit = ""
            self.statusMessage = "Deleted session permanently"
            refreshSessions()
        }
    }

    func copyLatestTranscript() {
        Task {
            let md = await runCli(["latest", "--format", "md"])
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(md, forType: .string)
            statusMessage = "Copied latest transcript to clipboard"
        }
    }

    func copySelectedTranscript() {
        guard !selectedTranscript.isEmpty else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(selectedTranscript, forType: .string)
        statusMessage = "Copied transcript to clipboard"
    }

    func connectAssistants() {
        Task {
            let output = await runCli(["connect"])
            self.connectReport = output.isEmpty ? "No assistant configs detected." : output
            self.statusMessage = "Updated assistant MCP registrations"
        }
    }

    func fetchModel() {
        statusMessage = "Fetching whistle.cact (16.9 MB)…"
        Task {
            let output = await runCli(["fetch"])
            self.statusMessage = output.trimmingCharacters(in: .whitespacesAndNewlines)
        }
    }

    func checkForUpdates() {
        updateMessage = "Checking GitHub Releases…"
        updateDownloadURL = nil
        Task {
            let result = await UpdateService.shared.checkForUpdates()
            switch result {
            case .updateAvailable(let ver, _, let url, _):
                updateMessage = "Update \(ver) available"
                updateDownloadURL = url
            case .upToDate(let cur):
                updateMessage = "Up to date (v\(cur))"
            case .error(let msg):
                updateMessage = msg
            }
        }
    }

    private func runCli(_ args: [String]) async -> String {
        let bin = mimirBinaryURL
        return await withCheckedContinuation { cont in
            DispatchQueue.global(qos: .userInitiated).async {
                let proc = Process()
                proc.executableURL = bin
                proc.arguments = args
                let out = Pipe()
                proc.standardOutput = out
                proc.standardError = Pipe()
                do {
                    try proc.run()
                    proc.waitUntilExit()
                    let data = out.fileHandleForReading.readDataToEndOfFile()
                    cont.resume(returning: String(data: data, encoding: .utf8) ?? "")
                } catch {
                    cont.resume(returning: "")
                }
            }
        }
    }
}
