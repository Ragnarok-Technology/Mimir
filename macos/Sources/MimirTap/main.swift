import Foundation
import MimirCore

@main
struct MimirTapCLI {
    static func main() async {
        let tap = SystemAudioTap(
            onPcmData: { data in
                FileHandle.standardOutput.write(data)
            },
            onError: { msg in
                if let errData = "mimir-sc-tap error: \(msg)\n".data(using: .utf8) {
                    FileHandle.standardError.write(errData)
                }
            }
        )

        do {
            try await tap.start()
        } catch {
            if let errData = "mimir-sc-tap failed to start: \(error.localizedDescription)\n".data(using: .utf8) {
                FileHandle.standardError.write(errData)
            }
            Foundation.exit(1)
        }

        // Stay alive until stdin closes or process receives SIGTERM/SIGINT.
        while FileHandle.standardInput.availableData.count > 0 {}
        await tap.stop()
    }
}
