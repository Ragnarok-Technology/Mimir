import MimirCore
import SwiftUI

@main
struct MimirMacApp: App {
    @StateObject private var controller = MimirController()

    var body: some Scene {
        WindowGroup("Mimir") {
            MainView(controller: controller)
                .frame(minWidth: 780, minHeight: 520)
                .preferredColorScheme(.dark)
        }

        MenuBarExtra {
            Button(controller.isRecording ? "Stop Recording" : "Start Recording") {
                controller.toggleRecording()
            }
            Button("Copy Latest Transcript") {
                controller.copyLatestTranscript()
            }
            Divider()
            Button("Check for Updates…") {
                controller.checkForUpdates()
            }
            Divider()
            Button("Quit Mimir") {
                NSApplication.shared.terminate(nil)
            }
        } label: {
            Image(nsImage: makeMenuBarTemplateIcon(isRecording: controller.isRecording))
                .accessibilityLabel("Mimir")
        }
    }
}

private func makeMenuBarTemplateIcon(isRecording: Bool) -> NSImage {
    let size = NSSize(width: 18, height: 18)
    let image = NSImage(size: size, flipped: true) { rect in
        let s = min(rect.width, rect.height) / 512.0
        func fillPoly(_ pts: [(CGFloat, CGFloat)], alpha: CGFloat) {
            let path = NSBezierPath()
            guard let first = pts.first else { return }
            path.move(to: NSPoint(x: first.0 * s, y: first.1 * s))
            for p in pts.dropFirst() {
                path.line(to: NSPoint(x: p.0 * s, y: p.1 * s))
            }
            path.close()
            NSColor.black.withAlphaComponent(alpha).setFill()
            path.fill()
        }
        let boost: CGFloat = isRecording ? 1.0 : 0.9
        fillPoly([(92, 92), (156, 92), (256, 192), (356, 92), (420, 92), (420, 104), (256, 268), (92, 104)], alpha: 1.0 * boost)
        fillPoly([(92, 125.21), (156, 189.21), (156, 344.23), (92, 394.15)], alpha: 0.85 * boost)
        fillPoly([(356, 189.21), (420, 125.21), (420, 394.15), (356, 344.23)], alpha: 0.72 * boost)
        fillPoly([(92, 413.17), (156, 363.25), (156, 420), (92, 420)], alpha: 0.58 * boost)
        fillPoly([(356, 363.25), (420, 413.17), (420, 420), (356, 420)], alpha: 0.58 * boost)
        return true
    }
    image.isTemplate = true
    return image
}

private enum MimirTheme {
    static let voidBg = Color(red: 6 / 255, green: 8 / 255, blue: 13 / 255)         // #06080D
    static let railBg = Color(red: 8 / 255, green: 12 / 255, blue: 20 / 255)        // #080C14
    static let cardBg = Color(red: 11 / 255, green: 16 / 255, blue: 26 / 255)       // #0B101A
    static let elevatedBg = Color(red: 15 / 255, green: 23 / 255, blue: 42 / 255)   // #0F172A
    static let activeFill = Color(red: 12 / 255, green: 27 / 255, blue: 46 / 255)   // #0C1B2E
    static let hairline = Color(red: 24 / 255, green: 36 / 255, blue: 54 / 255)     // #182436
    static let ice = Color(red: 103 / 255, green: 232 / 255, blue: 249 / 255)       // #67E8F9
    static let cyan = Color(red: 56 / 255, green: 189 / 255, blue: 248 / 255)       // #38BDF8
    static let azure = Color(red: 14 / 255, green: 165 / 255, blue: 233 / 255)      // #0EA5E9
    static let cobalt = Color(red: 37 / 255, green: 99 / 255, blue: 235 / 255)      // #2563EB
    static let textPrimary = Color(red: 226 / 255, green: 232 / 255, blue: 240 / 255) // #E2E8F0
    static let textSecondary = Color(red: 148 / 255, green: 163 / 255, blue: 184 / 255) // #94A3B8
    static let textMuted = Color(red: 100 / 255, green: 116 / 255, blue: 139 / 255) // #64748B
}

private enum WorkspaceSection: String, CaseIterable {
    case live = "Live"
    case transcripts = "Transcripts"
    case assistants = "Assistants & Updates"
}

private struct MimirPrismMark: View {
    var size: CGFloat = 20
    var grayscale: Bool = true
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        Canvas { ctx, canvasSize in
            let s = min(canvasSize.width, canvasSize.height) / 512.0
            let base: Color = colorScheme == .dark ? .white : .black
            let cTop = grayscale ? base.opacity(0.95) : MimirTheme.ice
            let cLeft = grayscale ? base.opacity(0.76) : MimirTheme.cyan
            let cRight = grayscale ? base.opacity(0.60) : MimirTheme.azure
            let cBot = grayscale ? base.opacity(0.45) : MimirTheme.cobalt

            func poly(_ pts: [(CGFloat, CGFloat)], _ color: Color) {
                var path = Path()
                guard let first = pts.first else { return }
                path.move(to: CGPoint(x: first.0 * s, y: first.1 * s))
                for p in pts.dropFirst() {
                    path.addLine(to: CGPoint(x: p.0 * s, y: p.1 * s))
                }
                path.closeSubpath()
                ctx.fill(path, with: .color(color))
            }
            poly([(92, 92), (156, 92), (256, 192), (356, 92), (420, 92), (420, 104), (256, 268), (92, 104)], cTop)
            poly([(92, 125.21), (156, 189.21), (156, 344.23), (92, 394.15)], cLeft)
            poly([(356, 189.21), (420, 125.21), (420, 394.15), (356, 344.23)], cRight)
            poly([(92, 413.17), (156, 363.25), (156, 420), (92, 420)], cBot)
            poly([(356, 363.25), (420, 413.17), (420, 420), (356, 420)], cBot)
        }
        .frame(width: size, height: size)
    }
}

private struct SegmentedMeter: View {
    let label: String
    let level: Double
    let activeColor: Color

    var body: some View {
        HStack(spacing: 3) {
            Text(label)
                .font(.system(size: 9.5, weight: .semibold, design: .monospaced))
                .foregroundStyle(MimirTheme.textMuted)
            let lit = Int(ceil(min(max(level, 0), 1) * 6.0))
            HStack(spacing: 2) {
                ForEach(0..<6, id: \.self) { idx in
                    RoundedRectangle(cornerRadius: 1)
                        .fill(idx < lit ? activeColor : MimirTheme.hairline)
                        .frame(width: 3, height: 10)
                }
            }
        }
    }
}

struct MainView: View {
    @ObservedObject var controller: MimirController
    @State private var section: WorkspaceSection = .live
    @State private var miniMode = false

    var body: some View {
        VStack(spacing: 0) {
            // Top Telemetry Strip
            HStack(spacing: 12) {
                MimirPrismMark(size: 20)
                Text("MIMIR")
                    .font(.system(size: 13, weight: .bold))
                    .tracking(2.2)
                    .foregroundStyle(MimirTheme.textPrimary)

                HStack(spacing: 6) {
                    Circle()
                        .fill(controller.isRecording ? MimirTheme.ice : MimirTheme.cyan)
                        .frame(width: 7, height: 7)
                    Text(controller.isRecording ? "LIVE · CAPTURING" : "STANDBY")
                        .font(.system(size: 10.5, weight: .semibold, design: .monospaced))
                        .foregroundStyle(controller.isRecording ? MimirTheme.ice : MimirTheme.textSecondary)
                }
                .padding(.horizontal, 9)
                .padding(.vertical, 4)
                .background(controller.isRecording ? MimirTheme.activeFill : MimirTheme.cardBg)
                .overlay(RoundedRectangle(cornerRadius: 5).stroke(MimirTheme.hairline, lineWidth: 1))
                .clipShape(RoundedRectangle(cornerRadius: 5))

                SegmentedMeter(label: "MIC", level: controller.micActivity, activeColor: MimirTheme.cyan)
                SegmentedMeter(label: "SYS", level: controller.sysActivity, activeColor: MimirTheme.azure)

                Spacer()

                if !miniMode {
                    HStack(spacing: 4) {
                        ForEach(WorkspaceSection.allCases, id: \.self) { item in
                            Button(item.rawValue) {
                                section = item
                            }
                            .buttonStyle(.plain)
                            .font(.system(size: 11.5, weight: section == item ? .semibold : .medium))
                            .foregroundStyle(section == item ? MimirTheme.textPrimary : MimirTheme.textSecondary)
                            .padding(.horizontal, 10)
                            .padding(.vertical, 5)
                            .background(section == item ? MimirTheme.elevatedBg : Color.clear)
                            .overlay(
                                RoundedRectangle(cornerRadius: 5)
                                    .stroke(section == item ? MimirTheme.hairline : Color.clear, lineWidth: 1)
                            )
                            .clipShape(RoundedRectangle(cornerRadius: 5))
                        }
                    }
                }

                Button(miniMode ? "Full View" : "Mini Mode") {
                    miniMode.toggle()
                    if miniMode { section = .live }
                }
                .buttonStyle(.plain)
                .font(.system(size: 11, weight: .medium))
                .foregroundStyle(MimirTheme.textSecondary)
                .padding(.horizontal, 9)
                .padding(.vertical, 4)
                .background(MimirTheme.cardBg)
                .overlay(RoundedRectangle(cornerRadius: 5).stroke(MimirTheme.hairline, lineWidth: 1))
                .clipShape(RoundedRectangle(cornerRadius: 5))
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
            .background(MimirTheme.railBg)
            .overlay(alignment: .bottom) {
                Rectangle().fill(MimirTheme.hairline).frame(height: 1)
            }

            // Main Workspace
            HStack(spacing: 0) {
                if !miniMode {
                    LeftControlRail(controller: controller, section: $section)
                        .frame(width: 232)
                        .background(MimirTheme.railBg)
                        .overlay(alignment: .trailing) {
                            Rectangle().fill(MimirTheme.hairline).frame(width: 1)
                        }
                }

                Group {
                    switch section {
                    case .live:
                        RecordTab(controller: controller)
                    case .transcripts:
                        SessionsTab(controller: controller)
                    case .assistants:
                        AssistantsTab(controller: controller)
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(MimirTheme.voidBg)
            }

            // Bottom Action Dock
            HStack(spacing: 10) {
                Button("Copy Latest") {
                    controller.copyLatestTranscript()
                }
                .buttonStyle(.plain)
                .font(.system(size: 11.5, weight: .medium))
                .foregroundStyle(MimirTheme.textPrimary)
                .padding(.horizontal, 10)
                .padding(.vertical, 4)
                .background(MimirTheme.elevatedBg)
                .overlay(RoundedRectangle(cornerRadius: 5).stroke(MimirTheme.hairline, lineWidth: 1))
                .clipShape(RoundedRectangle(cornerRadius: 5))

                Button("Connect AI (MCP)") {
                    controller.connectAssistants()
                    section = .assistants
                }
                .buttonStyle(.plain)
                .font(.system(size: 11.5, weight: .medium))
                .foregroundStyle(MimirTheme.ice)
                .padding(.horizontal, 10)
                .padding(.vertical, 4)
                .background(MimirTheme.activeFill)
                .overlay(RoundedRectangle(cornerRadius: 5).stroke(MimirTheme.azure.opacity(0.5), lineWidth: 1))
                .clipShape(RoundedRectangle(cornerRadius: 5))

                Text(controller.statusMessage)
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundStyle(MimirTheme.textMuted)
                    .lineLimit(1)

                Spacer()

                Text("whistle.cact · 100% local · zero telemetry")
                    .font(.system(size: 10.5, design: .monospaced))
                    .foregroundStyle(MimirTheme.textMuted)
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
            .background(MimirTheme.railBg)
            .overlay(alignment: .top) {
                Rectangle().fill(MimirTheme.hairline).frame(height: 1)
            }
        }
        .background(MimirTheme.voidBg)
        .tint(MimirTheme.cyan)
    }
}

private struct LeftControlRail: View {
    @ObservedObject var controller: MimirController
    @Binding var section: WorkspaceSection

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Button(action: controller.toggleRecording) {
                HStack(spacing: 8) {
                    RoundedRectangle(cornerRadius: 2)
                        .fill(controller.isRecording ? MimirTheme.ice : MimirTheme.cyan)
                        .frame(width: 10, height: 10)
                    Text(controller.isRecording ? "Stop Capture" : "Start Capture")
                        .font(.system(size: 13, weight: .semibold))
                        .foregroundStyle(MimirTheme.textPrimary)
                    Spacer()
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 9)
                .background(MimirTheme.activeFill)
                .overlay(
                    RoundedRectangle(cornerRadius: 6)
                        .stroke(controller.isRecording ? MimirTheme.ice : MimirTheme.cyan, lineWidth: 1)
                )
                .clipShape(RoundedRectangle(cornerRadius: 6))
            }
            .buttonStyle(.plain)

            VStack(alignment: .leading, spacing: 8) {
                Text("CAPTURE")
                    .font(.system(size: 10, weight: .semibold, design: .monospaced))
                    .foregroundStyle(MimirTheme.textMuted)
                Toggle("System Audio", isOn: $controller.captureSystemAudio)
                    .disabled(controller.isRecording)
                Toggle("Multi-Speaker Mic", isOn: $controller.diarizeMic)
                    .disabled(controller.isRecording)
                Toggle("Save Local WAVs", isOn: $controller.saveWav)
                    .disabled(controller.isRecording)
            }
            .font(.system(size: 12))
            .foregroundStyle(MimirTheme.textSecondary)

            Rectangle().fill(MimirTheme.hairline).frame(height: 1)

            HStack {
                Text("SESSIONS")
                    .font(.system(size: 10, weight: .semibold, design: .monospaced))
                    .foregroundStyle(MimirTheme.textMuted)
                Spacer()
                Button(controller.showArchived ? "All" : "Active") {
                    controller.showArchived.toggle()
                    controller.refreshSessions()
                }
                .buttonStyle(.plain)
                .font(.system(size: 10, design: .monospaced))
                .foregroundStyle(controller.showArchived ? MimirTheme.ice : MimirTheme.textMuted)
                .padding(.horizontal, 6)
                .padding(.vertical, 2)
                .background(MimirTheme.cardBg)
                .overlay(RoundedRectangle(cornerRadius: 4).stroke(MimirTheme.hairline, lineWidth: 1))
                Button(action: controller.refreshSessions) {
                    Image(systemName: "arrow.clockwise")
                        .font(.system(size: 11))
                        .foregroundStyle(MimirTheme.textMuted)
                }
                .buttonStyle(.plain)
            }

            ScrollView {
                LazyVStack(alignment: .leading, spacing: 6) {
                    ForEach(controller.sessions) { s in
                        let selected = controller.selectedSessionId == s.id
                        Button {
                            controller.selectSession(s.id)
                            section = .transcripts
                        } label: {
                            HStack(spacing: 8) {
                                RoundedRectangle(cornerRadius: 1.5)
                                    .fill(selected ? MimirTheme.cyan : Color.clear)
                                    .frame(width: 3, height: 30)
                                VStack(alignment: .leading, spacing: 2) {
                                    Text(s.title)
                                        .font(.system(size: 12, weight: selected ? .semibold : .medium))
                                        .foregroundStyle(selected ? MimirTheme.textPrimary : (s.archived ? MimirTheme.textMuted : MimirTheme.textSecondary))
                                        .lineLimit(1)
                                    Text(s.archived ? "\(s.utteranceCount) turns · archived" : "\(s.utteranceCount) turns")
                                        .font(.system(size: 10, design: .monospaced))
                                        .foregroundStyle(selected ? MimirTheme.ice : MimirTheme.textMuted)
                                }
                                Spacer()
                            }
                            .padding(.vertical, 6)
                            .padding(.trailing, 8)
                            .background(selected ? MimirTheme.elevatedBg : Color.clear)
                            .overlay(
                                RoundedRectangle(cornerRadius: 6)
                                    .stroke(selected ? MimirTheme.hairline : Color.clear, lineWidth: 1)
                            )
                            .clipShape(RoundedRectangle(cornerRadius: 6))
                        }
                        .buttonStyle(.plain)
                    }
                }
            }
        }
        .padding(14)
    }
}

struct RecordTab: View {
    @ObservedObject var controller: MimirController

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 10) {
                TextField("Session title (optional)", text: $controller.sessionTitle)
                    .textFieldStyle(.plain)
                    .padding(.horizontal, 10)
                    .padding(.vertical, 7)
                    .background(MimirTheme.cardBg)
                    .overlay(RoundedRectangle(cornerRadius: 6).stroke(MimirTheme.hairline, lineWidth: 1))
                    .disabled(controller.isRecording)

                TextField("Keywords (comma-separated)", text: $controller.keywordsText)
                    .textFieldStyle(.plain)
                    .padding(.horizontal, 10)
                    .padding(.vertical, 7)
                    .background(MimirTheme.cardBg)
                    .overlay(RoundedRectangle(cornerRadius: 6).stroke(MimirTheme.hairline, lineWidth: 1))
                    .disabled(controller.isRecording)
            }
            .font(.system(size: 12.5))
            .foregroundStyle(MimirTheme.textPrimary)

            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 8) {
                        if controller.liveLines.isEmpty {
                            VStack(alignment: .leading, spacing: 6) {
                                Text("Ready to refract dual-channel audio locally.")
                                    .font(.system(size: 14, weight: .medium))
                                    .foregroundStyle(MimirTheme.textPrimary)
                                Text("Press Start Capture to transcribe mic (Cyan) and system audio (Azure/Cobalt) with Whistle AI.")
                                    .font(.system(size: 12.5))
                                    .foregroundStyle(MimirTheme.textSecondary)
                            }
                            .padding(16)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .background(MimirTheme.cardBg)
                            .overlay(RoundedRectangle(cornerRadius: 8).stroke(MimirTheme.hairline, lineWidth: 1))
                        } else {
                            ForEach(Array(controller.liveLines.enumerated()), id: \.offset) { _, line in
                                let isSystem = line.contains("[System]") || line.contains("System")
                                HStack(alignment: .top, spacing: 10) {
                                    RoundedRectangle(cornerRadius: 1.5)
                                        .fill(isSystem ? MimirTheme.azure : MimirTheme.cyan)
                                        .frame(width: 3)
                                        .padding(.vertical, 2)
                                    Text(line)
                                        .textSelection(.enabled)
                                        .font(.system(size: 13.5))
                                        .foregroundStyle(MimirTheme.textPrimary)
                                        .frame(maxWidth: .infinity, alignment: .leading)
                                }
                                .padding(12)
                                .background(MimirTheme.cardBg)
                                .overlay(RoundedRectangle(cornerRadius: 8).stroke(MimirTheme.hairline, lineWidth: 1))
                            }
                        }
                        Color.clear.frame(height: 1).id("live_bottom")
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                .onChange(of: controller.liveLines.count) {
                    withAnimation(.easeOut(duration: 0.15)) {
                        proxy.scrollTo("live_bottom", anchor: .bottom)
                    }
                }
            }
        }
        .padding(16)
    }
}

struct SessionsTab: View {
    @ObservedObject var controller: MimirController
    @State private var confirmDelete = false

    private var selectedSummary: MimirSessionSummary? {
        guard let id = controller.selectedSessionId else { return nil }
        return controller.sessions.first(where: { $0.id == id })
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                TextField("Search transcripts (FTS5)…", text: $controller.searchQuery)
                    .textFieldStyle(.plain)
                    .font(.system(size: 12))
                    .foregroundStyle(MimirTheme.textPrimary)
                    .padding(.horizontal, 10)
                    .padding(.vertical, 6)
                    .background(MimirTheme.cardBg)
                    .overlay(RoundedRectangle(cornerRadius: 6).stroke(MimirTheme.hairline, lineWidth: 1))
                    .onSubmit { controller.runSearch() }

                Button("Search") {
                    controller.runSearch()
                }
                .buttonStyle(.plain)
                .font(.system(size: 11.5, weight: .medium))
                .foregroundStyle(MimirTheme.textPrimary)
                .padding(.horizontal, 10)
                .padding(.vertical, 5)
                .background(MimirTheme.elevatedBg)
                .overlay(RoundedRectangle(cornerRadius: 5).stroke(MimirTheme.hairline, lineWidth: 1))

                if !controller.searchResults.isEmpty {
                    Button("Clear") {
                        controller.clearSearch()
                    }
                    .buttonStyle(.plain)
                    .font(.system(size: 11.5, weight: .medium))
                    .foregroundStyle(MimirTheme.textSecondary)
                    .padding(.horizontal, 8)
                    .padding(.vertical, 5)
                    .background(MimirTheme.cardBg)
                    .overlay(RoundedRectangle(cornerRadius: 5).stroke(MimirTheme.hairline, lineWidth: 1))
                }
            }

            if !controller.searchResults.isEmpty {
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 4) {
                        ForEach(controller.searchResults) { hit in
                            Button {
                                controller.selectSession(hit.session_id)
                            } label: {
                                HStack(spacing: 6) {
                                    Text(hit.session_title ?? "Untitled")
                                        .font(.system(size: 11, weight: .semibold))
                                        .foregroundStyle(MimirTheme.ice)
                                    Text("\(hit.speaker_label):")
                                        .font(.system(size: 11, weight: .medium))
                                        .foregroundStyle(MimirTheme.cyan)
                                    Text(hit.text)
                                        .font(.system(size: 11))
                                        .foregroundStyle(MimirTheme.textPrimary)
                                        .lineLimit(1)
                                    Spacer()
                                }
                                .padding(.horizontal, 8)
                                .padding(.vertical, 4)
                                .background(MimirTheme.cardBg)
                                .clipShape(RoundedRectangle(cornerRadius: 4))
                            }
                            .buttonStyle(.plain)
                        }
                    }
                }
                .frame(maxHeight: 120)
                .padding(6)
                .background(MimirTheme.railBg)
                .overlay(RoundedRectangle(cornerRadius: 6).stroke(MimirTheme.hairline, lineWidth: 1))
            }

            HStack(spacing: 8) {
                TextField("Session title…", text: $controller.selectedTitleEdit)
                    .textFieldStyle(.plain)
                    .font(.system(size: 12.5, weight: .medium))
                    .foregroundStyle(MimirTheme.textPrimary)
                    .padding(.horizontal, 10)
                    .padding(.vertical, 6)
                    .background(MimirTheme.cardBg)
                    .overlay(RoundedRectangle(cornerRadius: 6).stroke(MimirTheme.hairline, lineWidth: 1))
                    .disabled(selectedSummary == nil)
                    .onSubmit { controller.renameSelectedSession() }

                Button("Rename") {
                    controller.renameSelectedSession()
                }
                .buttonStyle(.plain)
                .font(.system(size: 11.5, weight: .medium))
                .foregroundStyle(MimirTheme.textPrimary)
                .padding(.horizontal, 10)
                .padding(.vertical, 5)
                .background(MimirTheme.elevatedBg)
                .overlay(RoundedRectangle(cornerRadius: 5).stroke(MimirTheme.hairline, lineWidth: 1))
                .disabled(selectedSummary == nil)

                Button(selectedSummary?.archived == true ? "Unarchive" : "Archive") {
                    controller.toggleArchiveSelectedSession()
                }
                .buttonStyle(.plain)
                .font(.system(size: 11.5, weight: .medium))
                .foregroundStyle(MimirTheme.textSecondary)
                .padding(.horizontal, 10)
                .padding(.vertical, 5)
                .background(MimirTheme.cardBg)
                .overlay(RoundedRectangle(cornerRadius: 5).stroke(MimirTheme.hairline, lineWidth: 1))
                .disabled(selectedSummary == nil)

                Button("Delete") {
                    confirmDelete = true
                }
                .buttonStyle(.plain)
                .font(.system(size: 11.5, weight: .medium))
                .foregroundStyle(Color.red.opacity(0.9))
                .padding(.horizontal, 10)
                .padding(.vertical, 5)
                .background(MimirTheme.cardBg)
                .overlay(RoundedRectangle(cornerRadius: 5).stroke(Color.red.opacity(0.35), lineWidth: 1))
                .disabled(selectedSummary == nil)
                .alert("Delete Transcript?", isPresented: $confirmDelete) {
                    Button("Delete", role: .destructive) {
                        controller.deleteSelectedSession()
                    }
                    Button("Cancel", role: .cancel) {}
                } message: {
                    Text("This permanently deletes the transcript and its local search index.")
                }

                Button("Copy Markdown") {
                    controller.copySelectedTranscript()
                }
                .buttonStyle(.plain)
                .font(.system(size: 11.5, weight: .medium))
                .foregroundStyle(MimirTheme.ice)
                .padding(.horizontal, 10)
                .padding(.vertical, 5)
                .background(MimirTheme.activeFill)
                .overlay(RoundedRectangle(cornerRadius: 5).stroke(MimirTheme.hairline, lineWidth: 1))
                .disabled(controller.selectedTranscript.isEmpty)
            }

            HStack(spacing: 8) {
                TextField("Speaker ID (e.g. spk_1)", text: $controller.speakerIdEdit)
                    .textFieldStyle(.plain)
                    .font(.system(size: 11.5))
                    .foregroundStyle(MimirTheme.textPrimary)
                    .padding(.horizontal, 8)
                    .padding(.vertical, 5)
                    .frame(width: 150)
                    .background(MimirTheme.cardBg)
                    .overlay(RoundedRectangle(cornerRadius: 5).stroke(MimirTheme.hairline, lineWidth: 1))
                    .disabled(selectedSummary == nil)

                TextField("New speaker name", text: $controller.speakerLabelEdit)
                    .textFieldStyle(.plain)
                    .font(.system(size: 11.5))
                    .foregroundStyle(MimirTheme.textPrimary)
                    .padding(.horizontal, 8)
                    .padding(.vertical, 5)
                    .frame(width: 180)
                    .background(MimirTheme.cardBg)
                    .overlay(RoundedRectangle(cornerRadius: 5).stroke(MimirTheme.hairline, lineWidth: 1))
                    .disabled(selectedSummary == nil)
                    .onSubmit { controller.renameSpeakerInline() }

                Button("Rename Speaker") {
                    controller.renameSpeakerInline()
                }
                .buttonStyle(.plain)
                .font(.system(size: 11.5, weight: .medium))
                .foregroundStyle(MimirTheme.textPrimary)
                .padding(.horizontal, 10)
                .padding(.vertical, 5)
                .background(MimirTheme.elevatedBg)
                .overlay(RoundedRectangle(cornerRadius: 5).stroke(MimirTheme.hairline, lineWidth: 1))
                .disabled(selectedSummary == nil)

                Spacer()
            }

            ScrollView {
                Text(controller.selectedTranscript.isEmpty ? "Select a session from the left rail to inspect its transcript." : controller.selectedTranscript)
                    .textSelection(.enabled)
                    .font(.system(size: 13, design: .monospaced))
                    .foregroundStyle(MimirTheme.textPrimary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(14)
            }
            .background(MimirTheme.cardBg)
            .overlay(RoundedRectangle(cornerRadius: 8).stroke(MimirTheme.hairline, lineWidth: 1))
        }
        .padding(16)
    }
}

struct AssistantsTab: View {
    @ObservedObject var controller: MimirController

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            VStack(alignment: .leading, spacing: 10) {
                Text("MCP ASSISTANT AUTO-CONNECT")
                    .font(.system(size: 10.5, weight: .semibold, design: .monospaced))
                    .foregroundStyle(MimirTheme.textMuted)
                Text("Register Mimir's local MCP server with Claude Desktop, Claude Code, Cursor, Gemini CLI, Antigravity, and Windsurf.")
                    .font(.system(size: 13))
                    .foregroundStyle(MimirTheme.textSecondary)
                Button("Connect Installed Assistants") {
                    controller.connectAssistants()
                }
                .buttonStyle(.borderedProminent)
                .tint(MimirTheme.azure)

                if !controller.connectReport.isEmpty {
                    Text(controller.connectReport)
                        .font(.system(size: 12, design: .monospaced))
                        .foregroundStyle(MimirTheme.ice)
                        .textSelection(.enabled)
                        .padding(10)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(MimirTheme.voidBg)
                        .overlay(RoundedRectangle(cornerRadius: 6).stroke(MimirTheme.hairline, lineWidth: 1))
                }
            }
            .padding(14)
            .background(MimirTheme.cardBg)
            .overlay(RoundedRectangle(cornerRadius: 8).stroke(MimirTheme.hairline, lineWidth: 1))

            VStack(alignment: .leading, spacing: 10) {
                Text("WHISTLE MODEL & UPDATES")
                    .font(.system(size: 10.5, weight: .semibold, design: .monospaced))
                    .foregroundStyle(MimirTheme.textMuted)
                HStack {
                    Button("Download / Verify whistle.cact (16.9 MB)") {
                        controller.fetchModel()
                    }
                    Spacer()
                    Button("Check for App Updates…") {
                        controller.checkForUpdates()
                    }
                }
                if !controller.updateMessage.isEmpty {
                    HStack {
                        Text(controller.updateMessage)
                            .font(.system(size: 12.5))
                            .foregroundStyle(MimirTheme.textSecondary)
                        if let url = controller.updateDownloadURL {
                            Link("Download Release", destination: url)
                                .foregroundStyle(MimirTheme.cyan)
                        }
                    }
                }
            }
            .padding(14)
            .background(MimirTheme.cardBg)
            .overlay(RoundedRectangle(cornerRadius: 8).stroke(MimirTheme.hairline, lineWidth: 1))

            Spacer()
        }
        .padding(16)
    }
}
