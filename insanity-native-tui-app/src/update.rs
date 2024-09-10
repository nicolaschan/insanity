use std::os::unix::fs::PermissionsExt;

use insanity_core::built_info;
use log::{info, warn};
use tokio::{fs::create_dir_all, io::AsyncWriteExt};

pub async fn update(dry_run: bool, force: bool) -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let current_version = format!("v{}", built_info::PKG_VERSION);
    info!("Current version is: {}", current_version);

    let release_url = "https://api.github.com/repos/nicolaschan/insanity/releases/latest";
    let client = reqwest::Client::new();
    let random_user_agent_string = uuid::Uuid::new_v4().to_string();
    let latest_release_response = client
        .get(release_url)
        .header(
            "User-Agent",
            format!("insanity-updater-{}", random_user_agent_string),
        )
        .send()
        .await?;

    if !latest_release_response.status().is_success() {
        return Err(anyhow::anyhow!(
            "Failed to fetch latest release: {}",
            latest_release_response.status()
        ));
    }

    let latest_release = latest_release_response.json::<serde_json::Value>().await?;

    let new_version = latest_release["tag_name"]
        .as_str()
        .expect("tag_name is not a string");

    info!("Found latest release version: {}", new_version,);

    if !force && new_version == current_version {
        info!("Already up to date");
        return Ok(());
    }

    let assets = latest_release["assets"]
        .as_array()
        .expect("no assets in latest release");

    info!("Found {} release assets", assets.len());

    let current_platform =
        get_current_platform().expect("This platform does not support updating through the cli");
    info!("Current platform: {:?}", current_platform);

    let current_platform_asset_name = current_platform.get_asset_name();

    let asset_for_current_platform = assets.iter().find(|asset| {
        asset["name"]
            .as_str()
            .expect("asset name is not a string")
            .starts_with(&current_platform_asset_name)
    });

    let asset_download_url = asset_for_current_platform.expect("no asset for current platform")
        ["browser_download_url"]
        .as_str()
        .expect("asset download url is not a string");

    info!(
        "Found download URL for current platform: {}",
        asset_download_url
    );

    let content_length_response = client.head(asset_download_url).send().await?;

    let content_length: u64 = content_length_response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .expect("no content length header")
        .to_str()
        .unwrap()
        .parse()?;

    info!("Expected content length: {:?}", content_length);

    let temp_dir = tempfile::tempdir()?;
    let temp_file_path = temp_dir.path().join(&current_platform_asset_name);

    info!("Downloading to {}", temp_file_path.display());

    let mut download_response = client.get(asset_download_url).send().await?;
    let mut dest = tokio::fs::File::create(&temp_file_path).await?;

    let pb = indicatif::ProgressBar::new(content_length);

    while let Some(chunk) = download_response.chunk().await? {
        dest.write_all(&chunk).await?;
        pb.set_position(pb.position() + chunk.len() as u64);
    }

    pb.finish();

    let extract_path = temp_dir.path().join("extracted");
    let zip_file = std::fs::File::open(&temp_file_path)?;
    let mut archive = zip::ZipArchive::new(zip_file)?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let outpath = extract_path.join(file.name());

        if file.name().ends_with('/') {
            create_dir_all(&outpath).await?;
        } else {
            if let Some(p) = outpath.parent() {
                if !p.exists() {
                    create_dir_all(p).await?;
                }
            }
            let mut outfile = std::fs::File::create(&outpath)?;
            std::io::copy(&mut file, &mut outfile)?;
        }
    }

    info!("Extracted to {}", extract_path.display());

    let new_exe_dir = extract_path.join(&current_platform_asset_name);
    let new_exe_dir_contents = std::fs::read_dir(&new_exe_dir)?;

    let new_exe_path = new_exe_dir_contents
        .into_iter()
        .find_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            info!("Checking path: {}", path.display());
            if path.is_file()
                && path
                    .file_name()
                    .and_then(|f| f.to_str())
                    .map(|f| f.starts_with("insanity"))
                    .unwrap_or(false)
            {
                Some(path)
            } else {
                None
            }
        })
        .expect("no exe file found in extracted directory");

    info!("New executable path: {}", new_exe_path.display());

    let current_exe = std::env::current_exe()?;
    info!("Current executable: {}", current_exe.display());

    // Move the new executable to the current executable's location
    if !dry_run {
        info!(
            "Replacing {} with {}",
            current_exe.display(),
            new_exe_path.display(),
        );
        tokio::fs::rename(&new_exe_path, &current_exe).await?;
    }

    #[cfg(unix)]
    {
        let current_exe_file = tokio::fs::File::open(&current_exe).await?;
        let mut perms = current_exe_file.metadata().await?.permissions();
        perms.set_mode(perms.mode() | 0o111); // Add execute permission
        match tokio::fs::set_permissions(&current_exe, perms).await {
            Ok(_) => info!("Set execute permission on {}", current_exe.display()),
            Err(e) => warn!(
                "Failed to set execute permission on {}: {}",
                current_exe.display(),
                e
            ),
        }
    }

    info!(
        "Updated version {} -> {} complete",
        current_version, new_version
    );
    Ok(())
}

#[allow(dead_code)]
#[derive(Debug)]
enum UpdatablePlatform {
    LinuxGNU,
    LinuxMusl,
    WindowsMSVC,
    WindowsMingw,
    MacOSAppleSilicon,
}

impl UpdatablePlatform {
    fn get_asset_name(&self) -> String {
        match self {
            UpdatablePlatform::LinuxGNU => "insanity-linux-gnu",
            UpdatablePlatform::LinuxMusl => "insanity-linux-musl",
            UpdatablePlatform::WindowsMSVC => "insanity-windows-msvc",
            UpdatablePlatform::WindowsMingw => "insanity-windows-mingw",
            UpdatablePlatform::MacOSAppleSilicon => "insanity-macos-apple-silicon",
        }
        .to_string()
    }
}

#[allow(unreachable_code)]
fn get_current_platform() -> Option<UpdatablePlatform> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        return Some(UpdatablePlatform::LinuxGNU);
    }

    #[cfg(all(target_os = "linux", target_env = "musl"))]
    {
        return Some(UpdatablePlatform::LinuxMusl);
    }

    #[cfg(all(target_os = "windows", target_env = "msvc"))]
    {
        return Some(UpdatablePlatform::WindowsMSVC);
    }

    #[cfg(all(target_os = "windows", target_env = "gnu"))]
    {
        return Some(UpdatablePlatform::WindowsMingw);
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Some(UpdatablePlatform::MacOSAppleSilicon);
    }

    None
}
