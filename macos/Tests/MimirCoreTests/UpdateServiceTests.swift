import Foundation
import MimirCore

@main
struct MimirCoreTestsMain {
    static func main() {
        // 1. SemanticVersion comparison
        assert(SemanticVersion("v0.2.0") > SemanticVersion("0.1.0"))
        assert(SemanticVersion("1.0.0") > SemanticVersion("0.9.9"))
        assert(SemanticVersion("v0.1.0") == SemanticVersion("0.1.0"))

        // 2. CLI session line parser
        let sample = """
        3d79c2c8-6bd1-479b-8480-86fc9c04c356 | 2026-11-15T07:32:27Z | 3 utterances | Standup
        07c28748-6adb-4669-94af-05fadeff065d | 2026-11-15T07:33:27Z | 0 utterances | Untitled
        """
        let parsed = MimirSessionSummary.parseCliLines(sample)
        assert(parsed.count == 2)
        assert(parsed[0].utteranceCount == 3)
        assert(parsed[0].title == "Standup")

        print("MimirCoreTests: all assertions passed.")
    }
}
