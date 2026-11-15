import Foundation

public struct MimirSessionSummary: Identifiable, Hashable, Codable, Sendable {
    public let id: String
    public let startedAt: String
    public let utteranceCount: Int
    public let title: String
    public let archived: Bool

    public init(id: String, startedAt: String, utteranceCount: Int, title: String, archived: Bool = false) {
        self.id = id
        self.startedAt = startedAt
        self.utteranceCount = utteranceCount
        self.title = title
        self.archived = archived
    }

    public static func parseCliLines(_ output: String) -> [MimirSessionSummary] {
        output
            .split(separator: "\n")
            .compactMap { line in
                let parts = line.split(separator: "|", maxSplits: 4).map {
                    $0.trimmingCharacters(in: .whitespaces)
                }
                guard parts.count >= 4 else { return nil }
                let countStr = parts[2].replacingOccurrences(of: " utterances", with: "")
                let count = Int(countStr) ?? 0
                if parts.count >= 5 {
                    return MimirSessionSummary(
                        id: parts[0],
                        startedAt: parts[1],
                        utteranceCount: count,
                        title: parts[4],
                        archived: parts[3].lowercased() == "archived"
                    )
                }
                return MimirSessionSummary(
                    id: parts[0],
                    startedAt: parts[1],
                    utteranceCount: count,
                    title: parts[3],
                    archived: false
                )
            }
    }
}
