use std::env;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use anyhow::{anyhow, Result};
use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Region, Credentials};
use aws_sdk_s3::Client as S3Client;
use base64::Engine;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::process::Command;

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LoginResponse {
    pub token: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PendingJob {
    pub id: i64,
    pub raw_file_key: Option<String>,
    pub delete_raw_after: bool,
    pub title: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ProgressPayload {
    pub message: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct CompletePayload {
    pub hls_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HardwareAccel {
    None,
    Nvenc,
    VideoToolbox,
}

#[derive(Clone)]
struct WorkerConfig {
    api_base_url: String,
    token: String,
    s3_client: S3Client,
    s3_bucket: String,
    s3_public_url: String,
    ffmpeg_path: String,
    ffprobe_path: String,
    accel: HardwareAccel,
}

async fn cleanup_leftovers() {
    let temp_parent = std::env::temp_dir();
    if let Ok(mut entries) = fs::read_dir(&temp_parent).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with("anonhive_worker_transcode_") {
                        println!("🧹 Cleaning up leftover temporary directory from previous run/crash: {:?}", path);
                        let _ = fs::remove_dir_all(&path).await;
                    }
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    println!("=== Anonhive Distributed Transcoding Worker ===");
    cleanup_leftovers().await;

    let api_base_url = env::var("API_BASE_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
    let email = env::var("ADMIN_EMAIL").expect("ADMIN_EMAIL must be set");
    let password = env::var("ADMIN_PASSWORD").expect("ADMIN_PASSWORD must be set");

    let s3_endpoint = env::var("R2_ENDPOINT").expect("R2_ENDPOINT must be set");
    let s3_access_key = env::var("R2_ACCESS_KEY").expect("R2_ACCESS_KEY must be set");
    let s3_secret_key = env::var("R2_SECRET_KEY").expect("R2_SECRET_KEY must be set");
    let s3_bucket = env::var("R2_BUCKET").expect("R2_BUCKET must be set");
    let s3_public_url = env::var("R2_PUBLIC_URL").expect("R2_PUBLIC_URL must be set");

    let ffmpeg_path = env::var("FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".to_string());
    let ffprobe_path = env::var("FFPROBE_PATH").unwrap_or_else(|_| "ffprobe".to_string());

    // 1. Build HTTP client
    let http = HttpClient::new();

    // 2. Login to APIs backend
    println!("Logging in to {}...", api_base_url);
    let login_res = http.post(format!("{}/api/v1/auth/login", api_base_url))
        .json(&serde_json::json!({
            "email": email,
            "password": password,
            "turnstileToken": "bypass-local-transcoder"
        }))
        .send()
        .await?;

    if !login_res.status().is_success() {
        return Err(anyhow!("Failed to login: {}", login_res.text().await?));
    }

    let login_payload: LoginResponse = login_res.json().await?;
    let token = login_payload.token;
    println!("Successfully authenticated!");

    // 3. Build S3 client
    let credentials = Credentials::new(
        s3_access_key,
        s3_secret_key,
        None,
        None,
        "cloudflare-worker",
    );
    let s3_config = aws_config::defaults(BehaviorVersion::latest())
        .endpoint_url(s3_endpoint)
        .credentials_provider(credentials)
        .region(Region::new("auto"))
        .load()
        .await;
    let s3_client = S3Client::from_conf(aws_sdk_s3::config::Builder::from(&s3_config).force_path_style(true).build());

    // 4. Check hardware acceleration support
    println!("Checking for hardware-accelerated video encoders...");
    let ffmpeg_check = Command::new(&ffmpeg_path)
        .arg("-encoders")
        .output()
        .await;

    let accel = match ffmpeg_check {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stdout.contains("h264_nvenc") || stderr.contains("h264_nvenc") {
                HardwareAccel::Nvenc
            } else if stdout.contains("h264_videotoolbox") || stderr.contains("h264_videotoolbox") {
                HardwareAccel::VideoToolbox
            } else {
                HardwareAccel::None
            }
        }
        Err(e) => {
            println!("⚠️ Could not execute FFmpeg at path '{}': {:?}", ffmpeg_path, e);
            println!("👉 Make sure FFmpeg is installed and added to your system's PATH, or set the FFMPEG_PATH environment variable.");
            HardwareAccel::None
        }
    };

    match accel {
        HardwareAccel::Nvenc => {
            println!("✅ NVIDIA NVENC hardware acceleration detected! Using GPU encoding on RTX 4060.");
        }
        HardwareAccel::VideoToolbox => {
            println!("✅ Apple VideoToolbox hardware acceleration detected! Using Apple Silicon M-series GPU encoding.");
        }
        HardwareAccel::None => {
            println!("⚠️ No hardware acceleration detected (NVENC / VideoToolbox). Falling back to CPU (libx264) encoding.");
        }
    }

    let config = Arc::new(WorkerConfig {
        api_base_url,
        token,
        s3_client,
        s3_bucket,
        s3_public_url,
        ffmpeg_path,
        ffprobe_path,
        accel,
    });

    // 5. Polling Loop
    println!("Polling for local transcode jobs every 5 seconds...");
    loop {
        if let Err(e) = poll_and_process_job(&http, &config).await {
            eprintln!("Error in poll loop: {:?}", e);
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

fn get_hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|out| String::from_utf8(out.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "Unknown-Worker".to_string())
        })
}

async fn poll_and_process_job(http: &HttpClient, config: &WorkerConfig) -> Result<()> {
    let name = get_hostname();
    let os = std::env::consts::OS;
    let accel = format!("{:?}", config.accel);

    let pending_url = format!("{}/api/v1/admin/transcode/pending", config.api_base_url);
    let res = http.get(&pending_url)
        .bearer_auth(&config.token)
        .header("X-Worker-Name", &name)
        .header("X-Worker-OS", os)
        .header("X-Worker-Accel", &accel)
        .send()
        .await?;

    if !res.status().is_success() {
        return Err(anyhow!("Failed to poll pending jobs: status={}", res.status()));
    }

    let jobs: Vec<PendingJob> = res.json().await?;
    if jobs.is_empty() {
        return Ok(());
    }

    // Process the first job
    let job = &jobs[0];
    println!("Found pending job: {:?} (id={})", job.title, job.id);

    // Claim the job
    let claim_url = format!("{}/api/v1/admin/transcode/claim/{}", config.api_base_url, job.id);
    let claim_res = http.post(&claim_url)
        .bearer_auth(&config.token)
        .send()
        .await?;

    if claim_res.status() == reqwest::StatusCode::CONFLICT {
        println!("Job {} already claimed by another worker.", job.id);
        return Ok(());
    }

    if !claim_res.status().is_success() {
        return Err(anyhow!("Failed to claim job: status={}", claim_res.status()));
    }

    println!("Successfully claimed job {}!", job.id);

    // Process the claimed job
    if let Err(e) = run_job_pipeline(http, config, job).await {
        eprintln!("Job pipeline failed for job {}: {:?}", job.id, e);
        // Set progress to failed
        let _ = report_progress(http, config, job.id, &format!("❌ Failed: {:?}", e)).await;
    }

    Ok(())
}

fn parse_ffmpeg_time(line: &str) -> Option<f64> {
    if let Some(pos) = line.find("time=") {
        let time_part = &line[pos + 5..];
        let time_str = time_part.split_whitespace().next()?;
        let parts: Vec<&str> = time_str.split(':').collect();
        if parts.len() == 3 {
            let h: f64 = parts[0].parse().unwrap_or(0.0);
            let m: f64 = parts[1].parse().unwrap_or(0.0);
            let s: f64 = parts[2].parse().unwrap_or(0.0);
            return Some(h * 3600.0 + m * 60.0 + s);
        }
    }
    None
}

async fn run_ffmpeg_with_progress(
    mut cmd: Command,
    desc: &str,
    duration_secs: f64,
    http: &HttpClient,
    config: &WorkerConfig,
    job_id: i64,
    quality: &str,
) -> Result<()> {
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::inherit());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn()?;
    let stderr = child.stderr.take().expect("Failed to capture stderr");

    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut reader = BufReader::new(stderr);
    let mut buf = Vec::new();
    let mut last_reported_pct = -1;

    while let Ok(n) = reader.read_until(b'\r', &mut buf).await {
        if n == 0 {
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        print!("{}", line);
        let _ = std::io::Write::flush(&mut std::io::stdout());

        if let Some(current_secs) = parse_ffmpeg_time(&line) {
            if duration_secs > 0.0 {
                let mut progress = (current_secs / duration_secs) * 100.0;
                if progress > 100.0 {
                    progress = 100.0;
                }
                let prog_pct = progress.round() as i32;
                if prog_pct != last_reported_pct {
                    last_reported_pct = prog_pct;
                    let status_msg = format!("Transcoding {} ({}%)", quality, prog_pct);
                    let _ = report_progress(http, config, job_id, &status_msg).await;
                }
            }
        }
        buf.clear();
    }

    let status = child.wait().await?;
    if !status.success() {
        return Err(anyhow!("{} failed with exit status {:?}", desc, status.code()));
    }
    Ok(())
}

async fn run_command_checked(mut cmd: Command, desc: &str) -> Result<()> {
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::inherit());
    cmd.stderr(std::process::Stdio::inherit());
    let mut child = cmd.spawn()?;
    let status = child.wait().await?;
    if !status.success() {
        return Err(anyhow!("{} failed with exit status {:?}", desc, status.code()));
    }
    Ok(())
}

async fn report_progress(http: &HttpClient, config: &WorkerConfig, job_id: i64, msg: &str) -> Result<()> {
    println!("[Job {}] Progress: {}", job_id, msg);
    let progress_url = format!("{}/api/v1/admin/transcode/progress/{}", config.api_base_url, job_id);
    let _ = http.post(&progress_url)
        .bearer_auth(&config.token)
        .json(&ProgressPayload { message: msg.to_string() })
        .send()
        .await;
    Ok(())
}

async fn run_job_pipeline(http: &HttpClient, config: &WorkerConfig, job: &PendingJob) -> Result<()> {
    let temp_dir = env::temp_dir().join(format!("anonhive_worker_transcode_{}", job.id));
    if temp_dir.exists() {
        let _ = fs::remove_dir_all(&temp_dir).await;
    }
    fs::create_dir_all(&temp_dir).await?;

    let res = run_job_pipeline_inner(http, config, job, &temp_dir).await;
    
    // Always clean up directory regardless of success/error
    let _ = fs::remove_dir_all(&temp_dir).await;
    res
}

async fn run_job_pipeline_inner(http: &HttpClient, config: &WorkerConfig, job: &PendingJob, temp_dir: &Path) -> Result<()> {
    let raw_key = job.raw_file_key.as_ref().ok_or_else(|| anyhow!("Missing raw file key"))?;

    report_progress(http, config, job.id, "Downloading raw video from cloud...").await?;
    
    let raw_path = temp_dir.join("raw.mp4");
    
    // S3 download file
    let mut get_object_res = config.s3_client.get_object()
        .bucket(&config.s3_bucket)
        .key(raw_key)
        .send()
        .await?;

    let mut dest_file = fs::File::create(&raw_path).await?;
    while let Some(bytes) = get_object_res.body.try_next().await? {
        tokio::io::AsyncWriteExt::write_all(&mut dest_file, &bytes).await?;
    }
    drop(dest_file);

    report_progress(http, config, job.id, "Analyzing video metadata...").await?;

    // Probe duration
    let duration_output = Command::new(&config.ffprobe_path)
        .args(&["-v", "error", "-show_entries", "format=duration", "-of", "default=noprint_wrappers=1:nokey=1", raw_path.to_str().unwrap()])
        .output()
        .await?;
    let duration_secs: f64 = String::from_utf8_lossy(&duration_output.stdout).trim().parse().unwrap_or(0.0);

    // Probe height
    let height_output = Command::new(&config.ffprobe_path)
        .args(&["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=height", "-of", "default=noprint_wrappers=1:nokey=1", raw_path.to_str().unwrap()])
        .output()
        .await?;
    let source_height: i32 = String::from_utf8_lossy(&height_output.stdout).trim().parse().unwrap_or(1080);

    report_progress(http, config, job.id, "Extracting scrubbing thumbnails...").await?;

    let thumbs_dir = temp_dir.join("thumbs");
    fs::create_dir_all(&thumbs_dir).await?;

    let mut thumbs_cmd = Command::new(&config.ffmpeg_path);
    thumbs_cmd.args(&[
        "-y",
        "-i", raw_path.to_str().unwrap(),
        "-vf", "fps=1/10,scale=160:-1,tile=10x10",
        "-q:v", "5",
        thumbs_dir.join("thumbnails_%03d.jpg").to_str().unwrap()
    ]);
    run_command_checked(thumbs_cmd, "Thumbnail extraction").await?;

    // Upload thumbnails
    upload_hls_directory(config, &thumbs_dir, &format!("hls/{}/thumbs", job.id)).await?;

    let has_1080 = source_height >= 1000;
    let has_720 = source_height >= 700;

    let mut master_playlist = "#EXTM3U\n#EXT-X-VERSION:3\n".to_string();
    let master_path = temp_dir.join("master.txt");
    let master_key = format!("hls/{}/master.txt", job.id);

    // 1080p
    if has_1080 {
        report_progress(http, config, job.id, "Transcoding 1080p stream...").await?;
        let out_1080_dir = temp_dir.join("1080p");
        fs::create_dir_all(&out_1080_dir).await?;

        let segment_pattern = out_1080_dir.join("1080p_%03d.ts");
        let segment_pattern_str = segment_pattern.to_str().unwrap().to_string();
        let m3u8_file = out_1080_dir.join("1080p.m3u8");
        let m3u8_file_str = m3u8_file.to_str().unwrap().to_string();

        let mut success = false;
        if config.accel == HardwareAccel::Nvenc {
            let mut cmd = Command::new(&config.ffmpeg_path);
            let args = vec![
                "-y",
                "-fflags", "+genpts",
                "-i", raw_path.to_str().unwrap(),
                "-vf", "scale=-2:1080,format=yuv420p",
                "-c:v", "h264_nvenc",
                "-preset", "p4",
                "-map", "0:v:0",
                "-map", "0:a?",
                "-c:a", "aac",
                "-ac", "2",
                "-b:a", "192k",
                "-max_muxing_queue_size", "1024",
                "-threads", "4",
                "-f", "hls",
                "-hls_time", "4",
                "-hls_playlist_type", "vod",
                "-hls_segment_filename", &segment_pattern_str,
                &m3u8_file_str,
            ];
            cmd.args(&args);
            if let Err(e) = run_ffmpeg_with_progress(cmd, "1080p NVENC transcoding", duration_secs, http, config, job.id, "1080p").await {
                println!("⚠️ NVENC failed: {}. Retrying with CPU (libx264) encoding...", e);
            } else {
                success = true;
            }
        } else if config.accel == HardwareAccel::VideoToolbox {
            let mut cmd = Command::new(&config.ffmpeg_path);
            let args = vec![
                "-y",
                "-fflags", "+genpts",
                "-i", raw_path.to_str().unwrap(),
                "-vf", "scale=-2:1080,format=yuv420p",
                "-c:v", "h264_videotoolbox",
                "-realtime", "true",
                "-map", "0:v:0",
                "-map", "0:a?",
                "-c:a", "aac",
                "-ac", "2",
                "-b:a", "192k",
                "-max_muxing_queue_size", "1024",
                "-threads", "4",
                "-f", "hls",
                "-hls_time", "4",
                "-hls_playlist_type", "vod",
                "-hls_segment_filename", &segment_pattern_str,
                &m3u8_file_str,
            ];
            cmd.args(&args);
            if let Err(e) = run_ffmpeg_with_progress(cmd, "1080p VideoToolbox transcoding", duration_secs, http, config, job.id, "1080p").await {
                println!("⚠️ Apple VideoToolbox failed: {}. Retrying with CPU (libx264) encoding...", e);
            } else {
                success = true;
            }
        }
        if !success {
            let mut cmd = Command::new(&config.ffmpeg_path);
            let args = vec![
                "-y",
                "-fflags", "+genpts",
                "-i", raw_path.to_str().unwrap(),
                "-vf", "scale=-2:1080,format=yuv420p",
                "-c:v", "libx264",
                "-preset", "ultrafast",
                "-map", "0:v:0",
                "-map", "0:a?",
                "-c:a", "aac",
                "-ac", "2",
                "-b:a", "192k",
                "-max_muxing_queue_size", "1024",
                "-threads", "4",
                "-f", "hls",
                "-hls_time", "4",
                "-hls_playlist_type", "vod",
                "-hls_segment_filename", &segment_pattern_str,
                &m3u8_file_str,
            ];
            cmd.args(&args);
            run_ffmpeg_with_progress(cmd, "1080p CPU transcoding", duration_secs, http, config, job.id, "1080p").await?;
        }

        obfuscate_hls_dir(&out_1080_dir).await?;
        upload_hls_directory(config, &out_1080_dir, &format!("hls/{}/1080p", job.id)).await?;
        master_playlist.push_str("#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080\n1080p/1080p.txt\n");
    }

    // 720p
    if has_720 {
        report_progress(http, config, job.id, "Transcoding 720p stream...").await?;
        let out_720_dir = temp_dir.join("720p");
        fs::create_dir_all(&out_720_dir).await?;

        let segment_pattern = out_720_dir.join("720p_%03d.ts");
        let segment_pattern_str = segment_pattern.to_str().unwrap().to_string();
        let m3u8_file = out_720_dir.join("720p.m3u8");
        let m3u8_file_str = m3u8_file.to_str().unwrap().to_string();

        let mut success = false;
        if config.accel == HardwareAccel::Nvenc {
            let mut cmd = Command::new(&config.ffmpeg_path);
            let args = vec![
                "-y",
                "-fflags", "+genpts",
                "-i", raw_path.to_str().unwrap(),
                "-vf", "scale=-2:720,format=yuv420p",
                "-c:v", "h264_nvenc",
                "-preset", "p4",
                "-map", "0:v:0",
                "-map", "0:a?",
                "-c:a", "aac",
                "-ac", "2",
                "-b:a", "192k",
                "-max_muxing_queue_size", "1024",
                "-threads", "4",
                "-f", "hls",
                "-hls_time", "4",
                "-hls_playlist_type", "vod",
                "-hls_segment_filename", &segment_pattern_str,
                &m3u8_file_str,
            ];
            cmd.args(&args);
            if let Err(e) = run_ffmpeg_with_progress(cmd, "720p NVENC transcoding", duration_secs, http, config, job.id, "720p").await {
                println!("⚠️ NVENC failed: {}. Retrying with CPU (libx264) encoding...", e);
            } else {
                success = true;
            }
        } else if config.accel == HardwareAccel::VideoToolbox {
            let mut cmd = Command::new(&config.ffmpeg_path);
            let args = vec![
                "-y",
                "-fflags", "+genpts",
                "-i", raw_path.to_str().unwrap(),
                "-vf", "scale=-2:720,format=yuv420p",
                "-c:v", "h264_videotoolbox",
                "-realtime", "true",
                "-map", "0:v:0",
                "-map", "0:a?",
                "-c:a", "aac",
                "-ac", "2",
                "-b:a", "192k",
                "-max_muxing_queue_size", "1024",
                "-threads", "4",
                "-f", "hls",
                "-hls_time", "4",
                "-hls_playlist_type", "vod",
                "-hls_segment_filename", &segment_pattern_str,
                &m3u8_file_str,
            ];
            cmd.args(&args);
            if let Err(e) = run_ffmpeg_with_progress(cmd, "720p VideoToolbox transcoding", duration_secs, http, config, job.id, "720p").await {
                println!("⚠️ Apple VideoToolbox failed: {}. Retrying with CPU (libx264) encoding...", e);
            } else {
                success = true;
            }
        }
        if !success {
            let mut cmd = Command::new(&config.ffmpeg_path);
            let args = vec![
                "-y",
                "-fflags", "+genpts",
                "-i", raw_path.to_str().unwrap(),
                "-vf", "scale=-2:720,format=yuv420p",
                "-c:v", "libx264",
                "-preset", "ultrafast",
                "-map", "0:v:0",
                "-map", "0:a?",
                "-c:a", "aac",
                "-ac", "2",
                "-b:a", "192k",
                "-max_muxing_queue_size", "1024",
                "-threads", "4",
                "-f", "hls",
                "-hls_time", "4",
                "-hls_playlist_type", "vod",
                "-hls_segment_filename", &segment_pattern_str,
                &m3u8_file_str,
            ];
            cmd.args(&args);
            run_ffmpeg_with_progress(cmd, "720p CPU transcoding", duration_secs, http, config, job.id, "720p").await?;
        }

        obfuscate_hls_dir(&out_720_dir).await?;
        upload_hls_directory(config, &out_720_dir, &format!("hls/{}/720p", job.id)).await?;
        master_playlist.push_str("#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720\n720p/720p.txt\n");
    }

    // 480p
    {
        report_progress(http, config, job.id, "Transcoding 480p stream...").await?;
        let out_480_dir = temp_dir.join("480p");
        fs::create_dir_all(&out_480_dir).await?;

        let segment_pattern = out_480_dir.join("480p_%03d.ts");
        let segment_pattern_str = segment_pattern.to_str().unwrap().to_string();
        let m3u8_file = out_480_dir.join("480p.m3u8");
        let m3u8_file_str = m3u8_file.to_str().unwrap().to_string();

        let mut success = false;
        if config.accel == HardwareAccel::Nvenc {
            let mut cmd = Command::new(&config.ffmpeg_path);
            let args = vec![
                "-y",
                "-fflags", "+genpts",
                "-i", raw_path.to_str().unwrap(),
                "-vf", "scale=-2:480,format=yuv420p",
                "-c:v", "h264_nvenc",
                "-preset", "p4",
                "-map", "0:v:0",
                "-map", "0:a?",
                "-c:a", "aac",
                "-ac", "2",
                "-b:a", "192k",
                "-max_muxing_queue_size", "1024",
                "-threads", "4",
                "-f", "hls",
                "-hls_time", "4",
                "-hls_playlist_type", "vod",
                "-hls_segment_filename", &segment_pattern_str,
                &m3u8_file_str,
            ];
            cmd.args(&args);
            if let Err(e) = run_ffmpeg_with_progress(cmd, "480p NVENC transcoding", duration_secs, http, config, job.id, "480p").await {
                println!("⚠️ NVENC failed: {}. Retrying with CPU (libx264) encoding...", e);
            } else {
                success = true;
            }
        } else if config.accel == HardwareAccel::VideoToolbox {
            let mut cmd = Command::new(&config.ffmpeg_path);
            let args = vec![
                "-y",
                "-fflags", "+genpts",
                "-i", raw_path.to_str().unwrap(),
                "-vf", "scale=-2:480,format=yuv420p",
                "-c:v", "h264_videotoolbox",
                "-realtime", "true",
                "-map", "0:v:0",
                "-map", "0:a?",
                "-c:a", "aac",
                "-ac", "2",
                "-b:a", "192k",
                "-max_muxing_queue_size", "1024",
                "-threads", "4",
                "-f", "hls",
                "-hls_time", "4",
                "-hls_playlist_type", "vod",
                "-hls_segment_filename", &segment_pattern_str,
                &m3u8_file_str,
            ];
            cmd.args(&args);
            if let Err(e) = run_ffmpeg_with_progress(cmd, "480p VideoToolbox transcoding", duration_secs, http, config, job.id, "480p").await {
                println!("⚠️ Apple VideoToolbox failed: {}. Retrying with CPU (libx264) encoding...", e);
            } else {
                success = true;
            }
        }
        if !success {
            let mut cmd = Command::new(&config.ffmpeg_path);
            let args = vec![
                "-y",
                "-fflags", "+genpts",
                "-i", raw_path.to_str().unwrap(),
                "-vf", "scale=-2:480,format=yuv420p",
                "-c:v", "libx264",
                "-preset", "ultrafast",
                "-map", "0:v:0",
                "-map", "0:a?",
                "-c:a", "aac",
                "-ac", "2",
                "-b:a", "192k",
                "-max_muxing_queue_size", "1024",
                "-threads", "4",
                "-f", "hls",
                "-hls_time", "4",
                "-hls_playlist_type", "vod",
                "-hls_segment_filename", &segment_pattern_str,
                &m3u8_file_str,
            ];
            cmd.args(&args);
            run_ffmpeg_with_progress(cmd, "480p CPU transcoding", duration_secs, http, config, job.id, "480p").await?;
        }

        obfuscate_hls_dir(&out_480_dir).await?;
        upload_hls_directory(config, &out_480_dir, &format!("hls/{}/480p", job.id)).await?;
        master_playlist.push_str("#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=854x480\n480p/480p.txt\n");
    }

    report_progress(http, config, job.id, "Finalizing master HLS playlist...").await?;

    let master_encoded = base64::engine::general_purpose::STANDARD.encode(&master_playlist);
    fs::write(&master_path, &master_encoded).await?;
    
    // Upload master playlist
    let master_body = fs::read(&master_path).await?;
    config.s3_client.put_object()
        .bucket(&config.s3_bucket)
        .key(&master_key)
        .body(master_body.into())
        .content_type("text/plain")
        .cache_control("max-age=0")
        .send()
        .await?;

    let hls_url = format!("{}/{}", config.s3_public_url, master_key);

    // Call complete endpoint
    let complete_url = format!("{}/api/v1/admin/transcode/complete/{}", config.api_base_url, job.id);
    let complete_res = http.post(&complete_url)
        .bearer_auth(&config.token)
        .json(&CompletePayload { hls_url })
        .send()
        .await?;

    if !complete_res.status().is_success() {
        return Err(anyhow!("Failed to complete job: status={}", complete_res.status()));
    }

    println!("✅ Job {} transcode completed successfully!", job.id);

    Ok(())
}

async fn obfuscate_hls_dir(dir: &Path) -> Result<()> {
    let mut dir_entries = fs::read_dir(dir).await?;
    while let Some(entry) = dir_entries.next_entry().await? {
        let path = entry.path();
        if let Some(ext) = path.extension() {
            if ext == "ts" {
                let new_path = path.with_extension("bin");
                fs::rename(&path, &new_path).await?;
            } else if ext == "m3u8" {
                let content = fs::read_to_string(&path).await?;
                let obfuscated_content = content.replace(".ts", ".bin");
                let encoded = base64::engine::general_purpose::STANDARD.encode(obfuscated_content);
                let new_path = path.with_extension("txt");
                fs::write(&new_path, encoded).await?;
                fs::remove_file(&path).await?;
            }
        }
    }
    Ok(())
}

async fn upload_hls_directory(config: &WorkerConfig, dir: &Path, s3_prefix: &str) -> Result<()> {
    let mut dir_entries = fs::read_dir(dir).await?;
    let mut tasks = Vec::new();
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(16)); // 16 parallel uploads

    while let Some(entry) = dir_entries.next_entry().await? {
        let path = entry.path();
        if path.is_file() {
            let filename = path.file_name().unwrap().to_str().unwrap().to_string();
            let key = format!("{}/{}", s3_prefix, filename);
            let content_type = match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
                "bin" => "video/mp2t",
                "txt" => "text/plain",
                "jpg" => "image/jpeg",
                _ => "application/octet-stream",
            };

            let sem_clone = sem.clone();
            let config_clone = config.clone();

            tasks.push(tokio::spawn(async move {
                let _permit = sem_clone.acquire().await.unwrap();
                let body = fs::read(&path).await?;
                config_clone.s3_client.put_object()
                    .bucket(&config_clone.s3_bucket)
                    .key(&key)
                    .body(body.into())
                    .content_type(content_type)
                    .send()
                    .await?;
                Result::<()>::Ok(())
            }));
        }
    }

    for t in tasks {
        t.await??;
    }
    Ok(())
}
