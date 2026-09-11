//! Holt ein Original über den echten Sidecar — zum Nachstellen von Problemen.
//!
//! `cargo run -p icloud-photos-sync --example fetch_original -- <apple-id> <asset-id> <ziel>`

use std::path::PathBuf;

use icloud_photos_sync::{Prompts, SidecarOptions, SidecarSession};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let [username, asset_id, dest] = args.as_slice() else {
        eprintln!("Aufruf: fetch_original <apple-id> <asset-id> <zieldatei>");
        std::process::exit(2);
    };

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut options = SidecarOptions::new(
        root.join(".venv/bin/python"),
        root.join("sidecar/pyicloud_sync.py"),
        username,
    );
    options.cookie_dir = Some(
        directories::BaseDirs::new()
            .unwrap()
            .home_dir()
            .join(".local/share/icloud-photos/cookies"),
    );

    let started = std::time::Instant::now();
    let mut session = match SidecarSession::start(&options, &Prompts::default()).await {
        Ok(session) => session,
        Err(error) => {
            eprintln!("Sitzung nicht aufgebaut: {error}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "Sitzung bereit nach {:.1}s",
        started.elapsed().as_secs_f32()
    );

    match session
        .fetch_original(asset_id, None, std::path::Path::new(dest))
        .await
    {
        Ok(original) => eprintln!(
            "OK: {} Bytes nach {} (gesamt {:.1}s)",
            original.bytes,
            original.path.display(),
            started.elapsed().as_secs_f32()
        ),
        Err(error) => {
            eprintln!("Download fehlgeschlagen: {error}");
            std::process::exit(1);
        }
    }
    session.shutdown().await;
}
