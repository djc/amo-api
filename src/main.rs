use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs};

use anyhow::Context;
use clap::Parser;
use jsonwebtoken::{encode, EncodingKey, Header};
use rand::RngExt;
use reqwest::{
    multipart::{Form, Part},
    Client,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use zip::{write::FileOptions, CompressionMethod, ZipWriter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let client = Client::new();

    // Read addon ID from manifest.json
    let manifest = serde_json::from_str::<Manifest>(
        &fs::read_to_string(args.extension.join("manifest.json"))
            .context("failed to read manifest.json")?,
    )
    .context("failed to parse manifest.json")?;
    let addon_id = urlencoding::encode(&manifest.browser_specific_settings.gecko.id);
    let ext_version = urlencoding::encode(&manifest.version);

    let api_key = env::var("AMO_API_KEY").context("AMO_API_KEY environment variable not set")?;
    let api_secret =
        env::var("AMO_API_SECRET").context("AMO_API_SECRET environment variable not set")?;

    // Check if this version already exists on the server
    let version_url = format!(
        "https://addons.mozilla.org/api/v5/addons/addon/{}/versions/v{}/",
        addon_id, ext_version
    );

    let response = client
        .get(&version_url)
        .header(
            "Authorization",
            format!("jwt {}", jwt(&api_key, &api_secret)?),
        )
        .send()
        .await?;

    let mut version = if response.status().is_success() {
        eprintln!(
            "Version {} already exists, polling for signed file...",
            manifest.version
        );
        check_response::<VersionResponse>(response).await?
    } else {
        eprintln!("Version {} does not exist, uploading...", manifest.version);
        upload(&addon_id, &args.extension, &client, &api_key, &api_secret).await?
    };

    // Poll until signed
    loop {
        if let Some(url) = &version.file.url {
            // Download the signed .xpi
            eprintln!("Downloading signed .xpi...");
            let xpi = client
                .get(url)
                .header(
                    "Authorization",
                    format!("jwt {}", jwt(&api_key, &api_secret)?),
                )
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await?;

            fs::write(&args.output, &xpi)?;
            eprintln!("Wrote {}", args.output.display());
            return Ok(());
        }

        eprintln!("Waiting for signing...");
        sleep(Duration::from_secs(5)).await;
        let response = client
            .get(&version_url)
            .header(
                "Authorization",
                format!("jwt {}", jwt(&api_key, &api_secret)?),
            )
            .send()
            .await?;
        version = check_response(response).await?;
    }
}

async fn upload(
    addon_id: &str,
    path: &Path,
    client: &Client,
    api_key: &str,
    api_secret: &str,
) -> anyhow::Result<VersionResponse> {
    // Package the extension directory into a zip
    eprintln!("Packaging extension from {}...", path.display());
    let zip_bytes = package_extension(path)?;

    eprintln!("Uploading to AMO...");
    let response = client
        .post("https://addons.mozilla.org/api/v5/addons/upload/")
        .header(
            "Authorization",
            format!("jwt {}", jwt(api_key, api_secret)?),
        )
        .multipart(
            Form::new().text("channel", "unlisted").part(
                "upload",
                Part::bytes(zip_bytes)
                    .file_name("extension.zip")
                    .mime_str("application/zip")?,
            ),
        )
        .send()
        .await?;
    let mut upload: UploadResponse = check_response(response).await?;

    // Poll until validated
    while !upload.valid {
        eprintln!("Waiting for validation...");
        sleep(Duration::from_secs(3)).await;
        let response = client
            .get(format!(
                "https://addons.mozilla.org/api/v5/addons/upload/{}/",
                upload.uuid
            ))
            .header(
                "Authorization",
                format!("jwt {}", jwt(api_key, api_secret)?),
            )
            .send()
            .await?;
        upload = check_response(response).await?;
    }

    // Create or update the addon with the new version
    eprintln!("Creating version...");
    let response = client
        .put(format!(
            "https://addons.mozilla.org/api/v5/addons/addon/{}/",
            addon_id
        ))
        .header(
            "Authorization",
            format!("jwt {}", jwt(api_key, api_secret)?),
        )
        .json(&CreateAddonRequest {
            version: CreateVersionRequest {
                upload: upload.uuid,
            },
        })
        .send()
        .await?;

    Ok(check_response::<AddonResponse>(response)
        .await?
        .current_version)
}

fn package_extension(dir: &Path) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(dir.is_dir(), "{} is not a directory", dir.display());
    anyhow::ensure!(
        dir.join("manifest.json").exists(),
        "{} does not contain a manifest.json",
        dir.display()
    );

    let buf = Cursor::new(Vec::new());
    let mut zip = ZipWriter::new(buf);
    let options = FileOptions::<()>::default().compression_method(CompressionMethod::Deflated);

    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current)
            .with_context(|| format!("failed to read directory {}", current.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let name = path
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .into_owned();

            if name == ".git" || name.starts_with(".git/") {
                continue;
            }

            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                zip.add_directory(&name, options)?;
                stack.push(path);
            } else if file_type.is_file() {
                zip.start_file(&name, options)?;
                zip.write_all(&fs::read(&path)?)?;
            }
        }
    }

    Ok(zip.finish()?.into_inner())
}

async fn check_response<T: DeserializeOwned>(response: reqwest::Response) -> anyhow::Result<T> {
    let status = response.status();
    let body = response.text().await?;
    if status.is_client_error() || status.is_server_error() {
        anyhow::bail!("{status}\n{body}");
    }

    serde_json::from_str(&body).with_context(|| format!("failed to deserialize response:\n{body}"))
}

fn jwt(api_key: &str, api_secret: &str) -> anyhow::Result<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    Ok(encode(
        &Header::default(),
        &Claims {
            iss: api_key.to_owned(),
            jti: format!("{:032x}", rand::rng().random::<u128>()),
            iat: now,
            exp: now + 60,
        },
        &EncodingKey::from_secret(api_secret.as_bytes()),
    )?)
}

#[derive(Parser)]
#[command(about = "Sign a Firefox extension via the AMO API")]
struct Args {
    /// Path to the unpacked extension directory (must contain manifest.json)
    extension: PathBuf,
    /// Output path for the signed .xpi
    #[arg(short, long, default_value = "signed.xpi")]
    output: PathBuf,
}

#[derive(Deserialize)]
struct Manifest {
    version: String,
    browser_specific_settings: BrowserSpecificSettings,
}

#[derive(Deserialize)]
struct BrowserSpecificSettings {
    gecko: GeckoSettings,
}

#[derive(Deserialize)]
struct GeckoSettings {
    id: String,
}

#[derive(Serialize)]
struct Claims {
    iss: String,
    jti: String,
    iat: u64,
    exp: u64,
}

#[derive(Serialize)]
struct CreateAddonRequest {
    version: CreateVersionRequest,
}

#[derive(Serialize)]
struct CreateVersionRequest {
    upload: String,
}

#[derive(Deserialize)]
struct UploadResponse {
    uuid: String,
    valid: bool,
}

#[derive(Deserialize)]
struct AddonResponse {
    current_version: VersionResponse,
}

#[derive(Deserialize)]
struct VersionResponse {
    file: FileInfo,
}

#[derive(Deserialize)]
struct FileInfo {
    #[serde(default)]
    url: Option<String>,
}
