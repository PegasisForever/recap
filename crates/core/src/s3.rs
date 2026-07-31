//! Upload a recording and turn it into one shareable link.
//!
//! SigV4 signs a single object, so a bundle of several files cannot be one
//! URL on its own. The recording therefore gets an index: every part is
//! presigned, the index lists them, and the index itself is presigned. That
//! one link reaches the whole recording and nothing else in the bucket.

use crate::config::{clamp_expiry, Config};
use crate::manifest::{Manifest, Part, PartKind, MANIFEST_NAME, MANIFEST_VERSION, PLAYER_NAME};
use crate::record::FinishedPart;
use anyhow::{Context, Result};
use crate::progress::{Progress, Reporter, Stage};
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::{ByteStream, Length};
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::Client;
use std::path::Path;
use std::time::Duration;

pub async fn client(cfg: &Config) -> Result<Client> {
    if cfg.s3.bucket.is_empty() {
        anyhow::bail!("no S3 bucket configured. Set RECAP_S3_BUCKET or s3.bucket in the config.");
    }
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(cfg.s3.region.clone()));
    if !cfg.s3.access_key.is_empty() {
        loader = loader.credentials_provider(aws_credential_types::Credentials::new(
            cfg.s3.access_key.clone(),
            cfg.s3.secret_key.clone(),
            None,
            None,
            "recap-config",
        ));
    }
    let shared = loader.load().await;
    let mut b = aws_sdk_s3::config::Builder::from(&shared);
    if !cfg.s3.endpoint.is_empty() {
        b = b.endpoint_url(&cfg.s3.endpoint);
    }
    // MinIO and most self-hosted gateways have no wildcard DNS for buckets.
    b = b.force_path_style(cfg.s3.path_style);
    Ok(Client::from_conf(b.build()))
}

async fn presign(client: &Client, bucket: &str, key: &str, secs: u64) -> Result<String> {
    let cfg = PresigningConfig::expires_in(Duration::from_secs(clamp_expiry(secs)))?;
    let req = client
        .get_object()
        .bucket(bucket)
        .key(key)
        .presigned(cfg)
        .await
        .with_context(|| format!("presigning {key}"))?;
    Ok(req.uri().to_string())
}

/// Above this a single PUT is refused by S3 anyway, and below it multipart is
/// pointless overhead. An hour of two monitors lands well past this.
const MULTIPART_ABOVE: u64 = 64 * 1024 * 1024;
/// S3 rejects any part but the last under 5 MB, so this is the floor.
const MIN_PART: u64 = 16 * 1024 * 1024;
const MAX_PARTS: u64 = 9_000;

/// The multipart path only runs on files too big to produce in a quick test.
/// These two let a test drive it with a small file instead of a 20-minute
/// recording. Unset in normal use.
fn multipart_above() -> u64 {
    env_bytes("RECAP_MULTIPART_ABOVE").unwrap_or(MULTIPART_ABOVE)
}
fn min_part() -> u64 {
    env_bytes("RECAP_PART_SIZE").unwrap_or(MIN_PART).max(5 * 1024 * 1024)
}
fn env_bytes(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.parse().ok()
}

async fn put(
    client: &Client,
    bucket: &str,
    key: &str,
    path: &Path,
    ctype: &str,
    stage: &Stage,
    report: Reporter<'_>,
) -> Result<()> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("reading {}", path.display()))?
        .len();

    if size < multipart_above() {
        let body = ByteStream::from_path(path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        client
            .put_object()
            .bucket(bucket)
            .key(key)
            .content_type(ctype)
            .body(body)
            .send()
            .await
            .with_context(|| format!("uploading {key}"))?;
        report(Progress::new(stage, size, size));
        return Ok(());
    }

    // A single PUT is capped at 5 GB, which an hour of screen capture clears
    // easily. Multipart also means progress can be reported per chunk instead
    // of one silent wait.
    let part_size = min_part().max(size / MAX_PARTS + 1);
    let started = client
        .create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .content_type(ctype)
        .send()
        .await
        .with_context(|| format!("starting multipart upload of {key}"))?;
    let upload_id = started
        .upload_id()
        .context("S3 did not return an upload id")?
        .to_string();

    let result = upload_parts(client, bucket, key, path, size, part_size, &upload_id, stage, report).await;

    match result {
        Ok(parts) => {
            client
                .complete_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .multipart_upload(
                    CompletedMultipartUpload::builder().set_parts(Some(parts)).build(),
                )
                .send()
                .await
                .with_context(|| format!("completing upload of {key}"))?;
            Ok(())
        }
        Err(e) => {
            // Leave no half-uploaded object paying storage for ever.
            let _ = client
                .abort_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .send()
                .await;
            Err(e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn upload_parts(
    client: &Client,
    bucket: &str,
    key: &str,
    path: &Path,
    size: u64,
    part_size: u64,
    upload_id: &str,
    stage: &Stage,
    report: Reporter<'_>,
) -> Result<Vec<CompletedPart>> {
    let mut parts = Vec::new();
    let mut offset = 0u64;
    let mut number = 1i32;
    while offset < size {
        let len = part_size.min(size - offset);
        let body = ByteStream::read_from()
            .path(path)
            .offset(offset)
            .length(Length::Exact(len))
            .build()
            .await
            .with_context(|| format!("reading part {number} of {}", path.display()))?;
        let done = client
            .upload_part()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(number)
            .body(body)
            .send()
            .await
            .with_context(|| format!("uploading part {number} of {key}"))?;
        parts.push(
            CompletedPart::builder()
                .set_e_tag(done.e_tag().map(str::to_string))
                .part_number(number)
                .build(),
        );
        offset += len;
        number += 1;
        report(Progress::new(stage, offset, size));
    }
    Ok(parts)
}

/// A part that is ready to go out, and how big it turned out.
struct Prepared {
    path: std::path::PathBuf,
    bytes: u64,
}

/// Make one part streamable, off the async runtime because ffmpeg blocks.
///
/// Only video needs work: a browser shows nothing until it has the MP4 index,
/// and the recorder writes that at the end of the file. Audio goes as-is.
/// Returns None past the end of the list, which is what ends the pipeline.
fn spawn_prepare(
    parts: &[FinishedPart],
    i: usize,
    staging: &Path,
    step: u32,
    steps: u32,
    tx: tokio::sync::mpsc::UnboundedSender<Progress>,
) -> Option<tokio::task::JoinHandle<Prepared>> {
    let p = parts.get(i)?;
    let src = p.path.clone();
    let label = p.label.clone();
    let fallback_bytes = p.bytes;
    let is_video = p.kind == PartKind::Video;
    let web = staging.join(src.file_name().unwrap_or_default());

    Some(tokio::task::spawn_blocking(move || {
        let t0 = std::time::Instant::now();
        eprintln!("recap: prepare {label} start");
        let path = if is_video {
            let stage = Stage::new(format!("Preparing {label} for streaming"), step, steps);
            let mut report = |pr: Progress| {
                let _ = tx.send(pr);
            };
            let made = if crate::video::browser_ready(&src) {
                crate::video::faststart_reporting(&src, &web, &stage, &mut report)
            } else {
                crate::video::to_browser_h264_reporting(&src, &web, &stage, &mut report)
            };
            match made {
                Ok(()) => web,
                Err(e) => {
                    // An unreadable or exotic file still gets uploaded, just
                    // without the streaming fix.
                    eprintln!("recap: could not prepare {label} for streaming: {e}");
                    src
                }
            }
        } else {
            src
        };
        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(fallback_bytes);
        eprintln!("recap: prepare {label} done in {:.1}s", t0.elapsed().as_secs_f64());
        Prepared { path, bytes }
    }))
}

/// Wait for a preparation task, showing its progress while nothing else is
/// happening. Updates that piled up during the previous upload are collapsed to
/// the newest one, because replaying a stale percentage looks like going
/// backwards.
async fn await_prepare(
    handle: tokio::task::JoinHandle<Prepared>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Progress>,
    report: Reporter<'_>,
) -> Prepared {
    while let Ok(p) = rx.try_recv() {
        report(p);
    }
    tokio::pin!(handle);
    loop {
        tokio::select! {
            done = &mut handle => {
                return done.expect("preparation task panicked");
            }
            Some(p) = rx.recv() => report(p),
        }
    }
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("mp4") => "video/mp4",
        Some("mkv") => "video/x-matroska",
        Some("opus") | Some("ogg") => "audio/ogg",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

/// Push every part, write the index, and hand back the one link to copy.
pub async fn upload_recording(
    cfg: &Config,
    id: &str,
    started: u64,
    parts: Vec<FinishedPart>,
    first_step: u32,
    total_steps: u32,
    report: Reporter<'_>,
) -> Result<(Manifest, String)> {
    let client = client(cfg).await?;
    let bucket = &cfg.s3.bucket;
    let prefix = format!("rec/{id}");
    let expiry = cfg.s3.link_expiry_secs;

    // Preparing a video and uploading a part are separate stages, so a bar can
    // move during both instead of jumping.
    let videos = parts.iter().filter(|p| p.kind == PartKind::Video).count() as u32;
    let mut step = first_step;
    let prep_step = |i: usize| first_step + i.min(videos as usize) as u32;

    let mut manifest_parts = Vec::new();
    // A part that starts late also ends late, so the recording runs until the
    // last one finishes on the shared clock, not until the longest file does.
    let duration = parts
        .iter()
        .map(|p| p.offset_ms as f64 / 1000.0 + p.duration)
        .fold(0.0_f64, f64::max);
    let staging = crate::config::Config::staging_dir().join(format!("web-{id}"));
    std::fs::create_dir_all(&staging)?;

    // Preparing a monitor and uploading one are the two slow steps, and they
    // use different resources: ffmpeg wants disk and CPU, the upload wants the
    // network. Running them in lockstep wastes whichever is idle, so the next
    // part is prepared while the current one is going out.
    let (ptx, mut prx) = tokio::sync::mpsc::unbounded_channel::<Progress>();
    let mut pending = spawn_prepare(&parts, 0, &staging, prep_step(0), total_steps, ptx.clone());

    for i in 0..parts.len() {
        let prepared = match pending.take() {
            Some(handle) => await_prepare(handle, &mut prx, report).await,
            None => break,
        };
        // Start the next one before uploading this one, so the transfer and the
        // remux overlap instead of taking turns.
        pending = spawn_prepare(&parts, i + 1, &staging, prep_step(i + 1), total_steps, ptx.clone());

        let p = &parts[i];
        let name = p.path.file_name().and_then(|n| n.to_str()).unwrap_or("part.bin");
        let source = prepared.path;
        let bytes = prepared.bytes;

        let key = format!("{prefix}/{name}");
        let t0 = std::time::Instant::now();
        eprintln!("recap: upload {} start", p.label);
        step = first_step + videos + i as u32;
        let stage = Stage::new(format!("Uploading {}", p.label), step, total_steps);
        put(&client, bucket, &key, &source, content_type(&source), &stage, report).await?;
        eprintln!("recap: upload {} done in {:.1}s", p.label, t0.elapsed().as_secs_f64());
        let url = presign(&client, bucket, &key, expiry).await?;
        manifest_parts.push(Part {
            kind: p.kind,
            label: p.label.clone(),
            key: key.clone(),
            url,
            offset_ms: p.offset_ms,
            bytes,
            duration: p.duration,
        });
    }

    let manifest = Manifest {
        version: MANIFEST_VERSION,
        id: id.to_string(),
        created: started,
        duration,
        local_speaker: crate::manifest::LOCAL_SPEAKER.to_string(),
        parts: manifest_parts,
    };

    step += 1;
    let index_stage = Stage::new("Writing the index", step, total_steps);
    report(Progress::spinner(&index_stage));
    let mkey = format!("{prefix}/{MANIFEST_NAME}");
    let json = staging.join(MANIFEST_NAME);
    std::fs::write(&json, serde_json::to_vec_pretty(&manifest)?)?;
    put(&client, bucket, &mkey, &json, "application/json", &index_stage, report).await?;

    // The link opens a page, not a file. It carries the player and the manifest
    // together, so a person gets one transport for every track and
    // `watchvid.py` gets the part list from the same URL.
    step += 1;
    let player_stage = Stage::new("Writing the player page", step, total_steps);
    report(Progress::spinner(&player_stage));
    let pkey = format!("{prefix}/{PLAYER_NAME}");
    let html = staging.join(PLAYER_NAME);
    std::fs::write(&html, crate::player::render(&manifest))?;
    put(&client, bucket, &pkey, &html, "text/html; charset=utf-8", &player_stage, report).await?;
    let link = presign(&client, bucket, &pkey, expiry).await?;

    let _ = std::fs::remove_dir_all(&staging);
    Ok((manifest, link))
}

/// Resolve a copied link back into the bundle it indexes.
pub async fn fetch_manifest(url: &str) -> Result<Manifest> {
    let body = reqwest::get(url)
        .await
        .context("fetching the manifest link")?
        .error_for_status()
        .context("the manifest link was rejected, it may have expired")?
        .bytes()
        .await?;
    serde_json::from_slice(&body).context("the link did not point at a recap manifest")
}

/// Stream a presigned part to disk. Recordings are large, so this never holds
/// the whole body in memory.
pub async fn download(url: &str, out: &Path) -> Result<()> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;

    crate::config::ensure_parent(out)?;
    let resp = reqwest::get(url)
        .await
        .context("downloading part")?
        .error_for_status()
        .context("part rejected, the link may have expired")?;
    let mut file = tokio::fs::File::create(out).await?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        file.write_all(&chunk?).await?;
    }
    file.flush().await?;
    Ok(())
}
