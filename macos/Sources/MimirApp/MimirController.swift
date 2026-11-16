import AppKit
import CoreAudio
import EventKit
import Foundation
import MimirCore

struct MimirSearchHit: Identifiable, Decodable, Sendable {
    let utterance_id: String
    let session_id: String
    let session_title: String?
    let speaker_id: String
    let speaker_label: String
    let start_ms: UInt64
    let text: String
    var id: String { utterance_id }
}

struct MimirModelStatus: Identifiable, Decodable, Sendable {
    let id: String
    let name: String
    let installed: Bool
    let size_bytes: UInt64
    let expected_size_label: String
    let path: String
}

struct MimirKeywordPreset: Identifiable, Codable, Hashable, Sendable {
    let name: String
    let keywords: [String]
    var id: String { name }
}

struct MimirActionItem: Identifiable, Decodable, Sendable {
    let owner: String?
    let task: String
    let due_hint: String?
    let timestamp: String?
    var id: String { "\(owner ?? "")|\(task)|\(timestamp ?? "")" }
}

struct MimirSessionJson: Decodable, Sendable {
    let summary_markdown: String?
    let summary_template: String?
    let action_items_json: String?
}

struct MimirCalendarEvent: Identifiable, Sendable {
    let uid: String
    let title: String
    let startDate: Date
    let endDate: Date
    let meetingUrl: URL?
    let attendees: [String]
    let notes: String?
    var id: String { uid }
}

@MainActor
final class MimirController: ObservableObject {
    @Published var isRecording = false
    @Published var diarizeMic = false
    @Published var saveWav = false
    @Published var enableCleanup = false
    @Published var autoDetectCalls = false {
        didSet { updateAutoDetectMonitor() }
    }
    @Published var captureSystemAudio = true
    @Published var micActivity: Double = 0
    @Published var sysActivity: Double = 0
    @Published var sessionTitle = ""
    @Published var keywordsText = ""
    @Published var keywordPresets: [MimirKeywordPreset] = []
    @Published var selectedPresetName = ""
    @Published var presetNameInput = ""
    @Published var autoSummarize = true
    @Published var dropSyncEnabled = false
    @Published var liveLines: [String] = []
    @Published var sessions: [MimirSessionSummary] = []
    @Published var showArchived = false
    @Published var selectedSessionId: String?
    @Published var selectedTranscript = ""
    @Published var selectedTitleEdit = ""
    @Published var selectedSummaryMarkdown = ""
    @Published var selectedSummaryTemplate = "general"
    @Published var selectedActionItems: [MimirActionItem] = []
    @Published var isSummarizingSession = false
    @Published var summarizeDone = 0
    @Published var summarizeTotal = 0
    @Published var upcomingEvents: [MimirCalendarEvent] = []
    @Published var calendarAccessStatus = "Checking calendar…"
    @Published var calendarNeedsSettingsButton = false
    @Published var calendarIcsUrls: [String] = []
    @Published var calendarIcsInput = ""
    @Published var searchQuery = ""
    @Published var searchResults: [MimirSearchHit] = []
    @Published var speakerIdEdit = ""
    @Published var speakerLabelEdit = ""
    @Published var statusMessage = "Ready"
    @Published var connectReport = ""
    @Published var updateMessage = ""
    @Published var updateDownloadURL: URL?
    @Published var modelStatuses: [MimirModelStatus] = []
    @Published var activeModelTaskId: String?
    @Published var isCleaningSession = false
    @Published var cleanupDone = 0
    @Published var cleanupTotal = 0
    @Published var transcriptViewMode = "cleaned" {
        didSet {
            if transcriptViewMode != "summary", let sid = selectedSessionId {
                selectSession(sid)
            }
        }
    }

    private var recordProcess: Process?
    private var stdinPipe: Pipe?
    private var systemTap: SystemAudioTap?
    private var liveStdoutBuffer = ""
    private var localKeyMonitor: Any?
    private var globalKeyMonitor: Any?
    private var terminationObserver: NSObjectProtocol?
    private var autoDetectTask: Task<Void, Never>?
    private var calendarPollTask: Task<Void, Never>?
    private var wasAudioActiveBeforeStop = false
    private let eventStore = EKEventStore()
    private var pendingCalendarEvent: MimirCalendarEvent?

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
        loadConfigFlags()
        refreshSessions()
        refreshPresets()
        refreshCalendarEvents()
        startCalendarPolling()
        installHotkeys()
        terminationObserver = NotificationCenter.default.addObserver(
            forName: NSApplication.willTerminateNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            Task { @MainActor [weak self] in
                self?.shutdownSync()
            }
        }
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
        if enableCleanup {
            args.append("--cleanup")
        }
        for kw in keywordsText.split(separator: ",").map({ $0.trimmingCharacters(in: .whitespaces) }) where !kw.isEmpty {
            args += ["--keyword", kw]
        }
        if let calEv = pendingCalendarEvent {
            args += ["--calendar-uid", calEv.uid]
            if let url = calEv.meetingUrl {
                args += ["--meeting-url", url.absoluteString]
            }
            for att in calEv.attendees where !att.trimmingCharacters(in: .whitespaces).isEmpty {
                args += ["--attendee", att]
            }
            pendingCalendarEvent = nil
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
        try? stdinPipe?.fileHandleForWriting.close()
        stdinPipe = nil
        let tap = systemTap
        systemTap = nil
        Task {
            await tap?.stop()
        }
        if let proc = recordProcess, proc.isRunning {
            let pid = proc.processIdentifier
            kill(pid, SIGINT)
            DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + 3.0) {
                if proc.isRunning {
                    kill(pid, SIGTERM)
                }
            }
        }
        micActivity = 0
        sysActivity = 0
        wasAudioActiveBeforeStop = true
        statusMessage = "Finalizing transcript…"
    }

    /// Synchronously stops capture and waits briefly for `mimir-cli` to release
    /// the microphone and flush SQLite before the app terminates.
    func shutdownSync() {
        autoDetectTask?.cancel()
        autoDetectTask = nil
        calendarPollTask?.cancel()
        calendarPollTask = nil
        try? stdinPipe?.fileHandleForWriting.close()
        stdinPipe = nil
        let tap = systemTap
        systemTap = nil
        if let tap {
            Task {
                await tap.stop()
            }
        }
        if let proc = recordProcess, proc.isRunning {
            let pid = proc.processIdentifier
            kill(pid, SIGINT)
            let deadline = Date().addingTimeInterval(1.5)
            while proc.isRunning && Date() < deadline {
                Thread.sleep(forTimeInterval: 0.05)
            }
            if proc.isRunning {
                proc.terminate()
                let termDeadline = Date().addingTimeInterval(0.5)
                while proc.isRunning && Date() < termDeadline {
                    Thread.sleep(forTimeInterval: 0.05)
                }
            }
            if proc.isRunning {
                kill(pid, SIGKILL)
            }
        }
        recordProcess = nil
        isRecording = false
        micActivity = 0
        sysActivity = 0
    }

    private func updateAutoDetectMonitor() {
        autoDetectTask?.cancel()
        autoDetectTask = nil
        calendarPollTask?.cancel()
        calendarPollTask = nil
        guard autoDetectCalls else { return }
        autoDetectTask = Task { @MainActor [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 3_000_000_000)
                guard let self, self.autoDetectCalls else { break }
                let active = Self.isSystemOutputAudioRunning()
                if !active {
                    self.wasAudioActiveBeforeStop = false
                } else if !self.isRecording && !self.wasAudioActiveBeforeStop {
                    self.statusMessage = "Call/audio detected — starting local capture…"
                    self.startRecording()
                }
            }
        }
    }

    nonisolated private static func isSystemOutputAudioRunning() -> Bool {
        var devId = AudioDeviceID(0)
        var size = UInt32(MemoryLayout<AudioDeviceID>.size)
        var addr = AudioObjectPropertyAddress(
            mSelector: kAudioHardwarePropertyDefaultOutputDevice,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain
        )
        guard AudioObjectGetPropertyData(
            AudioObjectID(kAudioObjectSystemObject),
            &addr,
            0,
            nil,
            &size,
            &devId
        ) == noErr, devId != kAudioObjectUnknown else {
            return false
        }
        var running: UInt32 = 0
        var runSize = UInt32(MemoryLayout<UInt32>.size)
        var runAddr = AudioObjectPropertyAddress(
            mSelector: kAudioDevicePropertyDeviceIsRunningSomewhere,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain
        )
        guard AudioObjectGetPropertyData(devId, &runAddr, 0, nil, &runSize, &running) == noErr else {
            return false
        }
        return running != 0
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
        let viewMode = transcriptViewMode == "summary" ? "cleaned" : transcriptViewMode
        Task {
            let md = await runCli(["export", id, "--format", "md", "--view", viewMode])
            self.selectedTranscript = md
            let jsonOut = await runCli(["export", id, "--format", "json"])
            if let data = jsonOut.data(using: .utf8),
               let detail = try? JSONDecoder().decode(MimirSessionJson.self, from: data)
            {
                self.selectedSummaryMarkdown = detail.summary_markdown ?? ""
                if let tmpl = detail.summary_template, !tmpl.isEmpty {
                    self.selectedSummaryTemplate = tmpl
                }
                if let rawItems = detail.action_items_json,
                   let itemsData = rawItems.data(using: .utf8),
                   let decodedItems = try? JSONDecoder().decode([MimirActionItem].self, from: itemsData)
                {
                    self.selectedActionItems = decodedItems
                } else {
                    self.selectedActionItems = []
                }
            }
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

    func refreshModelStatuses() {
        Task {
            let output = await runCli(["models", "status", "--json"])
            if let data = output.data(using: .utf8),
               let decoded = try? JSONDecoder().decode([MimirModelStatus].self, from: data)
            {
                self.modelStatuses = decoded
            }
        }
    }

    func fetchModel(_ id: String = "whistle") {
        activeModelTaskId = id
        let flag: [String]
        switch id {
        case "qwen":
            statusMessage = "Downloading Qwen 2.5 0.5B Instruct (475 MB)…"
            flag = ["fetch", "--qwen"]
        case "silero_vad", "speaker_embed", "onnx":
            statusMessage = "Downloading ONNX VAD & Speaker Embedding models…"
            flag = ["fetch", "--onnx"]
        case "all":
            statusMessage = "Downloading all local Mimir models…"
            flag = ["fetch", "--all"]
        default:
            statusMessage = "Fetching whistle.cact (16.9 MB)…"
            flag = ["fetch"]
        }
        Task {
            let output = await runCli(flag)
            self.activeModelTaskId = nil
            let trimmed = output.trimmingCharacters(in: .whitespacesAndNewlines)
            if !trimmed.isEmpty {
                self.statusMessage = trimmed.components(separatedBy: "\n").last ?? trimmed
            }
            self.refreshModelStatuses()
        }
    }

    func deleteModel(_ id: String) {
        activeModelTaskId = id
        statusMessage = "Removing \(id)…"
        Task {
            let output = await runCli(["models", "delete", id])
            self.activeModelTaskId = nil
            let trimmed = output.trimmingCharacters(in: .whitespacesAndNewlines)
            if !trimmed.isEmpty {
                self.statusMessage = trimmed
            }
            self.refreshModelStatuses()
        }
    }

    func revertSelectedSession() {
        guard let sid = selectedSessionId else { return }
        statusMessage = "Reverting transcript to raw Whistle text…"
        Task {
            let output = await runCli(["revert", sid])
            let trimmed = output.trimmingCharacters(in: .whitespacesAndNewlines)
            if !trimmed.isEmpty {
                self.statusMessage = trimmed
            }
            self.selectSession(sid)
        }
    }

    func cleanupSelectedSession() {
        guard let sid = selectedSessionId else { return }
        isCleaningSession = true
        cleanupDone = 0
        cleanupTotal = 0
        statusMessage = "Running local Qwen transcript cleanup…"
        let bin = mimirBinaryURL
        Task {
            let output: String = await withCheckedContinuation { cont in
                DispatchQueue.global(qos: .userInitiated).async {
                    let proc = Process()
                    proc.executableURL = bin
                    proc.arguments = ["cleanup", sid, "--progress"]
                    let outPipe = Pipe()
                    let errPipe = Pipe()
                    proc.standardOutput = outPipe
                    proc.standardError = errPipe

                    errPipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
                        let data = handle.availableData
                        guard !data.isEmpty, let chunk = String(data: data, encoding: .utf8) else { return }
                        for line in chunk.components(separatedBy: .newlines) where line.hasPrefix("PROGRESS:") {
                            let parts = line.split(separator: ":")
                            if parts.count == 3, let d = Int(parts[1]), let t = Int(parts[2]) {
                                Task { @MainActor [weak self] in
                                    self?.cleanupDone = d
                                    self?.cleanupTotal = t
                                }
                            }
                        }
                    }

                    try? proc.run()
                    proc.waitUntilExit()
                    errPipe.fileHandleForReading.readabilityHandler = nil
                    let outData = outPipe.fileHandleForReading.readDataToEndOfFile()
                    cont.resume(returning: String(data: outData, encoding: .utf8) ?? "")
                }
            }
            self.isCleaningSession = false
            let trimmed = output.trimmingCharacters(in: .whitespacesAndNewlines)
            if !trimmed.isEmpty {
                self.statusMessage = trimmed
            }
            self.selectSession(sid)
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

    // MARK: - Summarization, Presets & Config

    func summarizeSelectedSession() {
        guard let sid = selectedSessionId else { return }
        isSummarizingSession = true
        summarizeDone = 0
        summarizeTotal = 0
        let tmpl = selectedSummaryTemplate
        statusMessage = "Summarizing session with Qwen 2.5 0.5B…"
        let bin = mimirBinaryURL
        Task {
            let output: String = await withCheckedContinuation { cont in
                DispatchQueue.global(qos: .userInitiated).async {
                    let proc = Process()
                    proc.executableURL = bin
                    proc.arguments = ["summarize", sid, "--template", tmpl, "--progress"]
                    let outPipe = Pipe()
                    let errPipe = Pipe()
                    proc.standardOutput = outPipe
                    proc.standardError = errPipe

                    errPipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
                        let data = handle.availableData
                        guard !data.isEmpty, let chunk = String(data: data, encoding: .utf8) else { return }
                        for line in chunk.components(separatedBy: .newlines) where line.hasPrefix("PROGRESS:") {
                            let parts = line.split(separator: ":")
                            if parts.count == 3, let d = Int(parts[1]), let t = Int(parts[2]) {
                                Task { @MainActor [weak self] in
                                    self?.summarizeDone = d
                                    self?.summarizeTotal = t
                                }
                            }
                        }
                    }

                    try? proc.run()
                    proc.waitUntilExit()
                    errPipe.fileHandleForReading.readabilityHandler = nil
                    let outData = outPipe.fileHandleForReading.readDataToEndOfFile()
                    cont.resume(returning: String(data: outData, encoding: .utf8) ?? "")
                }
            }
            self.isSummarizingSession = false
            self.transcriptViewMode = "summary"
            let trimmed = output.trimmingCharacters(in: .whitespacesAndNewlines)
            if !trimmed.isEmpty {
                self.statusMessage = "Summarized session (\(tmpl))"
            }
            self.refreshSessions()
            self.selectSession(sid)
        }
    }

    func refreshPresets() {
        Task {
            let output = await runCli(["presets", "list", "--json"])
            if let data = output.data(using: .utf8),
               let decoded = try? JSONDecoder().decode([MimirKeywordPreset].self, from: data)
            {
                self.keywordPresets = decoded
            }
        }
    }

    func applyPreset(_ name: String) {
        selectedPresetName = name
        if let preset = keywordPresets.first(where: { $0.name == name }) {
            presetNameInput = preset.name
            keywordsText = preset.keywords.joined(separator: ", ")
        }
    }

    func saveCurrentPreset() {
        let name = presetNameInput.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !name.isEmpty else { return }
        let kws = keywordsText
            .split(separator: ",")
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }
            .joined(separator: ",")
        Task {
            _ = await runCli(["presets", "save", name, "--keywords", kws])
            self.selectedPresetName = name
            self.statusMessage = "Saved keyword preset '\(name)'"
            self.refreshPresets()
        }
    }

    func deleteSelectedPreset() {
        let name = selectedPresetName.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !name.isEmpty else { return }
        Task {
            _ = await runCli(["presets", "delete", name])
            self.selectedPresetName = ""
            self.statusMessage = "Deleted keyword preset '\(name)'"
            self.refreshPresets()
        }
    }

    private var configFileURL: URL? {
        FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)
            .first?
            .appendingPathComponent("mimir/config.json")
    }

    func loadConfigFlags() {
        guard let url = configFileURL,
              let data = try? Data(contentsOf: url),
              let dict = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return }
        if let autoSum = dict["auto_summarize"] as? Bool {
            self.autoSummarize = autoSum
        }
        if let dropSync = dict["drop_sync_enabled"] as? Bool {
            self.dropSyncEnabled = dropSync
        }
        if let tmpl = dict["default_summary_template"] as? String, !tmpl.isEmpty {
            self.selectedSummaryTemplate = tmpl
        }
        if let ics = dict["calendar_ics_urls"] as? [String] {
            self.calendarIcsUrls = ics
        }
    }

    func saveConfigFlags() {
        guard let url = configFileURL else { return }
        let dir = url.deletingLastPathComponent()
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        var dict: [String: Any] = [:]
        if let data = try? Data(contentsOf: url),
           let existing = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        {
            dict = existing
        }
        dict["auto_summarize"] = autoSummarize
        dict["drop_sync_enabled"] = dropSyncEnabled
        dict["default_summary_template"] = selectedSummaryTemplate
        dict["calendar_ics_urls"] = calendarIcsUrls
        if let encoded = try? JSONSerialization.data(withJSONObject: dict, options: [.prettyPrinted, .sortedKeys]) {
            try? encoded.write(to: url)
        }
    }

    // MARK: - Calendar Integration (Apple Calendar EventKit + .ics Feeds) & Join & Record

    func addCalendarIcsSource() {
        let trimmed = calendarIcsInput.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        if !calendarIcsUrls.contains(trimmed) {
            calendarIcsUrls.append(trimmed)
            saveConfigFlags()
        }
        calendarIcsInput = ""
        refreshCalendarEvents()
    }

    func removeCalendarIcsSource(at index: Int) {
        guard calendarIcsUrls.indices.contains(index) else { return }
        calendarIcsUrls.remove(at: index)
        saveConfigFlags()
        refreshCalendarEvents()
    }

    func openCalendarPrivacySettings() {
        if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Calendars") {
            NSWorkspace.shared.open(url)
        }
    }

    private func startCalendarPolling() {
        calendarPollTask?.cancel()
        calendarPollTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(60))
                guard !Task.isCancelled else { break }
                self?.refreshCalendarEvents()
            }
        }
    }

    func refreshCalendarEvents() {
        let status = EKEventStore.authorizationStatus(for: .event)
        switch status {
        case .authorized, .fullAccess:
            calendarAccessStatus = "Apple Calendar connected"
            calendarNeedsSettingsButton = false
            reloadMergedCalendarEvents(includeEventKit: true)
        case .notDetermined:
            calendarAccessStatus = "Requesting Apple Calendar access…"
            calendarNeedsSettingsButton = false
            reloadMergedCalendarEvents(includeEventKit: false)
            eventStore.requestFullAccessToEvents { [weak self] granted, _ in
                Task { @MainActor [weak self] in
                    guard let self else { return }
                    if granted {
                        self.calendarAccessStatus = "Apple Calendar connected"
                        self.calendarNeedsSettingsButton = false
                        self.reloadMergedCalendarEvents(includeEventKit: true)
                    } else {
                        self.calendarAccessStatus = "Apple Calendar access denied (System Settings → Privacy & Security → Calendars)"
                        self.calendarNeedsSettingsButton = true
                        self.reloadMergedCalendarEvents(includeEventKit: false)
                    }
                }
            }
        case .denied, .restricted, .writeOnly:
            calendarAccessStatus = "Apple Calendar access denied (System Settings → Privacy & Security → Calendars)"
            calendarNeedsSettingsButton = true
            reloadMergedCalendarEvents(includeEventKit: false)
        @unknown default:
            calendarAccessStatus = "Apple Calendar unavailable"
            calendarNeedsSettingsButton = false
            reloadMergedCalendarEvents(includeEventKit: false)
        }
    }

    private func reloadMergedCalendarEvents(includeEventKit: Bool) {
        let ekEvents = includeEventKit ? fetchUpcomingEventKitEvents() : []
        let icsSources = calendarIcsUrls
        if icsSources.isEmpty {
            self.upcomingEvents = ekEvents.sorted { $0.startDate < $1.startDate }
            return
        }
        Task {
            let icsEvents = await Task.detached(priority: .utility) {
                Self.fetchUpcomingIcsEvents(from: icsSources)
            }.value
            var merged = ekEvents
            for ev in icsEvents {
                let duplicate = merged.contains { existing in
                    existing.uid == ev.uid
                        || (existing.title.caseInsensitiveCompare(ev.title) == .orderedSame
                            && abs(existing.startDate.timeIntervalSince(ev.startDate)) < 60)
                }
                if !duplicate {
                    merged.append(ev)
                }
            }
            self.upcomingEvents = merged.sorted { $0.startDate < $1.startDate }
        }
    }

    private func fetchUpcomingEventKitEvents() -> [MimirCalendarEvent] {
        let now = Date()
        let start = now.addingTimeInterval(-15 * 60)
        let end = now.addingTimeInterval(60 * 60)
        let predicate = eventStore.predicateForEvents(withStart: start, end: end, calendars: nil)
        let rawEvents = eventStore.events(matching: predicate)
        return rawEvents.compactMap { ev in
            guard !ev.isAllDay else { return nil }
            let isOngoing = ev.startDate <= now && now <= ev.endDate
            let startsSoon = abs(ev.startDate.timeIntervalSince(now)) <= 15 * 60
            guard isOngoing || startsSoon else { return nil }
            let url = ev.url
                ?? Self.extractMeetingURL(from: ev.location ?? "")
                ?? Self.extractMeetingURL(from: ev.notes ?? "")
            let attendees = (ev.attendees ?? []).compactMap { participant in
                participant.name ?? participant.url.absoluteString.replacingOccurrences(of: "mailto:", with: "")
            }
            return MimirCalendarEvent(
                uid: ev.eventIdentifier ?? UUID().uuidString,
                title: ev.title ?? "Untitled Meeting",
                startDate: ev.startDate,
                endDate: ev.endDate,
                meetingUrl: url,
                attendees: attendees,
                notes: ev.notes
            )
        }
    }

    nonisolated private static func fetchUpcomingIcsEvents(from sources: [String]) -> [MimirCalendarEvent] {
        let now = Date()
        var results: [MimirCalendarEvent] = []
        for src in sources {
            let trimmed = src.trimmingCharacters(in: .whitespacesAndNewlines)
            guard !trimmed.isEmpty else { continue }
            var content: String?
            if trimmed.lowercased().hasPrefix("http://") || trimmed.lowercased().hasPrefix("https://"),
               let url = URL(string: trimmed)
            {
                let sem = DispatchSemaphore(value: 0)
                let req = URLRequest(url: url, timeoutInterval: 10)
                URLSession.shared.dataTask(with: req) { data, _, _ in
                    if let data {
                        content = String(data: data, encoding: .utf8)
                    }
                    sem.signal()
                }.resume()
                sem.wait()
            } else {
                let expanded = (trimmed as NSString).expandingTildeInPath
                content = try? String(contentsOfFile: expanded, encoding: .utf8)
            }
            guard let ics = content else { continue }
            for ev in parseIcsEvents(from: ics) {
                let isOngoing = ev.startDate <= now && now <= ev.endDate
                let startsSoon = abs(ev.startDate.timeIntervalSince(now)) <= 15 * 60
                if isOngoing || startsSoon {
                    results.append(ev)
                }
            }
        }
        return results
    }

    nonisolated private static func parseIcsEvents(from raw: String) -> [MimirCalendarEvent] {
        let normalized = raw.replacingOccurrences(of: "\r\n", with: "\n").replacingOccurrences(of: "\r", with: "\n")
        var unfolded: [String] = []
        for line in normalized.components(separatedBy: "\n") {
            if (line.hasPrefix(" ") || line.hasPrefix("\t")), let last = unfolded.popLast() {
                unfolded.append(last + String(line.dropFirst()))
            } else {
                unfolded.append(line)
            }
        }

        var events: [MimirCalendarEvent] = []
        var inEvent = false
        var uid = ""
        var title = ""
        var startDate: Date?
        var endDate: Date?
        var location = ""
        var description = ""
        var urlProp: URL?
        var attendees: [String] = []

        for line in unfolded {
            if line == "BEGIN:VEVENT" {
                inEvent = true
                uid = ""
                title = ""
                startDate = nil
                endDate = nil
                location = ""
                description = ""
                urlProp = nil
                attendees = []
                continue
            }
            if line == "END:VEVENT" {
                if inEvent, let s = startDate {
                    let e = endDate ?? s.addingTimeInterval(3600)
                    let meetingUrl = urlProp
                        ?? extractMeetingURL(from: location)
                        ?? extractMeetingURL(from: description)
                    events.append(MimirCalendarEvent(
                        uid: uid.isEmpty ? UUID().uuidString : uid,
                        title: title.isEmpty ? "Untitled Meeting" : title,
                        startDate: s,
                        endDate: e,
                        meetingUrl: meetingUrl,
                        attendees: attendees,
                        notes: description.isEmpty ? nil : description
                    ))
                }
                inEvent = false
                continue
            }
            guard inEvent, let colonIdx = line.firstIndex(of: ":") else { continue }
            let left = String(line[..<colonIdx])
            let value = unescapeIcsText(String(line[line.index(after: colonIdx)...]))
            let propName = left.split(separator: ";").first.map { String($0).uppercased() } ?? ""

            switch propName {
            case "UID":
                uid = value
            case "SUMMARY":
                title = value
            case "LOCATION":
                location = value
            case "DESCRIPTION":
                description = value
            case "URL":
                if let u = URL(string: value.trimmingCharacters(in: .whitespacesAndNewlines)) {
                    urlProp = u
                }
            case "DTSTART":
                startDate = parseIcsDate(value, params: left)
            case "DTEND":
                endDate = parseIcsDate(value, params: left)
            case "ATTENDEE":
                var cn: String?
                for part in left.split(separator: ";").dropFirst() {
                    if part.uppercased().hasPrefix("CN=") {
                        cn = String(part.dropFirst(3)).trimmingCharacters(in: CharacterSet(charactersIn: "\""))
                    }
                }
                let email = value.replacingOccurrences(of: "mailto:", with: "", options: [.caseInsensitive, .anchored])
                    .trimmingCharacters(in: .whitespacesAndNewlines)
                let label = (cn?.isEmpty == false ? cn! : email)
                if !label.isEmpty && !attendees.contains(where: { $0.caseInsensitiveCompare(label) == .orderedSame }) {
                    attendees.append(label)
                }
            default:
                break
            }
        }
        return events
    }

    nonisolated private static func unescapeIcsText(_ s: String) -> String {
        s.replacingOccurrences(of: "\\n", with: "\n")
            .replacingOccurrences(of: "\\N", with: "\n")
            .replacingOccurrences(of: "\\,", with: ",")
            .replacingOccurrences(of: "\\;", with: ";")
            .replacingOccurrences(of: "\\\\", with: "\\")
    }

    nonisolated private static func parseIcsDate(_ value: String, params: String) -> Date? {
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        var tz: TimeZone = .current
        for part in params.split(separator: ";").dropFirst() {
            if part.uppercased().hasPrefix("TZID=") {
                let tzId = String(part.dropFirst(5)).trimmingCharacters(in: CharacterSet(charactersIn: "\""))
                if let found = TimeZone(identifier: tzId) {
                    tz = found
                }
            }
        }
        let fmt = DateFormatter()
        fmt.locale = Locale(identifier: "en_US_POSIX")
        if trimmed.hasSuffix("Z") {
            fmt.timeZone = TimeZone(secondsFromGMT: 0)
            fmt.dateFormat = "yyyyMMdd'T'HHmmss'Z'"
            if let d = fmt.date(from: trimmed) { return d }
        }
        fmt.timeZone = tz
        fmt.dateFormat = "yyyyMMdd'T'HHmmss"
        if let d = fmt.date(from: trimmed) { return d }
        fmt.dateFormat = "yyyyMMdd"
        return fmt.date(from: trimmed)
    }

    func joinAndRecord(_ event: MimirCalendarEvent, openURL: Bool) {
        if openURL, let url = event.meetingUrl {
            NSWorkspace.shared.open(url)
        }
        sessionTitle = event.title
        var existingKws = keywordsText
            .split(separator: ",")
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }
        for att in event.attendees {
            if !existingKws.contains(where: { $0.caseInsensitiveCompare(att) == .orderedSame }) {
                existingKws.append(att)
            }
        }
        keywordsText = existingKws.joined(separator: ", ")
        pendingCalendarEvent = event
        startRecording()
    }

    nonisolated private static func extractMeetingURL(from text: String) -> URL? {
        let separators = CharacterSet.whitespacesAndNewlines.union(CharacterSet(charactersIn: "<>\"'()"))
        for token in text.components(separatedBy: separators) {
            let cleaned = token.trimmingCharacters(in: CharacterSet(charactersIn: ",.;\\"))
            let lower = cleaned.lowercased()
            let isScheme = lower.hasPrefix("https://") || lower.hasPrefix("http://") || lower.hasPrefix("zoommtg://")
            guard isScheme else { continue }
            let isMeeting = lower.hasPrefix("zoommtg://")
                || (lower.contains("zoom.us/") && (lower.contains("/j/") || lower.contains("/my/") || lower.contains("/wc/")))
                || lower.contains("meet.google.com/")
                || lower.contains("teams.microsoft.com/l/meetup-join")
                || lower.contains("teams.live.com/meet/")
                || (lower.contains("webex.com/") && (lower.contains("/meet/") || lower.contains("/join/") || lower.contains("j.php")))
            if isMeeting, let url = URL(string: cleaned) {
                return url
            }
        }
        return nil
    }
}
