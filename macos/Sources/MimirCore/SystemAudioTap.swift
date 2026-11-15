import AVFoundation
import CoreMedia
import Foundation
import ScreenCaptureKit

/// Driverless macOS 14+ system audio loopback using `ScreenCaptureKit` audio-only `SCStream`.
/// Outputs 16 kHz mono float32 PCM frames directly to a callback/pipe.
public final class SystemAudioTap: NSObject, SCStreamOutput, SCStreamDelegate, @unchecked Sendable {
    private var stream: SCStream?
    private let queue = DispatchQueue(label: "mimir.system.audio.tap")
    private let onPcmData: @Sendable (Data) -> Void
    private let onError: @Sendable (String) -> Void

    public init(
        onPcmData: @escaping @Sendable (Data) -> Void,
        onError: @escaping @Sendable (String) -> Void
    ) {
        self.onPcmData = onPcmData
        self.onError = onError
    }

    public func start() async throws {
        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: false)
        guard let display = content.displays.first else {
            throw NSError(domain: "MimirAudioTap", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "No display found for ScreenCaptureKit audio tap"
            ])
        }

        let filter = SCContentFilter(display: display, excludingWindows: [])
        let config = SCStreamConfiguration()
        config.capturesAudio = true
        config.excludesCurrentProcessAudio = true
        config.sampleRate = 16_000
        config.channelCount = 1
        // Minimal video footprint since ScreenCaptureKit requires non-zero dimensions on some OS builds
        config.width = 2
        config.height = 2
        config.minimumFrameInterval = CMTime(value: 1, timescale: 1)

        let stream = SCStream(filter: filter, configuration: config, delegate: self)
        try stream.addStreamOutput(self, type: .audio, sampleHandlerQueue: queue)
        try await stream.startCapture()
        self.stream = stream
    }

    public func stop() async {
        guard let stream else { return }
        self.stream = nil
        try? await stream.stopCapture()
    }

    public func stream(
        _ stream: SCStream,
        didOutputSampleBuffer sampleBuffer: CMSampleBuffer,
        of outputType: SCStreamOutputType
    ) {
        guard outputType == .audio, CMSampleBufferDataIsReady(sampleBuffer) else { return }
        guard let blockBuffer = CMSampleBufferGetDataBuffer(sampleBuffer) else { return }

        var length = 0
        var dataPtr: UnsafeMutablePointer<Int8>?
        let status = CMBlockBufferGetDataPointer(
            blockBuffer,
            atOffset: 0,
            lengthAtOffsetOut: nil,
            totalLengthOut: &length,
            dataPointerOut: &dataPtr
        )
        guard status == kCMBlockBufferNoErr, let dataPtr, length > 0 else { return }

        let data = Data(bytes: dataPtr, count: length)
        onPcmData(data)
    }

    public func stream(_ stream: SCStream, didStopWithError error: Error) {
        onError(error.localizedDescription)
    }
}
