use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use lokalbot_core::LibraryPaths;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::asr::Decoded;
use crate::models::{self, SpeechModel, SpeechModelPaths};
use crate::text::normalized_text;

pub const WHISPER_SERVER_ENV: &str = "LOKALBOT_WHISPER_SERVER";
const READY_TIMEOUT: Duration = Duration::from_secs(45);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
const NO_SPEECH_DROP_THRESHOLD: f64 = 0.75;
const MIN_AUDIO_SAMPLES: usize = 16_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComputeDevice {
    Igpu,
    Cpu,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeLoadConfig {
    pub preferred_device: ComputeDevice,
    pub auto_fallback_to_cpu: bool,
    pub threads: usize,
    pub models_paths: SpeechModelPaths,
    pub whisper_server_bin: Option<PathBuf>,
}

impl Default for RuntimeLoadConfig {
    fn default() -> Self {
        Self {
            preferred_device: ComputeDevice::Igpu,
            auto_fallback_to_cpu: true,
            threads: 4,
            models_paths: SpeechModelPaths::default(),
            whisper_server_bin: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceProvenance {
    pub model_id: String,
    pub requested_device: ComputeDevice,
    pub actual_device: ComputeDevice,
    pub fallback_occurred: bool,
    pub fallback_reason: Option<String>,
    pub backend_detail: String,
}

struct ServerProcess {
    child: Child,
    port: u16,
    stderr_log: Arc<Mutex<String>>,
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        process::stop_child(&mut self.child, Duration::from_secs(2));
    }
}

enum RunnerBackend {
    Process {
        binary: PathBuf,
        model_file: PathBuf,
        language: Option<String>,
        threads: usize,
        auto_fallback_to_cpu: bool,
        server: Mutex<Option<ServerProcess>>,
    },
    #[cfg(test)]
    MockHttp {
        base_url: String,
    },
}

pub struct WhisperRunner {
    agent: ureq::Agent,
    provenance: Mutex<DeviceProvenance>,
    backend: RunnerBackend,
}

impl WhisperRunner {
    pub fn load(
        paths: &LibraryPaths,
        model: &'static SpeechModel,
        language: Option<&str>,
        config: RuntimeLoadConfig,
    ) -> Result<Self> {
        let model_dir = models::resolve_model_dir(paths, &config.models_paths, model);
        models::verify_integrity(&model_dir, model)?;
        let model_file = models::resolve_model_file(
            paths,
            &config.models_paths,
            model,
            model.files.first().copied().unwrap_or("ggml-model.bin"),
        );
        let binary = find_whisper_server(paths, &config)?;
        let threads = config.threads.clamp(1, 16);
        let language = normalized_language(language);
        let agent = loopback_agent(REQUEST_TIMEOUT);
        let (server, provenance) = start_with_fallback(
            &agent,
            &binary,
            &model_file,
            model.id,
            language.as_deref(),
            threads,
            config.preferred_device,
            config.auto_fallback_to_cpu,
        )?;
        Ok(Self {
            agent,
            provenance: Mutex::new(provenance),
            backend: RunnerBackend::Process {
                binary,
                model_file,
                language,
                threads,
                auto_fallback_to_cpu: config.auto_fallback_to_cpu,
                server: Mutex::new(Some(server)),
            },
        })
    }

    #[cfg(test)]
    pub(crate) fn with_mock_url(
        base_url: impl Into<String>,
        provenance: DeviceProvenance,
    ) -> Self {
        Self {
            agent: loopback_agent(Duration::from_secs(10)),
            provenance: Mutex::new(provenance),
            backend: RunnerBackend::MockHttp {
                base_url: base_url.into(),
            },
        }
    }

    pub fn provenance(&self) -> DeviceProvenance {
        self.provenance.lock().clone()
    }

    pub fn decode(&self, samples_16k: &[f32], language: Option<&str>) -> Result<Decoded> {
        if samples_16k.is_empty() {
            return Ok(Decoded::default());
        }
        let mut padded = samples_16k.to_vec();
        if padded.len() < MIN_AUDIO_SAMPLES {
            padded.resize(MIN_AUDIO_SAMPLES, 0.0);
        }
        let wav = lokalbot_audio::wav_bytes(&padded, lokalbot_audio::SAMPLE_RATE);
        match &self.backend {
            #[cfg(test)]
            RunnerBackend::MockHttp { base_url } => {
                post_inference(&self.agent, base_url, &wav, language)
            }
            RunnerBackend::Process {
                binary,
                model_file,
                language: loaded_lang,
                threads,
                auto_fallback_to_cpu,
                server,
            } => {
                let effective_lang = normalized_language(language).or_else(|| loaded_lang.clone());
                let url = {
                    let guard = server.lock();
                    let proc = guard
                        .as_ref()
                        .ok_or_else(|| anyhow!("whisper-server process is not running"))?;
                    format!("http://127.0.0.1:{}/inference", proc.port)
                };
                match post_inference(&self.agent, &url, &wav, effective_lang.as_deref()) {
                    Ok(decoded) => Ok(decoded),
                    Err(err) => {
                        if !*auto_fallback_to_cpu {
                            return Err(err);
                        }
                        let mut guard = server.lock();
                        if self.provenance.lock().actual_device != ComputeDevice::Igpu {
                            let proc = guard
                                .as_ref()
                                .ok_or_else(|| anyhow!("whisper-server process is not running"))?;
                            let retry_url = format!("http://127.0.0.1:{}/inference", proc.port);
                            drop(guard);
                            return post_inference(
                                &self.agent,
                                &retry_url,
                                &wav,
                                effective_lang.as_deref(),
                            );
                        }
                        let reason = format!("whisper-server iGPU inference failed: {err:#}");
                        log::warn!("speech: {reason}; falling back to CPU");
                        *guard = None;
                        let cpu_server = spawn_and_wait(
                            &self.agent,
                            binary,
                            model_file,
                            loaded_lang.as_deref(),
                            *threads,
                            ComputeDevice::Cpu,
                        )?;
                        let retry_url = format!("http://127.0.0.1:{}/inference", cpu_server.port);
                        *guard = Some(cpu_server);
                        drop(guard);
                        {
                            let mut prov = self.provenance.lock();
                            prov.actual_device = ComputeDevice::Cpu;
                            prov.fallback_occurred = true;
                            prov.fallback_reason = Some(reason);
                            prov.backend_detail = "whisper.cpp-cpu".to_string();
                        }
                        post_inference(&self.agent, &retry_url, &wav, effective_lang.as_deref())
                    }
                }
            }
        }
    }
}

fn normalized_language(language: Option<&str>) -> Option<String> {
    language
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("auto"))
        .map(str::to_string)
}

fn whisper_server_filename() -> &'static str {
    if cfg!(windows) {
        "whisper-server.exe"
    } else {
        "whisper-server"
    }
}

fn ggml_vulkan_filename() -> &'static str {
    if cfg!(windows) {
        "ggml-vulkan.dll"
    } else {
        "libggml-vulkan.so"
    }
}

pub fn find_whisper_server(paths: &LibraryPaths, config: &RuntimeLoadConfig) -> Result<PathBuf> {
    let exe_name = whisper_server_filename();
    let mut candidates = Vec::new();
    if let Some(explicit) = &config.whisper_server_bin {
        candidates.push(explicit.clone());
    }
    if let Some(env_bin) = std::env::var_os(WHISPER_SERVER_ENV) {
        candidates.push(PathBuf::from(env_bin));
    }
    if let Some(exe_dir) = &config.models_paths.exe_dir {
        candidates.push(exe_dir.join("runtimes").join("whisper-cpp").join(exe_name));
        candidates.push(exe_dir.join(exe_name));
    }
    if let Ok(current_exe) = std::env::current_exe()
        && let Some(dir) = current_exe.parent()
    {
        candidates.push(dir.join("runtimes").join("whisper-cpp").join(exe_name));
        candidates.push(dir.join(exe_name));
    }
    candidates.push(paths.runtimes_dir().join("whisper-cpp").join(exe_name));

    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }
    bail!(
        "Could not find {exe_name}. Checked: {}",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
}

fn vulkan_loader_present() -> bool {
    #[cfg(windows)]
    {
        let sys_dir = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("System32");
        sys_dir.join("vulkan-1.dll").is_file()
    }
    #[cfg(not(windows))]
    {
        [
            "/usr/lib/x86_64-linux-gnu/libvulkan.so.1",
            "/lib/x86_64-linux-gnu/libvulkan.so.1",
            "/usr/lib64/libvulkan.so.1",
            "/usr/lib/libvulkan.so.1",
            "/usr/local/lib/libvulkan.so.1",
        ]
        .iter()
        .any(|p| Path::new(p).exists())
    }
}

pub fn vulkan_available_for(binary: &Path) -> std::result::Result<(), String> {
    if !vulkan_loader_present() {
        return Err("system Vulkan loader is not present".to_string());
    }
    let has_ggml_vulkan = binary
        .parent()
        .is_some_and(|dir| dir.join(ggml_vulkan_filename()).is_file());
    if !has_ggml_vulkan {
        return Err(format!(
            "{} was not found beside {}",
            ggml_vulkan_filename(),
            binary.display()
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn start_with_fallback(
    agent: &ureq::Agent,
    binary: &Path,
    model_file: &Path,
    model_id: &str,
    language: Option<&str>,
    threads: usize,
    requested_device: ComputeDevice,
    auto_fallback_to_cpu: bool,
) -> Result<(ServerProcess, DeviceProvenance)> {
    start_with_fallback_using(
        binary,
        model_id,
        requested_device,
        auto_fallback_to_cpu,
        |device| spawn_and_wait(agent, binary, model_file, language, threads, device),
    )
}

fn start_with_fallback_using<T>(
    binary: &Path,
    model_id: &str,
    requested_device: ComputeDevice,
    auto_fallback_to_cpu: bool,
    mut spawner: impl FnMut(ComputeDevice) -> Result<T>,
) -> Result<(T, DeviceProvenance)> {
    if requested_device == ComputeDevice::Cpu {
        let server = spawner(ComputeDevice::Cpu)?;
        let provenance = DeviceProvenance {
            model_id: model_id.to_string(),
            requested_device,
            actual_device: ComputeDevice::Cpu,
            fallback_occurred: false,
            fallback_reason: None,
            backend_detail: "whisper.cpp-cpu".to_string(),
        };
        return Ok((server, provenance));
    }

    let probe_err = vulkan_available_for(binary).err();
    if let Some(reason) = probe_err {
        if !auto_fallback_to_cpu {
            bail!("Vulkan iGPU unavailable for {model_id}: {reason}");
        }
        log::warn!("speech: {reason}; starting {model_id} on CPU");
        let server = spawner(ComputeDevice::Cpu)?;
        let provenance = DeviceProvenance {
            model_id: model_id.to_string(),
            requested_device,
            actual_device: ComputeDevice::Cpu,
            fallback_occurred: true,
            fallback_reason: Some(reason),
            backend_detail: "whisper.cpp-cpu".to_string(),
        };
        return Ok((server, provenance));
    }

    match spawner(ComputeDevice::Igpu) {
        Ok(server) => {
            let provenance = DeviceProvenance {
                model_id: model_id.to_string(),
                requested_device,
                actual_device: ComputeDevice::Igpu,
                fallback_occurred: false,
                fallback_reason: None,
                backend_detail: "whisper.cpp-vulkan".to_string(),
            };
            Ok((server, provenance))
        }
        Err(err) => {
            if !auto_fallback_to_cpu {
                return Err(err);
            }
            let reason = format!("Vulkan iGPU launch failed: {err:#}");
            log::warn!("speech: {reason}; falling back to CPU for {model_id}");
            let server = spawner(ComputeDevice::Cpu)?;
            let provenance = DeviceProvenance {
                model_id: model_id.to_string(),
                requested_device,
                actual_device: ComputeDevice::Cpu,
                fallback_occurred: true,
                fallback_reason: Some(reason),
                backend_detail: "whisper.cpp-cpu".to_string(),
            };
            Ok((server, provenance))
        }
    }
}

fn spawn_and_wait(
    agent: &ureq::Agent,
    binary: &Path,
    model_file: &Path,
    language: Option<&str>,
    threads: usize,
    device: ComputeDevice,
) -> Result<ServerProcess> {
    let listener = TcpListener::bind("127.0.0.1:0").context("bind ephemeral port")?;
    let port = listener.local_addr()?.port();
    drop(listener);

    let mut command = Command::new(binary);
    if let Some(dir) = binary.parent() {
        command.current_dir(dir);
    }
    command
        .arg("-m")
        .arg(model_file)
        .arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .arg("-t")
        .arg(threads.to_string())
        .arg("-l")
        .arg(language.unwrap_or("auto"))
        .arg("-bs")
        .arg("5")
        .arg("-mc")
        .arg("0")
        .arg("-nth")
        .arg("0.6")
        .arg("-lpt")
        .arg("-1.0")
        .arg("-et")
        .arg("2.4")
        .arg("-sns")
        .arg("-nlp");
    if device == ComputeDevice::Cpu {
        command.arg("-ng");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut child = process::spawn_tied(command)
        .with_context(|| format!("failed to launch {}", binary.display()))?;
    let stderr_log = Arc::new(Mutex::new(String::new()));
    if let Some(mut stderr) = child.stderr.take() {
        let log_buf = Arc::clone(&stderr_log);
        let _ = std::thread::Builder::new()
            .name("whisper-server-stderr".into())
            .spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = stderr.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let chunk = String::from_utf8_lossy(&buf[..n]);
                    let mut guard = log_buf.lock();
                    guard.push_str(&chunk);
                    if guard.len() > 8192 {
                        let excess = guard.len() - 8192;
                        let cut = guard
                            .char_indices()
                            .find(|(idx, _)| *idx >= excess)
                            .map_or(guard.len(), |(idx, _)| idx);
                        guard.drain(..cut);
                    }
                }
            });
    }

    let mut server = ServerProcess {
        child,
        port,
        stderr_log: Arc::clone(&stderr_log),
    };

    let health_url = format!("http://127.0.0.1:{port}/health");
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if let Some(status) = server.child.try_wait()? {
            let log_tail = server.stderr_log.lock().clone();
            bail!("whisper-server exited early with {status}: {log_tail}");
        }
        {
            let log_now = server.stderr_log.lock();
            if device == ComputeDevice::Igpu
                && (log_now.contains("ggml_vk") && log_now.contains("error")
                    || log_now.contains("failed to initialize"))
            {
                bail!("whisper-server GPU init error: {log_now}");
            }
        }
        if let Ok(mut resp) = agent.get(&health_url).call()
            && resp.status().as_u16() == 200
        {
            let _ = resp.body_mut().read_to_string();
            return Ok(server);
        }
        if Instant::now() >= deadline {
            let log_tail = server.stderr_log.lock().clone();
            bail!("whisper-server timed out waiting for /health: {log_tail}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn loopback_agent(timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_connect(Some(Duration::from_secs(5)))
        .timeout_global(Some(timeout))
        .proxy(None)
        .build();
    ureq::Agent::new_with_config(config)
}

fn build_multipart(boundary: &str, wav: &[u8], language: Option<&str>) -> Vec<u8> {
    let mut fields = vec![
        ("response_format", "verbose_json"),
        ("temperature", "0.0"),
        ("token_timestamps", "false"),
        ("no_language_probabilities", "true"),
    ];
    if let Some(lang) = language.filter(|l| !l.trim().is_empty()) {
        fields.push(("language", lang.trim()));
    } else {
        fields.push(("language", "auto"));
    }
    let mut body = Vec::with_capacity(wav.len() + 512);
    for (name, value) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
                .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(wav);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

#[derive(Debug, Deserialize)]
struct InferenceSegment {
    #[serde(default)]
    text: String,
    #[serde(default)]
    no_speech_prob: f64,
}

#[derive(Debug, Deserialize)]
struct InferenceResponse {
    #[serde(default)]
    text: String,
    #[serde(default)]
    segments: Vec<InferenceSegment>,
}

fn post_inference(
    agent: &ureq::Agent,
    url: &str,
    wav: &[u8],
    language: Option<&str>,
) -> Result<Decoded> {
    let boundary = format!(
        "lokalbot-whisper-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let body = build_multipart(&boundary, wav, language);
    let mut response = agent
        .post(url)
        .header("Content-Type", &format!("multipart/form-data; boundary={boundary}"))
        .header("Accept", "application/json")
        .send(&body[..])
        .with_context(|| format!("whisper-server POST {url} failed"))?;
    let status = response.status().as_u16();
    let bytes = response
        .body_mut()
        .with_config()
        .limit(16 * 1024 * 1024)
        .read_to_vec()
        .context("failed to read whisper-server response")?;
    if !(200..300).contains(&status) {
        let detail = String::from_utf8_lossy(&bytes);
        bail!("whisper-server returned HTTP {status}: {detail}");
    }
    parse_inference_response(&bytes)
}

pub(crate) fn parse_inference_response(bytes: &[u8]) -> Result<Decoded> {
    let parsed: InferenceResponse =
        serde_json::from_slice(bytes).context("invalid JSON from whisper-server")?;
    let text = if !parsed.segments.is_empty() {
        let kept: Vec<&str> = parsed
            .segments
            .iter()
            .filter(|seg| seg.no_speech_prob <= NO_SPEECH_DROP_THRESHOLD)
            .map(|seg| seg.text.trim())
            .filter(|s| !s.is_empty())
            .collect();
        normalized_text(&kept.join(" "))
    } else {
        normalized_text(&parsed.text)
    };
    Ok(Decoded {
        text,
        ..Default::default()
    })
}

mod process {
    use std::io;
    use std::process::{Child, Command};
    use std::time::Duration;

    pub fn spawn_tied(command: Command) -> io::Result<Child> {
        #[cfg(target_os = "linux")]
        {
            linux::spawn(command)
        }
        #[cfg(windows)]
        {
            let mut command = command;
            use std::os::windows::process::CommandExt as _;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
            let child = command.spawn()?;
            windows::assign_to_job(&child);
            Ok(child)
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            let mut command = command;
            command.spawn()
        }
    }

    pub fn stop_child(child: &mut Child, grace: Duration) {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        #[cfg(unix)]
        {
            use std::time::Instant;
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
            let deadline = Instant::now() + grace;
            while Instant::now() < deadline {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        #[cfg(not(unix))]
        let _ = grace;
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use std::io;
        use std::os::unix::process::CommandExt as _;
        use std::process::{Child, Command};

        pub fn spawn(mut command: Command) -> io::Result<Child> {
            let parent = std::process::id() as libc::pid_t;
            unsafe {
                command.pre_exec(move || {
                    if libc::prctl(
                        libc::PR_SET_PDEATHSIG,
                        libc::SIGTERM as libc::c_ulong,
                        0,
                        0,
                        0,
                    ) != 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::getppid() != parent {
                        return Err(io::Error::from_raw_os_error(libc::ESRCH));
                    }
                    Ok(())
                });
            }
            command.spawn()
        }
    }

    #[cfg(windows)]
    mod windows {
        use std::os::windows::io::AsRawHandle as _;
        use std::process::Child;
        use std::sync::OnceLock;

        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        struct Job(HANDLE);
        unsafe impl Send for Job {}
        unsafe impl Sync for Job {}

        static JOB: OnceLock<Option<Job>> = OnceLock::new();

        fn job() -> Option<HANDLE> {
            JOB.get_or_init(|| unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return None;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                Some(Job(handle))
            })
            .as_ref()
            .map(|job| job.0)
        }

        pub fn assign_to_job(child: &Child) {
            if let Some(job) = job() {
                unsafe {
                    AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    #[test]
    fn parse_filters_high_no_speech_prob_segments() {
        let json = r#"{
            "text": "Merhaba. Sil.",
            "segments": [
                { "text": " Merhaba ", "no_speech_prob": 0.12 },
                { "text": " Sil. ", "no_speech_prob": 0.91 },
                { "text": " Dünya! ", "no_speech_prob": 0.40 }
            ]
        }"#
        .as_bytes();
        let decoded = parse_inference_response(json).unwrap();
        assert_eq!(decoded.text, "Merhaba Dünya!");
    }

    #[test]
    fn mock_http_inference_sends_multipart_and_decodes_response() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server_thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut req_buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = stream.read(&mut tmp).unwrap();
                if n == 0 {
                    break;
                }
                req_buf.extend_from_slice(&tmp[..n]);
                if let Some(hdr_end) = req_buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&req_buf[..hdr_end]);
                    let content_len = headers
                        .lines()
                        .find_map(|line| {
                            let (k, v) = line.split_once(':')?;
                            if k.eq_ignore_ascii_case("content-length") {
                                v.trim().parse::<usize>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    if req_buf.len() >= hdr_end + 4 + content_len {
                        break;
                    }
                }
            }
            let body = r#"{"text":"LokalBot test","segments":[{"text":"LokalBot test","no_speech_prob":0.05}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).unwrap();
        });

        let runner = WhisperRunner::with_mock_url(
            format!("http://{addr}/inference"),
            DeviceProvenance {
                model_id: "whisper-turbo".into(),
                requested_device: ComputeDevice::Igpu,
                actual_device: ComputeDevice::Igpu,
                fallback_occurred: false,
                fallback_reason: None,
                backend_detail: "mock".into(),
            },
        );
        let decoded = runner.decode(&vec![0.1f32; 8000], Some("tr")).unwrap();
        assert_eq!(decoded.text, "LokalBot test");
        server_thread.join().unwrap();
    }

    #[test]
    fn vulkan_probe_rejects_binary_without_ggml_vulkan_lib() {
        let dir = tempfile::tempdir().unwrap();
        let fake_bin = dir.path().join(whisper_server_filename());
        std::fs::write(&fake_bin, b"stub").unwrap();
        assert!(vulkan_available_for(&fake_bin).is_err());
    }

    #[test]
    fn find_whisper_server_uses_explicit_override() {
        let dir = tempfile::tempdir().unwrap();
        let fake_bin = dir.path().join(whisper_server_filename());
        std::fs::write(&fake_bin, b"stub").unwrap();
        let paths = LibraryPaths::new(dir.path());
        let cfg = RuntimeLoadConfig {
            whisper_server_bin: Some(fake_bin.clone()),
            ..Default::default()
        };
        assert_eq!(find_whisper_server(&paths, &cfg).unwrap(), fake_bin);
    }

    #[test]
    fn cpu_fallback_triggers_when_ggml_vulkan_missing() {
        let dir = tempfile::tempdir().unwrap();
        let fake_bin = dir.path().join(whisper_server_filename());
        std::fs::write(&fake_bin, b"stub").unwrap();
        assert!(vulkan_available_for(&fake_bin).is_err());

        let mut called = Vec::new();
        let (tag, prov) = start_with_fallback_using(
            &fake_bin,
            "whisper-turbo",
            ComputeDevice::Igpu,
            true,
            |device| {
                called.push(device);
                Ok(device)
            },
        )
        .unwrap();
        assert_eq!(tag, ComputeDevice::Cpu);
        assert_eq!(called, vec![ComputeDevice::Cpu]);
        assert_eq!(prov.requested_device, ComputeDevice::Igpu);
        assert_eq!(prov.actual_device, ComputeDevice::Cpu);
        assert!(prov.fallback_occurred);
        assert_eq!(prov.backend_detail, "whisper.cpp-cpu");

        let mut disabled_calls = 0usize;
        let err = start_with_fallback_using(
            &fake_bin,
            "whisper-turbo",
            ComputeDevice::Igpu,
            false,
            |_| -> Result<()> {
                disabled_calls += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(disabled_calls, 0);
        assert!(err.to_string().contains("Vulkan iGPU unavailable"));
    }
}
