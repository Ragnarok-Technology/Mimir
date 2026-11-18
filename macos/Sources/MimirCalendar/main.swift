import EventKit
import Foundation

struct CalendarEventDTO: Encodable {
    let uid: String
    let title: String
    let start_ts: Int64
    let end_ts: Int64
    let meeting_url: String?
    let attendees: [String]
    let notes_or_agenda: String?
    let source: String
}

struct CalendarHelperOutput: Encodable {
    let authorized: Bool
    let status: String
    let events: [CalendarEventDTO]
}

func extractMeetingURL(from texts: [String?]) -> String? {
    let patterns = [
        "zoommtg://",
        "zoom.us/j/",
        "zoom.us/my/",
        "zoom.us/wc/",
        "meet.google.com/",
        "teams.microsoft.com/l/meetup-join",
        "teams.live.com/meet/",
        "webex.com/meet/",
        "webex.com/join/"
    ]
    for raw in texts {
        guard let text = raw, !text.isEmpty else { continue }
        let tokens = text.components(separatedBy: CharacterSet.whitespacesAndNewlines.union(CharacterSet(charactersIn: "<>\"'()")))
        for token in tokens {
            let trimmed = token.trimmingCharacters(in: CharacterSet(charactersIn: ",.;\\"))
            let lower = trimmed.lowercased()
            if (lower.hasPrefix("https://") || lower.hasPrefix("http://") || lower.hasPrefix("zoommtg://")),
               patterns.contains(where: { lower.contains($0) }) {
                return trimmed
            }
        }
    }
    return nil
}

@main
struct MimirCalendarCLI {
    static func main() async {
        let args = CommandLine.arguments
        let requestAccess = args.contains("--request-access")
        let store = EKEventStore()
        var status = EKEventStore.authorizationStatus(for: .event)

        if requestAccess && status == .notDetermined {
            _ = try? await store.requestFullAccessToEvents()
            status = EKEventStore.authorizationStatus(for: .event)
        }

        let statusLabel: String
        let authorized: Bool
        switch status {
        case .fullAccess:
            statusLabel = "authorized"
            authorized = true
        case .writeOnly:
            statusLabel = "write_only"
            authorized = false
        case .denied:
            statusLabel = "denied"
            authorized = false
        case .restricted:
            statusLabel = "restricted"
            authorized = false
        case .notDetermined:
            statusLabel = "not_determined"
            authorized = false
        @unknown default:
            statusLabel = "unknown"
            authorized = false
        }

        var dtos: [CalendarEventDTO] = []
        if authorized {
            let now = Date()
            let start = now.addingTimeInterval(-15 * 60)
            let end = now.addingTimeInterval(24 * 60 * 60)
            let predicate = store.predicateForEvents(withStart: start, end: end, calendars: nil)
            let events = store.events(matching: predicate)
            for ev in events {
                let attendees = (ev.attendees ?? []).compactMap { p -> String? in
                    p.name ?? p.url.absoluteString.replacingOccurrences(of: "mailto:", with: "")
                }
                let url = extractMeetingURL(from: [ev.url?.absoluteString, ev.location, ev.notes])
                let uid = ev.eventIdentifier ?? UUID().uuidString
                let title = (ev.title ?? "Untitled").trimmingCharacters(in: .whitespacesAndNewlines)
                dtos.append(CalendarEventDTO(
                    uid: uid,
                    title: title.isEmpty ? "Untitled" : title,
                    start_ts: Int64(ev.startDate.timeIntervalSince1970),
                    end_ts: Int64(ev.endDate.timeIntervalSince1970),
                    meeting_url: url,
                    attendees: attendees,
                    notes_or_agenda: ev.notes,
                    source: "apple_calendar"
                ))
            }
        }

        let out = CalendarHelperOutput(authorized: authorized, status: statusLabel, events: dtos)
        if let data = try? JSONEncoder().encode(out) {
            FileHandle.standardOutput.write(data)
            FileHandle.standardOutput.write(Data("\n".utf8))
        }
    }
}
