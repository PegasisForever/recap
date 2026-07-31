//! Multipart upload against a real S3 endpoint.
//!
//! The multipart path only runs on files larger than a quick recording
//! produces, so without this it would first execute on a user's three-hour
//! session. Run with a MinIO on localhost:9000:
//!
//!   cargo test -p recap-core --test multipart -- --ignored --nocapture

use recap_core::config::{Config, S3Config};
use recap_core::manifest::PartKind;
use recap_core::progress::Progress;
use recap_core::record::FinishedPart;
use std::io::Write;

fn minio() -> Config {
    Config {
        s3: S3Config {
            bucket: "recordings".into(),
            endpoint: "http://localhost:9000".into(),
            region: "us-east-1".into(),
            access_key: "minioadmin".into(),
            secret_key: "minioadmin123".into(),
            path_style: true,
            link_expiry_secs: 3600,
        },
        ..Default::default()
    }
}

/// Write a file with a byte pattern that changes every part, so a mis-ordered
/// or duplicated part shows up as a content mismatch rather than passing.
fn make_file(path: &std::path::Path, size: usize) {
    let mut f = std::fs::File::create(path).unwrap();
    let mut buf = vec![0u8; 1 << 16];
    let mut written = 0usize;
    while written < size {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((written + i) % 251) as u8;
        }
        let n = buf.len().min(size - written);
        f.write_all(&buf[..n]).unwrap();
        written += n;
    }
    f.flush().unwrap();
}

#[tokio::test]
#[ignore = "needs a MinIO on localhost:9000"]
async fn multipart_roundtrip_is_byte_identical() {
    // 12 MB against a 5 MB part size is three parts: two full and a remainder.
    // That exercises the loop, the part numbering and the final short part.
    std::env::set_var("RECAP_MULTIPART_ABOVE", "1000000");
    std::env::set_var("RECAP_PART_SIZE", "5242880");

    let dir = std::env::temp_dir().join("recap-multipart-test");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("monitor-0.mp4");
    const SIZE: usize = 12 * 1024 * 1024;
    make_file(&src, SIZE);

    let id = format!("test-{}", uuid::Uuid::new_v4());
    let parts = vec![FinishedPart {
        kind: PartKind::Video,
        label: "Monitor 1".into(),
        path: src.clone(),
        offset_ms: 0,
        bytes: SIZE as u64,
        duration: 1.0,
    }];

    let mut seen: Vec<(String, u64, u64)> = Vec::new();
    let mut report = |p: Progress| seen.push((p.phase.clone(), p.done, p.total));

    let cfg = minio();
    let (manifest, link) =
        recap_core::s3::upload_recording(&cfg, &id, 0, parts, 1, 4, &mut report)
            .await
            .expect("upload should succeed");

    // The uploader reported real byte counts, not one lump at the end.
    let upload_ticks: Vec<_> = seen
        .iter()
        .filter(|(phase, _, total)| phase.starts_with("Uploading") && *total == SIZE as u64)
        .collect();
    assert!(
        upload_ticks.len() >= 3,
        "expected at least one tick per part, got {}: {:?}",
        upload_ticks.len(),
        upload_ticks
    );
    assert_eq!(
        upload_ticks.last().unwrap().1,
        SIZE as u64,
        "final tick should report the whole file"
    );

    // What came back has to be the bytes that went out. faststart rewrites a
    // real MP4, but this synthetic file is not one, so it is stored verbatim.
    let url = &manifest.videos().next().unwrap().url;
    let got = reqwest::get(url).await.unwrap().bytes().await.unwrap();
    assert_eq!(got.len(), SIZE, "reassembled object is the wrong length");
    let expected = std::fs::read(&src).unwrap();
    assert!(got[..] == expected[..], "reassembled object differs from the source");

    assert!(link.contains("index.html"), "link should open the player: {link}");
    let _ = std::fs::remove_dir_all(&dir);
}
