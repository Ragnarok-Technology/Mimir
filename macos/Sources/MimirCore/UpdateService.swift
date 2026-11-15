import AppKit
import Foundation

public struct SemanticVersion: Comparable, Sendable {
    public let major: Int
    public let minor: Int
    public let patch: Int
    public let raw: String

    public init(_ string: String) {
        self.raw = string
        let clean = string.trimmingCharacters(in: CharacterSet(charactersIn: "vV \t\n\r"))
        let parts = clean.split(separator: ".").compactMap { Int($0) }
        self.major = parts.indices.contains(0) ? parts[0] : 0
        self.minor = parts.indices.contains(1) ? parts[1] : 0
        self.patch = parts.indices.contains(2) ? parts[2] : 0
    }

    public static func < (lhs: SemanticVersion, rhs: SemanticVersion) -> Bool {
        if lhs.major != rhs.major { return lhs.major < rhs.major }
        if lhs.minor != rhs.minor { return lhs.minor < rhs.minor }
        return lhs.patch < rhs.patch
    }

    public static func == (lhs: SemanticVersion, rhs: SemanticVersion) -> Bool {
        lhs.major == rhs.major && lhs.minor == rhs.minor && lhs.patch == rhs.patch
    }
}

public struct GitHubReleaseInfo: Codable, Sendable {
    public let tagName: String
    public let name: String?
    public let body: String?
    public let htmlUrl: String
    public let assets: [GitHubAssetInfo]

    enum CodingKeys: String, CodingKey {
        case tagName = "tag_name"
        case name
        case body
        case htmlUrl = "html_url"
        case assets
    }
}

public struct GitHubAssetInfo: Codable, Sendable {
    public let name: String
    public let browserDownloadUrl: String
    public let size: Int

    enum CodingKeys: String, CodingKey {
        case name
        case browserDownloadUrl = "browser_download_url"
        case size
    }
}

public enum UpdateCheckResult: Sendable {
    case updateAvailable(version: String, releaseNotes: String?, downloadURL: URL, releasePageURL: URL)
    case upToDate(currentVersion: String)
    case error(String)
}

/// Checks GitHub Releases (`Ragnarok-Technology/Mimir`) using semantic versioning.
/// Never runs unless explicitly triggered or enabled by the user.
public final class UpdateService: Sendable {
    public static let shared = UpdateService()
    public let repo = "Ragnarok-Technology/Mimir"

    public var currentVersion: String {
        Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "0.1.0"
    }

    public init() {}

    public func checkForUpdates() async -> UpdateCheckResult {
        guard let url = URL(string: "https://api.github.com/repos/\(repo)/releases/latest") else {
            return .error("Invalid update API endpoint")
        }

        var request = URLRequest(url: url)
        request.setValue("application/vnd.github.v3+json", forHTTPHeaderField: "Accept")
        request.setValue("Mimir-Updater/\(currentVersion)", forHTTPHeaderField: "User-Agent")
        request.timeoutInterval = 10

        do {
            let (data, response) = try await URLSession.shared.data(for: request)
            guard let http = response as? HTTPURLResponse, (200...299).contains(http.statusCode) else {
                return .error("No published releases found yet")
            }

            let release = try JSONDecoder().decode(GitHubReleaseInfo.self, from: data)
            let latestSemVer = SemanticVersion(release.tagName)
            let currentSemVer = SemanticVersion(currentVersion)

            if latestSemVer > currentSemVer {
                guard let asset = release.assets.first(where: { $0.name == "Mimir.zip" })
                    ?? release.assets.first(where: { $0.name.hasSuffix(".dmg") }),
                    let downloadURL = URL(string: asset.browserDownloadUrl),
                    let pageURL = URL(string: release.htmlUrl)
                else {
                    return .error("No compatible release assets found")
                }

                return .updateAvailable(
                    version: release.tagName,
                    releaseNotes: release.body,
                    downloadURL: downloadURL,
                    releasePageURL: pageURL
                )
            } else {
                return .upToDate(currentVersion: currentVersion)
            }
        } catch {
            return .error(error.localizedDescription)
        }
    }
}
