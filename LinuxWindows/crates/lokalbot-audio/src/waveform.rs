use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::decode::TrackReader;

const CACHE_FILE: &str = ".waveform-peaks.json";
const DETAIL_CACHE_FILE: &str = ".waveform-peaks-detail-v1.json";
const CACHE_LIMIT_BYTES: u64 = 1 << 20;
pub const DETAIL_BUCKETS: usize = 16_384;

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    key: String,
    peaks: Vec<f32>,
}

pub fn waveform_peaks(paths: &[&Path], buckets: usize) -> Result<Vec<f32>> {
    if buckets == 0 {
        return Ok(Vec::new());
    }
    let paths: Vec<&Path> = paths.iter().copied().filter(|p| p.is_file()).collect();
    if paths.is_empty() {
        bail!("no audio to draw");
    }
    let key = cache_key(&paths, buckets);
    let cache = cache_path(&paths, buckets);
    if let (Some(cache), Some(key)) = (&cache, &key)
        && let Some(peaks) = read_cache(cache, key, buckets)
    {
        return Ok(peaks);
    }
    let peaks = compute_peaks(&paths, buckets)?;
    if let (Some(cache), Some(key)) = (cache, key) {
        let entry = CacheEntry {
            key,
            peaks: peaks.clone(),
        };
        if let Ok(json) = serde_json::to_vec(&entry)
            && json.len() as u64 <= CACHE_LIMIT_BYTES
        {
            let tmp = cache.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, &cache).is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
    Ok(peaks)
}

pub fn resample_peaks(
    peaks: &[f32],
    duration: f64,
    visible_start: f64,
    visible_end: f64,
    width: f64,
) -> Vec<f32> {
    if peaks.is_empty()
        || !duration.is_finite()
        || duration <= 0.0
        || !visible_start.is_finite()
        || !visible_end.is_finite()
        || visible_end <= visible_start
        || !width.is_finite()
        || width <= 0.0
    {
        return Vec::new();
    }
    let start_ratio = (visible_start / duration).clamp(0.0, 1.0);
    let end_ratio = (visible_end / duration).clamp(0.0, 1.0);
    if end_ratio <= start_ratio {
        return Vec::new();
    }
    let start_index = ((start_ratio * peaks.len() as f64).floor() as usize).min(peaks.len() - 1);
    let end_index = ((end_ratio * peaks.len() as f64).ceil() as usize)
        .max(start_index + 1)
        .min(peaks.len());
    let slice_len = end_index - start_index;
    let count = ((width / 4.0).floor().min(slice_len as f64) as usize)
        .max(1)
        .min(slice_len);
    (0..count)
        .map(|index| {
            let lo = start_index + index * slice_len / count;
            let hi = (start_index + (index + 1) * slice_len / count)
                .max(lo + 1)
                .min(end_index);
            peaks[lo..hi]
                .iter()
                .copied()
                .filter(|p| p.is_finite())
                .fold(0.0f32, f32::max)
                .clamp(0.0, 1.0)
        })
        .collect()
}

fn cache_path(paths: &[&Path], buckets: usize) -> Option<PathBuf> {
    let folder = paths.first()?.parent()?;
    let file = if buckets == DETAIL_BUCKETS {
        DETAIL_CACHE_FILE
    } else {
        CACHE_FILE
    };
    paths
        .iter()
        .all(|p| p.parent() == Some(folder))
        .then(|| folder.join(file))
}

fn cache_key(paths: &[&Path], buckets: usize) -> Option<String> {
    let mut key = format!("v1|{buckets}");
    for path in paths {
        let meta = std::fs::metadata(path).ok()?;
        let modified = meta
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos();
        let name = path.file_name()?.to_string_lossy();
        key.push_str(&format!("|{name}:{}:{modified}", meta.len()));
    }
    Some(key)
}

fn read_cache(path: &Path, key: &str, buckets: usize) -> Option<Vec<f32>> {
    if std::fs::metadata(path).ok()?.len() > CACHE_LIMIT_BYTES {
        return None;
    }
    let entry: CacheEntry = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (entry.key == key
        && entry.peaks.len() == buckets
        && entry
            .peaks
            .iter()
            .all(|p| p.is_finite() && (0.0..=1.0).contains(p)))
    .then_some(entry.peaks)
}

fn compute_peaks(paths: &[&Path], buckets: usize) -> Result<Vec<f32>> {
    let mut readers = Vec::new();
    for path in paths {
        match TrackReader::open(path) {
            Ok(reader) => readers.push(reader),
            Err(error) => log::warn!("waveform: skipping {}: {error}", path.display()),
        }
    }
    let duration = readers
        .iter()
        .map(|r| r.duration_seconds())
        .fold(0.0, f64::max);
    if readers.is_empty() || duration <= 0.0 {
        bail!("no audio to draw");
    }
    let mut peaks = vec![0.0f32; buckets];
    let mut block = Vec::new();
    for reader in &mut readers {
        let rate = reader.sample_rate() as f64;
        let mut frame = 0u64;
        loop {
            block.clear();
            let read = reader
                .read_mono(1 << 15, &mut block)
                .context("could not read audio")?;
            if read == 0 {
                break;
            }
            for &sample in &block {
                let time = frame as f64 / rate;
                let bin = ((time / duration) * buckets as f64) as usize;
                let bin = bin.min(buckets - 1);
                let magnitude = if sample.is_finite() {
                    sample.abs()
                } else {
                    0.0
                };
                if magnitude > peaks[bin] {
                    peaks[bin] = magnitude;
                }
                frame += 1;
            }
        }
    }
    let maximum = peaks.iter().copied().fold(0.0f32, f32::max);
    if maximum > 0.0 {
        for peak in &mut peaks {
            *peak = (*peak / maximum).min(1.0);
        }
    }
    Ok(peaks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::write_wav_16k;

    #[test]
    fn peaks_follow_time_and_cache() {
        let dir = tempfile::tempdir().unwrap();
        let mic = dir.path().join("mic.wav");
        let system = dir.path().join("system.wav");
        let mut mic_samples = vec![0.8f32; 8_000];
        mic_samples.extend(vec![0.0f32; 24_000]);
        write_wav_16k(&mic, &mic_samples).unwrap();
        write_wav_16k(&system, &vec![0.2f32; 32_000]).unwrap();
        let peaks = waveform_peaks(&[&mic, &system], 8).unwrap();
        assert_eq!(peaks.len(), 8);
        assert!((peaks[0] - 1.0).abs() < 1e-3);
        assert!((peaks[3] - 0.25).abs() < 0.01, "{peaks:?}");
        assert!(dir.path().join(CACHE_FILE).is_file());
        assert_eq!(waveform_peaks(&[&mic, &system], 8).unwrap(), peaks);
        assert_eq!(waveform_peaks(&[&mic, &system], 4).unwrap().len(), 4);
    }

    #[test]
    fn detail_peaks_use_separate_detail_cache_file() {
        let dir = tempfile::tempdir().unwrap();
        let mic = dir.path().join("mic.wav");
        let mut mic_samples = vec![0.8f32; 8_000];
        mic_samples.extend(vec![0.1f32; 24_000]);
        write_wav_16k(&mic, &mic_samples).unwrap();

        let peaks = waveform_peaks(&[&mic], DETAIL_BUCKETS).unwrap();
        assert_eq!(peaks.len(), DETAIL_BUCKETS);
        let detail_cache = dir.path().join(DETAIL_CACHE_FILE);
        assert!(detail_cache.is_file());
        assert!(std::fs::metadata(&detail_cache).unwrap().len() <= CACHE_LIMIT_BYTES);
        assert!(!dir.path().join(CACHE_FILE).exists());
        assert_eq!(waveform_peaks(&[&mic], DETAIL_BUCKETS).unwrap(), peaks);
    }

    #[test]
    fn detail_cache_rejects_oversized_file() {
        let dir = tempfile::tempdir().unwrap();
        let mic = dir.path().join("mic.wav");
        write_wav_16k(&mic, &vec![0.5f32; 16_000]).unwrap();
        let detail_cache = dir.path().join(DETAIL_CACHE_FILE);
        let oversized = vec![b' '; (CACHE_LIMIT_BYTES + 16) as usize];
        std::fs::write(&detail_cache, oversized).unwrap();

        let peaks = waveform_peaks(&[&mic], DETAIL_BUCKETS).unwrap();
        assert_eq!(peaks.len(), DETAIL_BUCKETS);
        assert!(peaks.iter().all(|p| p.is_finite() && *p >= 0.0));
    }

    #[test]
    fn resample_peaks_covers_visible_slice_and_handles_edge_cases() {
        let mut peaks = vec![0.0f32; DETAIL_BUCKETS];
        let target = (DETAIL_BUCKETS * 3) / 4;
        peaks[target] = 1.0;

        let full = resample_peaks(&peaks, 100.0, 0.0, 100.0, 400.0);
        assert_eq!(full.len(), 100);
        assert_eq!(full.iter().copied().fold(0.0f32, f32::max), 1.0);

        let early = resample_peaks(&peaks, 100.0, 0.0, 50.0, 400.0);
        assert_eq!(early.iter().copied().fold(0.0f32, f32::max), 0.0);

        let zoomed = resample_peaks(&peaks, 100.0, 70.0, 80.0, 400.0);
        assert_eq!(zoomed.iter().copied().fold(0.0f32, f32::max), 1.0);

        let clamped = resample_peaks(&peaks, 100.0, -20.0, 150.0, 200.0);
        assert_eq!(clamped.len(), 50);
        assert_eq!(clamped.iter().copied().fold(0.0f32, f32::max), 1.0);

        let narrow = resample_peaks(&[0.2, 0.9, 0.3, 0.4], 4.0, 0.0, 2.0, 400.0);
        assert_eq!(narrow, vec![0.2, 0.9]);

        assert!(resample_peaks(&[], 10.0, 0.0, 10.0, 100.0).is_empty());
        assert!(resample_peaks(&peaks, 0.0, 0.0, 10.0, 100.0).is_empty());
        assert!(resample_peaks(&peaks, 10.0, 5.0, 5.0, 100.0).is_empty());
        assert!(resample_peaks(&peaks, 10.0, 0.0, 10.0, 0.0).is_empty());
        assert!(resample_peaks(&peaks, f64::NAN, 0.0, 10.0, 100.0).is_empty());
    }
}
