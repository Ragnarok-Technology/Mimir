import AppKit
import Foundation
import MimirCore

@MainActor
final class MimirController: ObservableObject {
    @Published var isRecording = false
    @Published var diarizeMic = false
    @Published var captureSystemAudio = true
    @Published var sessionTitle = ""
    @Published var keywordsText = ""
    @Published var liveLines: [String] = []
    @Published var sessions: [MimirSessionSummary] = []
    @Published var showArchived = false
    @Published var selectedSessionId: String?
    @Published var selectedTranscript = ""
    @Published var selectedTitleEdit = ""
    @Published var statusMessage = "Ready"
    @Published var connectReport = ""
    @Published var updateMessage = ""
    @Published var updateDownloadURL: URL?

    private var recordProcess: Process?
    private var stdinPipe: Pipe?
    private var systemTap: SystemAudioTap?
    private var liveStdoutBuffer = ""

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
                self?.recordProcess = nil
                self?.stdinPipe = nil
                self?.refreshSessions()
            }
        }

        do {
            try proc.run()
            self.recordProcess = proc
            self.isRecording = true
            self.statusMessage = "Recording locally via Whistle AI…"

            if captureSystemAudio, let inPipe = self.stdinPipe {
                let handle = inPipe.fileHandleForWriting
                let tap = SystemAudioTap(
                    onPcmData: { data in
                        try? handle.write(contentsOf: data)
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
        statusMessage = "Finalizing transcript…"
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
