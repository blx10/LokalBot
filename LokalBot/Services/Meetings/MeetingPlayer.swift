import Foundation
import AVFoundation

/// Only the audio bar and lightweight row highlights subscribe to timer ticks.
/// Keeping this separate avoids invalidating the entire meeting workspace.
@MainActor
final class MeetingPlaybackClock: ObservableObject {
    @Published private(set) var currentTime: TimeInterval = 0

    func update(to time: TimeInterval) {
        guard currentTime != time else { return }
        currentTime = time
    }
}

/// Plays a meeting's two source files (mic.m4a + system.m4a) together. They
/// were recorded simultaneously, so playing both from the same offset
/// reproduces the meeting; click-to-seek from the transcript lands here.
///
/// Each track gets its own `AVAudioPlayer` and the OS mixes them at the output
/// device. This is deliberate: the mic and system files routinely differ in
/// sample rate (device-native vs. the 48 kHz tap) and channel count (mono vs.
/// stereo mixdown). Folding both into one `AVMutableComposition` + `AVAudioMix`
/// and rendering through `AVPlayer` garbles the mismatched track during
/// real-time playback — independent players resample each cleanly. (Offline
/// export can still use the composition; see `MeetingAudioAsset`.)
@MainActor
final class MeetingPlayer: NSObject, ObservableObject {

    @Published private(set) var isPlaying = false
    let clock = MeetingPlaybackClock()
    private(set) var currentTime: TimeInterval {
        get { clock.currentTime }
        set { clock.update(to: newValue) }
    }
    @Published private(set) var duration: TimeInterval = 0
    @Published private(set) var windowStart: TimeInterval = 0
    @Published private(set) var windowEnd: TimeInterval = 0
    @Published private(set) var isLoaded = false
    /// Playback rate (1.0 = normal). `AVAudioPlayer` honors `enableRate` + `rate`
    /// and preserves pitch while the rate changes.
    @Published var speed: Float = 1.0 { didSet { applySpeed() } }

    private(set) var rawDuration: TimeInterval = 0
    private var players: [AVAudioPlayer] = []
    private var ticker: Timer?
    private var excerptEnd: TimeInterval?
    private var loadGeneration = 0

    /// Use exactly the tracks and gains that loaded successfully for playback.
    var waveformSources: [WaveformAnalysis.Source] {
        players.compactMap { player in
            player.url.map { WaveformAnalysis.Source(url: $0, gain: player.volume) }
        }
    }

    /// The longest source owns the meeting timeline. The system track can end
    /// before the mic (or vice versa); clocking from `players.first` freezes or
    /// jumps the scrubber as soon as that shorter player finishes.
    private var timelinePlayer: AVAudioPlayer? {
        players.max { $0.duration < $1.duration }
    }

    func load(folder: URL, hasSystemTrack: Bool, contentRange: Meeting.ContentRange? = nil) {
        stop()
        // Share the file + gain policy with the offline export so what you hear
        // matches what you export.
        players = MeetingAudioAsset.playbackSources(folder: folder, hasSystemTrack: hasSystemTrack)
            .compactMap { source -> AVAudioPlayer? in
                guard let player = try? AVAudioPlayer(contentsOf: source.url) else { return nil }
                player.delegate = self
                player.enableRate = true
                player.volume = source.gain
                player.prepareToPlay()
                return player
            }
        applySpeed()
        rawDuration = players.map(\.duration).max() ?? 0
        duration = rawDuration
        setPlaybackRange(contentRange)
        currentTime = windowStart
        setPlayheads(to: windowStart)
        isLoaded = !players.isEmpty
    }

    /// The worker exclusively owns these players until it returns. Main-actor
    /// playback takes ownership only after the generation/cancellation check.
    private struct PreparedPlayers: @unchecked Sendable { let values: [AVAudioPlayer] }

    func loadInBackground(folder: URL, hasSystemTrack: Bool, contentRange: Meeting.ContentRange? = nil) async {
        stop()
        let generation = loadGeneration
        let prepared = await Task.detached(priority: .userInitiated) {
            PreparedPlayers(values: MeetingAudioAsset.playbackSources(folder: folder, hasSystemTrack: hasSystemTrack)
                .compactMap { source in
                    guard let player = try? AVAudioPlayer(contentsOf: source.url) else { return nil }
                    player.enableRate = true
                    player.volume = source.gain
                    player.prepareToPlay()
                    return player
                })
        }.value
        guard !Task.isCancelled, generation == loadGeneration else { return }
        players = prepared.values
        for player in players { player.delegate = self }
        applySpeed()
        rawDuration = players.map(\.duration).max() ?? 0
        duration = rawDuration
        setPlaybackRange(contentRange)
        currentTime = windowStart
        setPlayheads(to: windowStart)
        isLoaded = !players.isEmpty
    }

    /// Limits active playback and waveform visibility to `range` while keeping
    /// the underlying audio files and `duration` untouched.
    func setPlaybackRange(_ range: Meeting.ContentRange?) {
        if let range, range.isValid, rawDuration > 0, range.start < rawDuration {
            windowStart = max(0, range.start)
            windowEnd = min(rawDuration, max(windowStart + 0.05, range.end))
        } else {
            windowStart = 0
            windowEnd = rawDuration
        }
        if currentTime < windowStart || currentTime > windowEnd {
            if isPlaying && currentTime >= windowEnd {
                pause()
            }
            let clamped = min(max(windowStart, currentTime), windowEnd)
            setPlayheads(to: clamped)
            currentTime = clamped
        }
    }

    private var activeRange: (start: TimeInterval, end: TimeInterval) {
        let upper = windowEnd > windowStart ? windowEnd : duration
        let lower = upper > windowStart ? windowStart : 0
        return (lower, upper)
    }

    func playPause() {
        if isPlaying { pause() } else { play() }
    }

    func play(at time: TimeInterval? = nil) {
        excerptEnd = nil
        guard !players.isEmpty else { return }
        let bounds = activeRange
        let outOfBounds = currentTime >= bounds.end || currentTime < bounds.start
        if let time {
            let clamped = min(max(bounds.start, time), bounds.end)
            setPlayheads(to: clamped)
            currentTime = clamped
        } else if bounds.end > bounds.start, outOfBounds {
            // Replaying after reaching the end (or from before the start boundary): rewind first.
            setPlayheads(to: bounds.start)
            currentTime = bounds.start
        }
        // Schedule against a shared device-time anchor so both tracks stay in sync.
        let anchor = (players.first?.deviceCurrentTime ?? 0) + 0.05
        for player in players {
            player.enableRate = true
            player.rate = speed
            player.play(atTime: anchor)
        }
        isPlaying = true
        startTicker()
    }

    /// Review a bounded voice sample without continuing through the meeting.
    func playExcerpt(from start: TimeInterval, to end: TimeInterval) {
        guard start.isFinite, end.isFinite, start >= 0, end > start, start < duration else { return }
        play(at: start)
        excerptEnd = min(end, duration)
    }

    /// Push the current `speed` to every player (live rate change).
    private func applySpeed() {
        for player in players where player.enableRate { player.rate = speed }
    }

    func pause() {
        excerptEnd = nil
        for player in players { player.pause() }
        isPlaying = false
        stopTicker()
    }

    func seek(to time: TimeInterval) {
        let wasPlaying = isPlaying
        pause()
        let bounds = activeRange
        let clamped = max(bounds.start, min(time, bounds.end))
        setPlayheads(to: clamped)
        currentTime = clamped
        if wasPlaying { play() }
    }

    func stop() {
        excerptEnd = nil
        loadGeneration &+= 1
        for player in players { player.stop() }
        players = []
        isPlaying = false
        isLoaded = false
        currentTime = 0
        duration = 0
        rawDuration = 0
        windowStart = 0
        windowEnd = 0
        stopTicker()
    }

    /// Position every track at `time`, clamped to that track's own length so a
    /// shorter track doesn't reject the seek.
    private func setPlayheads(to time: TimeInterval) {
        for player in players { player.currentTime = min(max(0, time), player.duration) }
    }

    private func startTicker() {
        stopTicker()
        ticker = Timer.scheduledTimer(withTimeInterval: 0.25, repeats: true) { [weak self] _ in
            Task { @MainActor [weak self] in
                guard let self, let timelinePlayer = self.timelinePlayer else { return }
                let bounds = self.activeRange
                self.currentTime = min(max(bounds.start, timelinePlayer.currentTime), bounds.end)
                let stopBoundary = self.excerptEnd.map { min($0, bounds.end) } ?? bounds.end
                if stopBoundary > bounds.start, timelinePlayer.currentTime >= stopBoundary {
                    self.pause()
                    self.setPlayheads(to: stopBoundary)
                    self.currentTime = stopBoundary
                }
            }
        }
    }

    private func stopTicker() {
        ticker?.invalidate()
        ticker = nil
    }
}

extension MeetingPlayer: AVAudioPlayerDelegate {
    nonisolated func audioPlayerDidFinishPlaying(_ player: AVAudioPlayer, successfully flag: Bool) {
        Task { @MainActor in
            // The longest track decides when the meeting is over.
            if players.allSatisfy({ !$0.isPlaying }) {
                isPlaying = false
                currentTime = activeRange.end
                stopTicker()
            }
        }
    }
}
