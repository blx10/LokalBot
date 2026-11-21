import AVFoundation
import Accelerate
import Foundation

/// Peak envelopes for all audible tracks on the player's shared timeline.
/// This is an activity overview, not a phase-accurate mix of the source signals.
enum WaveformAnalysis {
    struct Source: Hashable, Sendable {
        let url: URL
        let gain: Float
    }

    struct Request: Hashable, Sendable {
        let sources: [Source]
        let duration: TimeInterval
        let binCount: Int

        init(sources: [Source], duration: TimeInterval, binCount: Int = WaveformAnalysis.binCount) {
            self.sources = sources
            self.duration = duration
            self.binCount = binCount
        }
    }

    static let binCount = 1_024
    static let detailBinCount = 16_384
    private static let cache: NSCache<NSString, PeaksBox> = {
        let cache = NSCache<NSString, PeaksBox>()
        cache.countLimit = 64
        return cache
    }()

    static func load(_ request: Request) async -> [Float]? {
        guard !Task.isCancelled else { return nil }
        let worker = Task.detached(priority: .utility) { () -> [Float]? in
            guard !Task.isCancelled else { return nil }
            let key = cacheKey(request)
            if let cached = cache.object(forKey: key) { return cached.peaks }
            if let peaks = readDiskCache(request, key: key as String) {
                cache.setObject(PeaksBox(peaks), forKey: key)
                return peaks
            }
            guard let peaks = decode(request), !Task.isCancelled else { return nil }
            // Do not save or publish an envelope if a source changed mid-read.
            guard key == cacheKey(request) else { return nil }
            cache.setObject(PeaksBox(peaks), forKey: key)
            writeDiskCache(peaks, request: request, key: key as String)
            return peaks
        }
        return await withTaskCancellationHandler {
            let result = await worker.value
            return Task.isCancelled ? nil : result
        } onCancel: {
            worker.cancel()
        }
    }

    /// The disk cache lives with the recording, so deleting a meeting removes
    /// its derived peaks too. It stores only 1,024 floats and source identities.
    private struct DiskEntry: Codable {
        let key: String
        let peaks: [Float]
    }

    static func diskCacheURL(for request: Request) -> URL? {
        guard let first = request.sources.first, first.url.isFileURL else { return nil }
        let folder = first.url.deletingLastPathComponent()
        guard request.sources.allSatisfy({ $0.url.isFileURL && $0.url.deletingLastPathComponent() == folder }) else {
            return nil
        }
        let fileName = request.binCount == binCount ? ".waveform-peaks-v2.json" : ".waveform-peaks-detail-v1.json"
        return folder.appendingPathComponent(fileName)
    }

    private static func readDiskCache(_ request: Request, key: String) -> [Float]? {
        let maxBytes = request.binCount == binCount ? 65_536 : 1_048_576
        guard let url = diskCacheURL(for: request),
              let attributes = try? FileManager.default.attributesOfItem(atPath: url.path),
              let size = attributes[.size] as? NSNumber, size.intValue <= maxBytes,
              let data = try? Data(contentsOf: url),
              let entry = try? JSONDecoder().decode(DiskEntry.self, from: data),
              entry.key == key, entry.peaks.count == request.binCount,
              entry.peaks.allSatisfy({ $0.isFinite && $0 >= 0 && $0 <= 1 }) else { return nil }
        return entry.peaks
    }

    private static func writeDiskCache(_ peaks: [Float], request: Request, key: String) {
        guard !Task.isCancelled, let url = diskCacheURL(for: request),
              let data = try? JSONEncoder().encode(DiskEntry(key: key, peaks: peaks)) else { return }
        // A read-only folder must never prevent playback or waveform loading.
        try? data.write(to: url, options: .atomic)
    }

    static func clearMemoryCache() {
        cache.removeAllObjects()
    }

    /// File identity includes modification time and size so replacing a recording
    /// at the same path does not reuse its old waveform on the next load.
    private static func cacheKey(_ request: Request) -> NSString {
        let files = request.sources.map { source in
            // URL resource values can be cached by Foundation for this URL
            // instance; ask the filesystem afresh on every load.
            let attributes = try? FileManager.default.attributesOfItem(atPath: source.url.path)
            let size = attributes?[.size] as? NSNumber
            let modified = attributes?[.modificationDate] as? Date
            return "\(source.url.absoluteString)|\(source.gain)|\(size?.int64Value ?? -1)|\(modified?.timeIntervalSince1970 ?? 0)"
        }
        let binPrefix = request.binCount == binCount ? "" : "b\(request.binCount):"
        return ("\(binPrefix)\(request.duration):" + files.joined(separator: "\n")) as NSString
    }

    /// Bounded PCM buffers keep long recordings off the main actor without
    /// allocating a full decoded file. Sample time, rather than each track's
    /// length, determines the bin, so shorter tracks never stretch to fill it.
    static func decode(_ request: Request) -> [Float]? {
        let bins = max(1, request.binCount)
        guard request.duration.isFinite, request.duration > 0 else { return nil }
        var combined = [Float](repeating: 0, count: bins)
        var decodedTrack = false
        for source in request.sources {
            guard !Task.isCancelled else { return nil }
            guard source.gain.isFinite, source.gain > 0,
                  let track = decode(source, duration: request.duration, binCount: bins) else { continue }
            decodedTrack = true
            for index in combined.indices {
                combined[index] = max(combined[index], track[index])
            }
        }
        guard decodedTrack else { return nil }
        let maximum = combined.max() ?? 0
        return maximum > 0 ? combined.map { $0 / maximum } : combined
    }

    private static func decode(_ source: Source, duration: TimeInterval, binCount: Int) -> [Float]? {
        guard let file = try? AVAudioFile(forReading: source.url,
                                         commonFormat: .pcmFormatFloat32, interleaved: false),
              file.length > 0 else { return nil }
        let format = file.processingFormat
        guard format.sampleRate.isFinite, format.sampleRate > 0 else { return nil }
        let chunkFrames: AVAudioFrameCount = 32_768
        guard let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: chunkFrames) else { return nil }
        var peaks = [Float](repeating: 0, count: binCount)
        var absoluteFrame: Int64 = 0
        while absoluteFrame < file.length {
            guard !Task.isCancelled else { return nil }
            do {
                try file.read(into: buffer,
                              frameCount: AVAudioFrameCount(min(Int64(chunkFrames), file.length - absoluteFrame)))
            } catch { return nil }
            let frameCount = Int(buffer.frameLength)
            guard frameCount > 0, let channels = buffer.floatChannelData else { return nil }
            var frame = 0
            while frame < frameCount {
                let absolute = absoluteFrame + Int64(frame)
                let time = Double(absolute) / format.sampleRate
                guard time < duration else { break }
                let bin = min(binCount - 1, Int(time / duration * Double(binCount)))
                // Reduce a whole bin/chunk intersection at once. The old loop
                // performed divisions and Swift array updates for every sample.
                let boundary = ceil(Double(bin + 1) * duration / Double(binCount) * format.sampleRate)
                let remaining = Double(frameCount - frame)
                let count = max(1, Int(min(remaining, max(0, boundary - Double(absolute)))))
                var peak: Float = 0
                for channel in 0..<Int(format.channelCount) {
                    let samples = channels[channel].advanced(by: frame)
                    var maximum: Float = 0
                    vDSP_maxmgv(samples, 1, &maximum, vDSP_Length(count))
                    if !maximum.isFinite {
                        // Malformed PCM must not poison the entire envelope.
                        maximum = 0
                        for index in 0..<count where samples[index].isFinite {
                            maximum = max(maximum, abs(samples[index]))
                        }
                    }
                    peak = max(peak, maximum)
                }
                peaks[bin] = max(peaks[bin], peak * source.gain)
                frame += count
            }
            absoluteFrame += Int64(frameCount)
            if Double(absoluteFrame) / format.sampleRate >= duration { break }
        }
        return peaks
    }

    /// Pool peaks to the available width instead of overflowing a narrow player
    /// or dropping brief sounds between sampled points.
    static func resample(_ peaks: [Float], width: Double) -> [Float] {
        guard !peaks.isEmpty, width.isFinite, width > 0 else { return [] }
        let count = min(peaks.count, max(1, Int(min(width / 4, Double(peaks.count)))))
        return (0..<count).map { index in
            let start = index * peaks.count / count
            let end = (index + 1) * peaks.count / count
            return peaks[start..<end].max() ?? 0
        }
    }

    /// Pool peaks inside `[visibleStart, visibleEnd]` on a `0...duration` timeline.
    static func resample(
        _ peaks: [Float],
        duration: TimeInterval,
        visibleStart: TimeInterval,
        visibleEnd: TimeInterval,
        width: Double
    ) -> [Float] {
        guard !peaks.isEmpty, duration.isFinite, duration > 0,
              visibleStart.isFinite, visibleEnd.isFinite, visibleEnd > visibleStart,
              width.isFinite, width > 0 else { return [] }
        let startRatio = clamp(visibleStart / duration)
        let endRatio = clamp(visibleEnd / duration)
        guard endRatio > startRatio else { return [] }
        let startIndex = min(peaks.count - 1, max(0, Int(floor(startRatio * Double(peaks.count)))))
        let endIndex = min(peaks.count, max(startIndex + 1, Int(ceil(endRatio * Double(peaks.count)))))
        let slice = peaks[startIndex..<endIndex]
        let count = min(slice.count, max(1, Int(min(width / 4, Double(slice.count)))))
        return (0..<count).map { index in
            let lo = startIndex + index * slice.count / count
            let hi = min(endIndex, max(lo + 1, startIndex + (index + 1) * slice.count / count))
            return peaks[lo..<hi].max() ?? 0
        }
    }

    static func clamp(_ progress: Double) -> Double {
        progress.isFinite ? min(1, max(0, progress)) : 0
    }
}

private final class PeaksBox {
    let peaks: [Float]
    init(_ peaks: [Float]) { self.peaks = peaks }
}
