import AVFoundation
import CoreGraphics
import CoreMedia
import Darwin
import Foundation
import ImageIO
import ScreenCaptureKit

signal(SIGPIPE, SIG_IGN)

struct BridgeStatus: Encodable {
    let type = "status"
    let status: String
    let message: String
    let timestampMs: UInt64
}

struct CaptureSource: Encodable {
    let id: String
    let name: String
    let bundleId: String?
}

struct SourceList: Encodable {
    let type = "sources"
    let microphones: [CaptureSource]
    let applications: [CaptureSource]
    let applicationDiscoveryError: SourceDiscoveryError?
}

struct SourceDiscoveryError: Encodable {
    let code: String
    let message: String
}

struct ScreenPermissionResult: Encodable {
    let type = "screenPermission"
    let granted: Bool
    let message: String
}

struct StartRequest: Decodable {
    let command: String
    let microphoneId: String
    let applicationId: String
}

private enum MicrophonePermissionState: String, CaseIterable {
    case authorized
    case notDetermined = "not_determined"
    case denied
    case restricted
}

private enum MicrophonePermissionAction: Equatable {
    case proceed
    case request
    case fail(CaptureError)
}

private func microphonePermissionState(for status: AVAuthorizationStatus) -> MicrophonePermissionState {
    switch status {
    case .authorized:
        return .authorized
    case .notDetermined:
        return .notDetermined
    case .denied:
        return .denied
    case .restricted:
        return .restricted
    @unknown default:
        return .restricted
    }
}

private func microphonePermissionAction(for state: MicrophonePermissionState) -> MicrophonePermissionAction {
    switch state {
    case .authorized:
        return .proceed
    case .notDetermined:
        return .request
    case .denied:
        return .fail(.microphonePermissionDenied)
    case .restricted:
        return .fail(.microphonePermissionRestricted)
    }
}

private func requestMicrophoneAccess() async -> Bool {
    await withCheckedContinuation { continuation in
        AVCaptureDevice.requestAccess(for: .audio) { granted in
            continuation.resume(returning: granted)
        }
    }
}

private func ensureMicrophoneAccessForExplicitStart() async throws {
    let initialState = microphonePermissionState(for: AVCaptureDevice.authorizationStatus(for: .audio))
    switch microphonePermissionAction(for: initialState) {
    case .proceed:
        return
    case let .fail(error):
        throw error
    case .request:
        guard await requestMicrophoneAccess() else {
            let resolvedState = microphonePermissionState(for: AVCaptureDevice.authorizationStatus(for: .audio))
            if resolvedState == .restricted {
                throw CaptureError.microphonePermissionRestricted
            }
            throw CaptureError.microphonePermissionDenied
        }
    }
}

enum AudioSource: String, Codable, CaseIterable {
    case candidate
    case interviewer
}

struct NativeEvent: Codable {
    let type: String
    let source: String?
    let sampleRate: Double?
    let channels: Int?
    let capturedAtMs: UInt64?
    let samples: String?
    let code: String?
    let message: String?

    static func state(_ message: String) -> NativeEvent {
        NativeEvent(type: "state", source: nil, sampleRate: nil, channels: nil, capturedAtMs: nil, samples: nil, code: nil, message: message)
    }

    static func error(_ code: String, _ message: String, source: AudioSource? = nil) -> NativeEvent {
        NativeEvent(type: "error", source: source?.rawValue, sampleRate: nil, channels: nil, capturedAtMs: nil, samples: nil, code: code, message: message)
    }

    static func warning(_ code: String, _ message: String, source: AudioSource) -> NativeEvent {
        NativeEvent(type: "warning", source: source.rawValue, sampleRate: nil, channels: nil, capturedAtMs: nil, samples: nil, code: code, message: message)
    }

    static func pcm(source: AudioSource, sampleRate: Double, channels: Int, capturedAtMs: UInt64, samples: Data) -> NativeEvent {
        NativeEvent(
            type: "pcm",
            source: source.rawValue,
            sampleRate: sampleRate,
            channels: channels,
            capturedAtMs: capturedAtMs,
            samples: samples.base64EncodedString(),
            code: nil,
            message: nil
        )
    }
}

@discardableResult
private func writeJSON<T: Encodable>(_ value: T, fileDescriptor: Int32 = STDOUT_FILENO) -> Bool {
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.sortedKeys]
    guard var payload = try? encoder.encode(value) else { return false }
    payload.append(0x0A)
    return payload.withUnsafeBytes { rawBuffer in
        guard let baseAddress = rawBuffer.baseAddress else { return false }
        var offset = 0
        while offset < rawBuffer.count {
            let written = Darwin.write(fileDescriptor, baseAddress.advanced(by: offset), rawBuffer.count - offset)
            if written > 0 {
                offset += written
            } else if written == -1 && errno == EINTR {
                continue
            } else {
                return false
            }
        }
        return true
    }
}

private let maximumPCMChannels = 32
private let maximumPCMFrames = 48_000
private let maximumPCMBufferListBytes =
    MemoryLayout<AudioBufferList>.size + (maximumPCMChannels - 1) * MemoryLayout<AudioBuffer>.size

private func timestampMs() -> UInt64 {
    UInt64(Date().timeIntervalSince1970 * 1000)
}

private func source(for outputType: SCStreamOutputType) -> AudioSource? {
    switch outputType {
    case .microphone:
        return .candidate
    case .audio:
        return .interviewer
    default:
        return nil
    }
}

private struct FrameMetadata {
    let source: AudioSource
    let capturedAtMs: UInt64
    let durationMs: Double
}

private enum FrameObservation {
    case accept
    case warning(NativeEvent)
    case stale
}

private final class SourceFrameTracker: @unchecked Sendable {
    private let lock = NSLock()
    private var lastCapturedAtMs: [AudioSource: UInt64] = [:]

    func observe(_ frame: FrameMetadata) -> FrameObservation {
        lock.lock()
        defer { lock.unlock() }

        guard let previous = lastCapturedAtMs[frame.source] else {
            lastCapturedAtMs[frame.source] = frame.capturedAtMs
            return .accept
        }
        guard frame.capturedAtMs >= previous else {
            return .stale
        }

        lastCapturedAtMs[frame.source] = frame.capturedAtMs
        let expectedNext = Double(previous) + frame.durationMs
        if Double(frame.capturedAtMs) > expectedNext + 250 {
            return .warning(.warning(
                "audio_gap",
                "A gap was observed in \(frame.source.rawValue) audio callbacks; subsequent audio remains source-tagged and transient.",
                source: frame.source
            ))
        }
        return .accept
    }
}

private final class CaptureLifecycle: @unchecked Sendable {
    private let lock = NSLock()
    private var stream: SCStream?
    private var stopping = false
    private var terminalEventEmitted = false

    func register(_ stream: SCStream) {
        lock.withLock {
            self.stream = stream
        }
    }

    enum StopDecision {
        case alreadyStopping
        case stop(SCStream?)
    }

    func beginStop() -> StopDecision {
        lock.withLock {
            guard !stopping else { return .alreadyStopping }
            stopping = true
            let ownedStream = stream
            stream = nil
            return .stop(ownedStream)
        }
    }

    func noteUnexpectedStop() -> Bool {
        lock.withLock {
            stream = nil
            return !stopping
        }
    }

    func claimTerminalEvent() -> Bool {
        lock.withLock {
            guard !terminalEventEmitted else { return false }
            terminalEventEmitted = true
            return true
        }
    }
}

@available(macOS 13.0, *)
private final class CaptureDelegate: NSObject, SCStreamOutput, SCStreamDelegate, @unchecked Sendable {
    private let candidateQueue = DispatchQueue(label: "intervjuare.native.candidate")
    private let interviewerQueue = DispatchQueue(label: "intervjuare.native.interviewer")
    private let writeQueue = DispatchQueue(label: "intervjuare.native.stdout")
    private let lifecycle = CaptureLifecycle()
    private let frameTracker = SourceFrameTracker()

    func start(request: StartRequest) async throws {
        try await ensureMicrophoneAccessForExplicitStart()

        let content: SCShareableContent
        do {
            content = try await SCShareableContent.excludingDesktopWindows(true, onScreenWindowsOnly: false)
        } catch {
            throw CaptureError.screenPermission(error.localizedDescription)
        }

        guard let application = content.applications.first(where: { String($0.processID) == request.applicationId }) else {
            throw CaptureError.applicationUnavailable
        }
        guard let microphone = microphoneDevices().first(where: { $0.uniqueID == request.microphoneId }) else {
            throw CaptureError.microphoneUnavailable
        }
        guard let display = content.displays.first else {
            throw CaptureError.captureStart("No display is available for application-scoped ScreenCaptureKit audio capture.")
        }

        let filter = SCContentFilter(display: display, including: [application], exceptingWindows: [])
        let configuration = SCStreamConfiguration()
        configuration.capturesAudio = true
        configuration.captureMicrophone = true
        configuration.microphoneCaptureDeviceID = microphone.uniqueID
        configuration.excludesCurrentProcessAudio = true
        configuration.sampleRate = 16_000
        configuration.channelCount = 1
        configuration.queueDepth = 3

        let stream = SCStream(filter: filter, configuration: configuration, delegate: self)
        do {
            try stream.addStreamOutput(self, type: .audio, sampleHandlerQueue: interviewerQueue)
            try stream.addStreamOutput(self, type: .microphone, sampleHandlerQueue: candidateQueue)
            try await stream.startCapture()
        } catch {
            try? await stream.stopCapture()
            throw CaptureError.captureStart(error.localizedDescription)
        }

        lifecycle.register(stream)
        emit(.state("capturing"))
    }

    func stop() async {
        guard case let .stop(ownedStream) = lifecycle.beginStop() else { return }
        if let ownedStream {
            try? await ownedStream.stopCapture()
        }
        emitTerminal(.state("stopped"), exitCode: 0)
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        guard lifecycle.noteUnexpectedStop() else { return }

        emitTerminal(.error(
            "capture_failed",
            "Native capture stopped unexpectedly. Refresh sources and start again. (\(error.localizedDescription))"
        ), exitCode: 3)
    }

    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of outputType: SCStreamOutputType) {
        guard let audioSource = source(for: outputType) else { return }
        guard sampleBuffer.isValid, CMSampleBufferDataIsReady(sampleBuffer) else {
            emit(.warning("audio_gap", "A \(audioSource.rawValue) audio buffer was invalid or not ready and was dropped.", source: audioSource))
            return
        }
        guard let format = CMSampleBufferGetFormatDescription(sampleBuffer),
              let description = CMAudioFormatDescriptionGetStreamBasicDescription(format)
        else {
            emit(.warning("audio_gap", "A \(audioSource.rawValue) audio buffer had no usable format and was dropped.", source: audioSource))
            return
        }

        let sampleRate = description.pointee.mSampleRate
        let channels = Int(description.pointee.mChannelsPerFrame)
        guard sampleRate > 0, channels > 0 else {
            emit(.warning("audio_gap", "A \(audioSource.rawValue) audio buffer reported an invalid audio format and was dropped.", source: audioSource))
            return
        }

        let capturedAt = timestampMs()
        let durationMs = Double(CMSampleBufferGetNumSamples(sampleBuffer)) / sampleRate * 1_000
        switch frameTracker.observe(FrameMetadata(source: audioSource, capturedAtMs: capturedAt, durationMs: durationMs)) {
        case .stale:
            emit(.warning("audio_gap", "A \(audioSource.rawValue) callback timestamp regressed and the frame was dropped.", source: audioSource))
            return
        case let .warning(event):
            emit(event)
        case .accept:
            break
        }

        do {
            let bytes = try copyAudioBytes(from: sampleBuffer, description: description.pointee)
            emit(.pcm(source: audioSource, sampleRate: sampleRate, channels: 1, capturedAtMs: capturedAt, samples: bytes))
        } catch {
            emit(.warning("audio_gap", "A \(audioSource.rawValue) audio frame could not be copied and was dropped. (\(error.localizedDescription))", source: audioSource))
        }
    }

    private func emit(_ event: NativeEvent) {
        writeQueue.async { writeJSON(event) }
    }

    private func emitTerminal(_ event: NativeEvent, exitCode: Int32) {
        guard lifecycle.claimTerminalEvent() else { return }

        writeQueue.async {
            writeJSON(event)
            exit(exitCode)
        }
    }
}

private enum AudioCopyError: LocalizedError {
    case bufferList(OSStatus)
    case empty
    case unsupportedFormat
    case invalidLayout

    var errorDescription: String? {
        switch self {
        case let .bufferList(status):
            return "CoreMedia could not expose PCM bytes (status \(status))."
        case .empty:
            return "The PCM buffer contained no bytes."
        case .unsupportedFormat:
            return "The PCM buffer used an unsupported audio format."
        case .invalidLayout:
            return "The PCM buffer layout did not match its audio format."
        }
    }
}

private func normalizedFloat32Mono(
    buffers: UnsafeMutableAudioBufferListPointer,
    description: AudioStreamBasicDescription,
    frameCount: Int
) throws -> Data {
    guard description.mFormatID == kAudioFormatLinearPCM,
          description.mBitsPerChannel == 32,
          description.mBytesPerFrame > 0,
          frameCount > 0
    else { throw AudioCopyError.unsupportedFormat }

    let flags = description.mFormatFlags
    let isFloat = flags & kAudioFormatFlagIsFloat != 0
    let isBigEndian = flags & kAudioFormatFlagIsBigEndian != 0
    let isNonInterleaved = flags & kAudioFormatFlagIsNonInterleaved != 0
    let channels = Int(description.mChannelsPerFrame)
    guard isFloat, !isBigEndian,
          channels > 0, channels <= maximumPCMChannels,
          frameCount <= maximumPCMFrames
    else { throw AudioCopyError.unsupportedFormat }

    var mono = [Float](repeating: 0, count: frameCount)
    if isNonInterleaved {
        guard buffers.count >= channels else { throw AudioCopyError.invalidLayout }
        let requiredBytes = frameCount.multipliedReportingOverflow(by: MemoryLayout<Float>.size)
        guard !requiredBytes.overflow else { throw AudioCopyError.invalidLayout }
        for channel in 0..<channels {
            let buffer = buffers[channel]
            guard Int(buffer.mDataByteSize) >= requiredBytes.partialValue, let data = buffer.mData else {
                throw AudioCopyError.invalidLayout
            }
            let samples = data.assumingMemoryBound(to: Float.self)
            for frame in 0..<frameCount {
                mono[frame] += samples[frame] / Float(channels)
            }
        }
    } else {
        guard let buffer = buffers.first else { throw AudioCopyError.empty }
        let bytesPerFrame = Int(description.mBytesPerFrame)
        let stride = bytesPerFrame / MemoryLayout<Float>.size
        let requiredBytes = frameCount.multipliedReportingOverflow(by: bytesPerFrame)
        guard bytesPerFrame.isMultiple(of: MemoryLayout<Float>.size),
              stride >= channels, !requiredBytes.overflow,
              Int(buffer.mDataByteSize) >= requiredBytes.partialValue, let data = buffer.mData else {
            throw AudioCopyError.invalidLayout
        }
        let samples = data.assumingMemoryBound(to: Float.self)
        for frame in 0..<frameCount {
            let base = frame * stride
            for channel in 0..<channels {
                mono[frame] += samples[base + channel] / Float(channels)
            }
        }
    }
    guard mono.allSatisfy(\.isFinite) else { throw AudioCopyError.invalidLayout }
    return mono.withUnsafeBytes { Data($0) }
}

private func copyAudioBytes(
    from sampleBuffer: CMSampleBuffer,
    description: AudioStreamBasicDescription
) throws -> Data {
    var sizeNeeded = 0
    var blockBuffer: CMBlockBuffer?
    var status = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
        sampleBuffer,
        bufferListSizeNeededOut: &sizeNeeded,
        bufferListOut: nil,
        bufferListSize: 0,
        blockBufferAllocator: nil,
        blockBufferMemoryAllocator: nil,
        flags: 0,
        blockBufferOut: &blockBuffer
    )
    guard status == noErr, sizeNeeded > 0 else { throw AudioCopyError.bufferList(status) }
    guard sizeNeeded <= maximumPCMBufferListBytes else { throw AudioCopyError.invalidLayout }

    let storage = UnsafeMutableRawPointer.allocate(byteCount: sizeNeeded, alignment: MemoryLayout<AudioBufferList>.alignment)
    defer { storage.deallocate() }
    let bufferList = storage.bindMemory(to: AudioBufferList.self, capacity: 1)
    status = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
        sampleBuffer,
        bufferListSizeNeededOut: nil,
        bufferListOut: bufferList,
        bufferListSize: sizeNeeded,
        blockBufferAllocator: nil,
        blockBufferMemoryAllocator: nil,
        flags: 0,
        blockBufferOut: &blockBuffer
    )
    guard status == noErr else { throw AudioCopyError.bufferList(status) }

    let buffers = UnsafeMutableAudioBufferListPointer(bufferList)
    return try normalizedFloat32Mono(
        buffers: buffers,
        description: description,
        frameCount: CMSampleBufferGetNumSamples(sampleBuffer)
    )
}

private enum CaptureError: LocalizedError, Equatable {
    case microphonePermissionDenied
    case microphonePermissionRestricted
    case screenPermission(String)
    case applicationUnavailable
    case microphoneUnavailable
    case captureStart(String)

    var errorDescription: String? {
        switch self {
        case .microphonePermissionDenied:
            return "Microphone permission was denied. Enable Intervjuare in System Settings → Privacy & Security → Microphone, then restart the app."
        case .microphonePermissionRestricted:
            return "Microphone access is restricted by macOS policy and cannot be requested by Intervjuare."
        case let .screenPermission(detail):
            return "Screen & System Audio Recording permission is required. Enable Intervjuare in System Settings → Privacy & Security → Screen & System Audio Recording, then restart the app. (\(detail))"
        case .applicationUnavailable:
            return "The selected meeting application is no longer running. Refresh sources and select it again."
        case .microphoneUnavailable:
            return "The selected microphone is unavailable. Refresh sources and select another device."
        case let .captureStart(detail):
            return "Native audio capture could not start. Refresh both selected sources and try again. (\(detail))"
        }
    }

    var code: String {
        switch self {
        case .microphonePermissionDenied: "microphone_permission_denied"
        case .microphonePermissionRestricted: "microphone_permission_restricted"
        case .screenPermission: "screen_audio_permission_denied"
        case .applicationUnavailable: "application_unavailable"
        case .microphoneUnavailable: "microphone_unavailable"
        case .captureStart: "capture_start_failed"
        }
    }
}

private func microphoneDevices() -> [AVCaptureDevice] {
    AVCaptureDevice.DiscoverySession(
        deviceTypes: [.microphone, .external],
        mediaType: .audio,
        position: .unspecified
    ).devices
}

// Passive, non-requesting ScreenCaptureKit preflight for Refresh Sources.
// The preflight call below never prompts; the requesting screen-capture
// variant and SCShareableContent enumeration (a prompting API when permission
// is missing) must not run on the passive path.
private func screenCapturePreflightGranted() -> Bool {
    CGPreflightScreenCaptureAccess()
}

// Explicit owner-initiated screen-capture permission request. This is the ONLY
// path that may call the prompting screen-capture request API, and it
// runs solely after the dedicated UI button invokes the `request-screen-access`
// helper command. It never starts recording and never enumerates sources.
private func requestScreenCaptureAccessForExplicitAction() -> Bool {
    CGRequestScreenCaptureAccess()
}

private func requestScreenAccess() {
    let granted = requestScreenCaptureAccessForExplicitAction()
    if granted {
        writeJSON(ScreenPermissionResult(
            granted: true,
            message: "Screen & System Audio Recording permission is granted for Intervjuare. It may not take effect until the app is restarted."
        ))
    } else {
        writeJSON(ScreenPermissionResult(
            granted: false,
            message: "Screen & System Audio Recording permission was not granted for Intervjuare (denied or not yet effective)."
        ))
    }
}

private func status() {
    writeJSON(BridgeStatus(status: "ready", message: "Swift macOS bridge is ready for ScreenCaptureKit audio.", timestampMs: timestampMs()))
}

@available(macOS 13.0, *)
private func listSources() async {
    let microphones = microphoneDevices().map {
        CaptureSource(id: $0.uniqueID, name: $0.localizedName, bundleId: nil)
    }
    // Passive Refresh Sources: never prompt here. Microphone enumeration
    // above is independent and always returned. ScreenCaptureKit content
    // enumeration can prompt when permission is missing, so gate it behind
    // the non-requesting preflight. Permission requests happen only on the
    // explicit Start path (ensureMicrophoneAccessForExplicitStart + capture).
    guard screenCapturePreflightGranted() else {
        writeJSON(SourceList(
            microphones: microphones,
            applications: [],
            applicationDiscoveryError: SourceDiscoveryError(
                code: "screen_audio_permission_denied",
                message: "Microphones are available, but meeting applications were not listed because Screen & System Audio Recording permission is not granted for Intervjuare (passive check; no permission prompt was requested). Grant it in System Settings → Privacy & Security → Screen & System Audio Recording, then use Refresh sources again."
            )
        ))
        return
    }
    do {
        let content = try await SCShareableContent.excludingDesktopWindows(true, onScreenWindowsOnly: false)
        let ownPid = ProcessInfo.processInfo.processIdentifier
        let applications = content.applications
            .filter { $0.processID != ownPid && !$0.applicationName.isEmpty }
            .map { CaptureSource(id: String($0.processID), name: $0.applicationName, bundleId: $0.bundleIdentifier) }
            .sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
        writeJSON(SourceList(
            microphones: microphones,
            applications: applications,
            applicationDiscoveryError: nil
        ))
    } catch {
        writeJSON(SourceList(
            microphones: microphones,
            applications: [],
            applicationDiscoveryError: SourceDiscoveryError(
                code: "screen_audio_permission_denied",
                message: "Microphones are available, but meeting applications could not be listed. Screen & System Audio Recording permission is unavailable for Intervjuare. (\(error.localizedDescription))"
            )
        ))
    }
}

private func sourceListHarnessPayload(screenDiscoveryAvailable: Bool) -> SourceList {
    let microphones = [CaptureSource(id: "test-microphone", name: "Test Microphone", bundleId: nil)]
    if screenDiscoveryAvailable {
        return SourceList(
            microphones: microphones,
            applications: [CaptureSource(id: "42", name: "Test Meeting App", bundleId: "com.example.meeting")],
            applicationDiscoveryError: nil
        )
    }
    return SourceList(
        microphones: microphones,
        applications: [],
        applicationDiscoveryError: SourceDiscoveryError(
            code: "screen_audio_permission_denied",
            message: "Meeting applications are unavailable in the synthetic permission fixture."
        )
    )
}

@available(macOS 13.0, *)
private func runCapture() async {
    guard let line = readLine(), let data = line.data(using: .utf8), let request = try? JSONDecoder().decode(StartRequest.self, from: data), request.command == "start" else {
        writeJSON(NativeEvent.error("invalid_request", "Expected one JSON start request on stdin."))
        exit(2)
    }

    let delegate = CaptureDelegate()
    do {
        try await delegate.start(request: request)
    } catch let error as CaptureError {
        writeJSON(NativeEvent.error(error.code, error.localizedDescription))
        exit(3)
    } catch {
        writeJSON(NativeEvent.error("capture_start_failed", error.localizedDescription))
        exit(3)
    }

    DispatchQueue.global().async {
        while let line = readLine() {
            if line == "stop" {
                Task { await delegate.stop() }
                return
            }
        }
        Task { await delegate.stop() }
    }
    await withCheckedContinuation { (_: CheckedContinuation<Void, Never>) in }
}

private func runSyntheticHarness() -> Int32 {
    // Passive preflight linkage check: CGPreflightScreenCaptureAccess() is the
    // non-requesting API used by Refresh Sources. Its granted/denied result
    // depends on machine TCC state, so the harness only proves the call links
    // and returns without prompting, requesting, or crashing.
    _ = screenCapturePreflightGranted()
    guard microphonePermissionState(for: .authorized) == .authorized,
          microphonePermissionState(for: .notDetermined) == .notDetermined,
          microphonePermissionState(for: .denied) == .denied,
          microphonePermissionState(for: .restricted) == .restricted,
          Set(MicrophonePermissionState.allCases.map(\.rawValue)) == Set([
              "authorized", "not_determined", "denied", "restricted",
          ])
    else {
        FileHandle.standardError.write(Data("synthetic harness: microphone permission state mapping failed\n".utf8))
        return 1
    }

    guard microphonePermissionAction(for: .authorized) == .proceed,
          microphonePermissionAction(for: .notDetermined) == .request,
          microphonePermissionAction(for: .denied) == .fail(.microphonePermissionDenied),
          microphonePermissionAction(for: .restricted) == .fail(.microphonePermissionRestricted)
    else {
        FileHandle.standardError.write(Data("synthetic harness: microphone permission action failed\n".utf8))
        return 1
    }

    func interleavedMono(_ samples: [Float], sampleRate: Double) -> [Float]? {
        let sampleData = samples.withUnsafeBytes { Data($0) }
        let result: Data? = sampleData.withUnsafeBytes { rawBuffer in
            guard let baseAddress = rawBuffer.baseAddress else { return nil }
            let audioBuffer = AudioBuffer(
                mNumberChannels: 2,
                mDataByteSize: UInt32(rawBuffer.count),
                mData: UnsafeMutableRawPointer(mutating: baseAddress)
            )
            var list = AudioBufferList(mNumberBuffers: 1, mBuffers: audioBuffer)
            return withUnsafeMutablePointer(to: &list) { listPointer in
                let buffers = UnsafeMutableAudioBufferListPointer(listPointer)
                var format = AudioStreamBasicDescription()
                format.mFormatID = kAudioFormatLinearPCM
                format.mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked
                format.mSampleRate = sampleRate
                format.mChannelsPerFrame = 2
                format.mBitsPerChannel = 32
                format.mBytesPerFrame = 8
                format.mBytesPerPacket = 8
                format.mFramesPerPacket = 1
                return try? normalizedFloat32Mono(buffers: buffers, description: format, frameCount: 2)
            }
        }
        return result?.withUnsafeBytes { Array($0.bindMemory(to: Float.self)) }
    }

    func planarMono(_ left: [Float], _ right: [Float], sampleRate: Double) -> [Float]? {
        guard left.count == right.count else { return nil }
        return left.withUnsafeBytes { leftRaw in
            right.withUnsafeBytes { rightRaw in
                guard let leftBase = leftRaw.baseAddress, let rightBase = rightRaw.baseAddress else { return nil }
                let listSize = MemoryLayout<AudioBufferList>.size + MemoryLayout<AudioBuffer>.size
                let storage = UnsafeMutableRawPointer.allocate(
                    byteCount: listSize,
                    alignment: MemoryLayout<AudioBufferList>.alignment
                )
                defer { storage.deallocate() }
                let list = storage.bindMemory(to: AudioBufferList.self, capacity: 1)
                list.pointee.mNumberBuffers = 2
                let buffers = UnsafeMutableAudioBufferListPointer(list)
                buffers[0] = AudioBuffer(
                    mNumberChannels: 1,
                    mDataByteSize: UInt32(leftRaw.count),
                    mData: UnsafeMutableRawPointer(mutating: leftBase)
                )
                buffers[1] = AudioBuffer(
                    mNumberChannels: 1,
                    mDataByteSize: UInt32(rightRaw.count),
                    mData: UnsafeMutableRawPointer(mutating: rightBase)
                )
                var format = AudioStreamBasicDescription()
                format.mFormatID = kAudioFormatLinearPCM
                format.mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked | kAudioFormatFlagIsNonInterleaved
                format.mSampleRate = sampleRate
                format.mChannelsPerFrame = 2
                format.mBitsPerChannel = 32
                format.mBytesPerFrame = 4
                format.mBytesPerPacket = 4
                format.mFramesPerPacket = 1
                guard let result = try? normalizedFloat32Mono(
                    buffers: buffers,
                    description: format,
                    frameCount: left.count
                ) else { return nil }
                return result.withUnsafeBytes { Array($0.bindMemory(to: Float.self)) }
            }
        }
    }

    let stereoSamples: [Float] = [1, 3, 2, 4]
    guard [16_000.0, 24_000.0, 48_000.0].allSatisfy({ rate in
        interleavedMono(stereoSamples, sampleRate: rate) == [2, 3]
            && planarMono([1, 2], [3, 4], sampleRate: rate) == [2, 3]
    }) else {
        FileHandle.standardError.write(Data("synthetic harness: supported-rate PCM normalization failed\n".utf8))
        return 1
    }

    let nonFiniteResult: Data? = [Float.nan].withUnsafeBytes { rawBuffer in
        guard let baseAddress = rawBuffer.baseAddress else { return nil }
        let audioBuffer = AudioBuffer(
            mNumberChannels: 1,
            mDataByteSize: UInt32(rawBuffer.count),
            mData: UnsafeMutableRawPointer(mutating: baseAddress)
        )
        var list = AudioBufferList(mNumberBuffers: 1, mBuffers: audioBuffer)
        return withUnsafeMutablePointer(to: &list) { listPointer in
            let buffers = UnsafeMutableAudioBufferListPointer(listPointer)
            var format = AudioStreamBasicDescription()
            format.mFormatID = kAudioFormatLinearPCM
            format.mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked
            format.mSampleRate = 16_000
            format.mChannelsPerFrame = 1
            format.mBitsPerChannel = 32
            format.mBytesPerFrame = 4
            format.mBytesPerPacket = 4
            format.mFramesPerPacket = 1
            return try? normalizedFloat32Mono(buffers: buffers, description: format, frameCount: 1)
        }
    }
    guard nonFiniteResult == nil else {
        FileHandle.standardError.write(Data("synthetic harness: non-finite PCM rejection failed\n".utf8))
        return 1
    }

    let availableSources = sourceListHarnessPayload(screenDiscoveryAvailable: true)
    let partialSources = sourceListHarnessPayload(screenDiscoveryAvailable: false)
    guard availableSources.microphones.count == 1,
          availableSources.applications.count == 1,
          availableSources.applicationDiscoveryError == nil,
          partialSources.microphones.count == 1,
          partialSources.applications.isEmpty,
          partialSources.applicationDiscoveryError?.code == "screen_audio_permission_denied"
    else {
        FileHandle.standardError.write(Data("synthetic harness: partial source discovery failed\n".utf8))
        return 1
    }

    let candidate = source(for: .microphone)
    let interviewer = source(for: .audio)
    guard candidate == .candidate, interviewer == .interviewer, candidate != interviewer else {
        FileHandle.standardError.write(Data("synthetic harness: source assignment failed\n".utf8))
        return 1
    }

    let tracker = SourceFrameTracker()
    guard case .accept = tracker.observe(FrameMetadata(source: .candidate, capturedAtMs: 1_000, durationMs: 20)),
          case .accept = tracker.observe(FrameMetadata(source: .interviewer, capturedAtMs: 1_000, durationMs: 20)),
          case .stale = tracker.observe(FrameMetadata(source: .candidate, capturedAtMs: 999, durationMs: 20)),
          case .warning = tracker.observe(FrameMetadata(source: .interviewer, capturedAtMs: 1_500, durationMs: 20))
    else {
        FileHandle.standardError.write(Data("synthetic harness: independent timing/gap handling failed\n".utf8))
        return 1
    }

    let encoder = JSONEncoder()
    guard let candidateJSON = try? encoder.encode(NativeEvent.pcm(source: .candidate, sampleRate: 16_000, channels: 1, capturedAtMs: 1_000, samples: Data([0, 0, 0, 0]))),
          let interviewerJSON = try? encoder.encode(NativeEvent.pcm(source: .interviewer, sampleRate: 16_000, channels: 1, capturedAtMs: 1_001, samples: Data([1, 0, 0, 0]))),
          let candidateText = String(data: candidateJSON, encoding: .utf8),
          let interviewerText = String(data: interviewerJSON, encoding: .utf8),
          candidateText.contains("candidate"), !candidateText.contains("interviewer"),
          interviewerText.contains("interviewer"), !interviewerText.contains("candidate")
    else {
        FileHandle.standardError.write(Data("synthetic harness: event isolation failed\n".utf8))
        return 1
    }

    let lifecycle = CaptureLifecycle()
    guard case let .stop(stream) = lifecycle.beginStop(), stream == nil,
          case .alreadyStopping = lifecycle.beginStop(),
          lifecycle.claimTerminalEvent(), !lifecycle.claimTerminalEvent()
    else {
        FileHandle.standardError.write(Data("synthetic harness: lifecycle idempotence failed\n".utf8))
        return 1
    }

    writeJSON(NativeEvent.state("synthetic_harness_passed"))
    return 0
}

struct ScreenshotResult: Encodable {
    let type: String
    let mime: String
    let width: Int
    let height: Int
    let data: String
}

private let screenshotMaxDimension = 1600.0

/// One still image for "Låt oss se". Captures only the selected meeting
/// application's windows when a process id is given, otherwise the main
/// display without Intervjuare's own windows. The JPEG goes to stdout only;
/// nothing is written to disk.
@available(macOS 14.0, *)
private func captureScreenshot(applicationPid: pid_t?) async -> Int32 {
    guard screenCapturePreflightGranted() else {
        writeJSON(NativeEvent.error("screen_permission_denied", "Screen & System Audio Recording permission is not granted for Intervjuare, so the screen cannot be read. Grant it in System Settings → Privacy & Security → Screen & System Audio Recording."))
        return 4
    }
    do {
        let content = try await SCShareableContent.excludingDesktopWindows(true, onScreenWindowsOnly: true)
        let meetingApp = applicationPid.flatMap { pid in content.applications.first { $0.processID == pid } }
        let meetingWindows = content.windows.filter {
            $0.owningApplication?.processID == meetingApp?.processID && $0.isOnScreen && $0.frame.width > 200 && $0.frame.height > 150
        }
        let largestWindow = meetingWindows.max { $0.frame.width * $0.frame.height < $1.frame.width * $1.frame.height }
        let display = largestWindow.flatMap { window in content.displays.first { $0.frame.intersects(window.frame) } }
            ?? content.displays.first { $0.displayID == CGMainDisplayID() }
            ?? content.displays.first
        guard let display else {
            writeJSON(NativeEvent.error("screen_capture_failed", "No display is available to capture."))
            return 5
        }
        let filter: SCContentFilter
        if let meetingApp, largestWindow != nil {
            filter = SCContentFilter(display: display, including: [meetingApp], exceptingWindows: [])
        } else {
            let ownProcesses: Set<pid_t> = [ProcessInfo.processInfo.processIdentifier, getppid()]
            let excluded = content.applications.filter { ownProcesses.contains($0.processID) }
            filter = SCContentFilter(display: display, excludingApplications: excluded, exceptingWindows: [])
        }
        let configuration = SCStreamConfiguration()
        let pointScale = Double(filter.pointPixelScale)
        let pixelWidth = Double(display.width) * pointScale
        let pixelHeight = Double(display.height) * pointScale
        let scale = min(1.0, screenshotMaxDimension / max(pixelWidth, pixelHeight))
        configuration.width = max(1, Int(pixelWidth * scale))
        configuration.height = max(1, Int(pixelHeight * scale))
        configuration.showsCursor = false
        let image = try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: configuration)
        let buffer = NSMutableData()
        guard let destination = CGImageDestinationCreateWithData(buffer as CFMutableData, "public.jpeg" as CFString, 1, nil) else {
            writeJSON(NativeEvent.error("screen_capture_failed", "Could not encode the screenshot."))
            return 5
        }
        CGImageDestinationAddImage(destination, image, [kCGImageDestinationLossyCompressionQuality: 0.72] as CFDictionary)
        guard CGImageDestinationFinalize(destination) else {
            writeJSON(NativeEvent.error("screen_capture_failed", "Could not encode the screenshot."))
            return 5
        }
        writeJSON(ScreenshotResult(type: "screenshot", mime: "image/jpeg", width: image.width, height: image.height, data: (buffer as Data).base64EncodedString()))
        return 0
    } catch {
        writeJSON(NativeEvent.error("screen_capture_failed", "Screen capture failed: \(error.localizedDescription)"))
        return 5
    }
}

let command = CommandLine.arguments.dropFirst().first ?? ""
if command == "status" {
    status()
} else if command == "synthetic-harness" {
    exit(runSyntheticHarness())
} else if command == "request-screen-access" {
    requestScreenAccess()
} else if #available(macOS 13.0, *) {
    if command == "sources" {
        await listSources()
    } else if command == "capture" {
        await runCapture()
    } else if command == "screenshot" {
        guard #available(macOS 14.0, *) else {
            writeJSON(NativeEvent.error("unsupported_macos", "Screenshot assistance requires macOS 14 or newer."))
            exit(3)
        }
        let pidArgument = CommandLine.arguments.dropFirst(2).first.flatMap { pid_t($0) }
        exit(await captureScreenshot(applicationPid: pidArgument))
    } else {
        FileHandle.standardError.write(Data("unsupported command\n".utf8))
        exit(2)
    }
} else {
    writeJSON(NativeEvent.error("unsupported_macos", "Intervjuare audio capture requires macOS 13 or newer."))
    exit(3)
}