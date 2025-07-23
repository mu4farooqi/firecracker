use anyhow::{Context, Result};
use bollard::container::{CreateContainerOptions, Config as ContainerConfig};
use bollard::image::{CreateImageOptions, InspectImageOptions};
use bollard::Docker;
use chrono::{DateTime, Utc};
use clap::{Arg, Command};
use digest::Digest;
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, Permissions};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tar::Archive;
use tempfile::TempDir;
use walkdir::WalkDir;

#[derive(Debug, Serialize, Deserialize)]
struct ImageConfig {
    #[serde(rename = "Cmd")]
    cmd: Option<Vec<String>>,
    #[serde(rename = "Entrypoint")]
    entrypoint: Option<Vec<String>>,
    #[serde(rename = "Env")]
    env: Option<Vec<String>>,
    #[serde(rename = "WorkingDir")]
    working_dir: Option<String>,
    #[serde(rename = "User")]
    user: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ImageManifest {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "Layers")]
    layers: Vec<String>,
    #[serde(rename = "RepoTags")]
    repo_tags: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
struct FileEntry {
    path: String,
    content: Vec<u8>,
    mode: u32,
    uid: u32,
    gid: u32,
    mtime: i64,
    is_dir: bool,
    is_symlink: bool,
    symlink_target: Option<String>,
}

#[derive(Debug)]
struct LayerInfo {
    digest: String,
    is_base_layer: bool,
}

const DETERMINISTIC_TIMESTAMP: i64 = 1640995200; // 2022-01-01 00:00:00 UTC

#[tokio::main]
async fn main() -> Result<()> {
    let matches = Command::new("image-extractor")
        .about("Extract Docker image layers into deterministic file systems")
        .arg(
            Arg::new("image")
                .help("Docker image name (e.g., myapp:latest)")
                .required(true)
                .index(1),
        )
        .arg(
            Arg::new("base-image")
                .long("base-image")
                .help("Base image pattern (e.g., rowerdev/dev-base)")
                .required(true),
        )
        .arg(
            Arg::new("output-dir")
                .long("output-dir")
                .help("Output directory for extracted file systems")
                .default_value("./extracted"),
        )
        .arg(
            Arg::new("chunk-size")
                .long("chunk-size")
                .help("Chunk size in KB for block-level operations")
                .default_value("512"),
        )
        .get_matches();

    let image_name = matches.get_one::<String>("image").unwrap();
    let base_image_pattern = matches.get_one::<String>("base-image").unwrap();
    let output_dir = matches.get_one::<String>("output-dir").unwrap();
    let chunk_size: usize = matches
        .get_one::<String>("chunk-size")
        .unwrap()
        .parse()
        .context("Invalid chunk size")?
        * 1024;

    let docker = Docker::connect_with_socket_defaults()
        .context("Failed to connect to Docker daemon")?;

    println!("Pulling image: {}", image_name);
    pull_image(&docker, image_name).await?;

    println!("Inspecting image...");
    let (layers, config) = inspect_image(&docker, image_name).await?;

    println!("Identifying base and additional layers...");
    let layer_info = identify_layers(&docker, &layers, base_image_pattern).await?;

    println!("Extracting layers...");
    let temp_dir = TempDir::new().context("Failed to create temp directory")?;
    let (base_files, additional_files) =
        extract_layers(&docker, image_name, &layer_info, &temp_dir).await?;

    println!("Creating deterministic file systems...");
    let output_path = Path::new(output_dir);
    fs::create_dir_all(output_path).context("Failed to create output directory")?;

    create_deterministic_filesystem(&base_files, &output_path.join("base_fs")).await?;
    create_deterministic_filesystem(&additional_files, &output_path.join("additional_fs")).await?;

    println!("Creating entrypoint script...");
    create_entrypoint_script(&config, &output_path.join("entrypoint.sh")).await?;

    println!("Creating block-level chunks...");
    create_block_chunks(&output_path.join("base_fs"), &output_path.join("base_chunks"), chunk_size).await?;
    create_block_chunks(&output_path.join("additional_fs"), &output_path.join("additional_chunks"), chunk_size).await?;

    println!("Extraction completed successfully!");
    println!("Base filesystem: {}/base_fs", output_dir);
    println!("Additional filesystem: {}/additional_fs", output_dir);
    println!("Entrypoint script: {}/entrypoint.sh", output_dir);
    println!("Base chunks: {}/base_chunks", output_dir);
    println!("Additional chunks: {}/additional_chunks", output_dir);

    Ok(())
}

async fn pull_image(docker: &Docker, image_name: &str) -> Result<()> {
    let mut stream = docker.create_image(
        Some(CreateImageOptions {
            from_image: image_name,
            ..Default::default()
        }),
        None,
        None,
    );

    use futures_util::stream::StreamExt;
    while let Some(result) = stream.next().await {
        match result {
            Ok(_) => {}, // Progress update
            Err(e) => return Err(anyhow::anyhow!("Failed to pull image: {}", e)),
        }
    }

    Ok(())
}

async fn inspect_image(docker: &Docker, image_name: &str) -> Result<(Vec<String>, ImageConfig)> {
    let image_inspect = docker
        .inspect_image(image_name)
        .await
        .context("Failed to inspect image")?;

    let layers = image_inspect
        .root_fs
        .as_ref()
        .and_then(|fs| fs.layers.as_ref())
        .cloned()
        .unwrap_or_default();

    let config_raw = image_inspect
        .config
        .as_ref()
        .context("Image config not found")?;

    let config = ImageConfig {
        cmd: config_raw.cmd.clone(),
        entrypoint: config_raw.entrypoint.clone(),
        env: config_raw.env.clone(),
        working_dir: config_raw.working_dir.clone(),
        user: config_raw.user.clone(),
    };

    Ok((layers, config))
}

async fn identify_layers(
    docker: &Docker,
    layers: &[String],
    base_image_pattern: &str,
) -> Result<Vec<LayerInfo>> {
    let mut layer_info = Vec::new();
    let mut found_base_end = false;

    // For each layer, we need to determine if it belongs to the base image
    // This is a simplified heuristic - in practice, you might need more sophisticated logic
    for (i, layer) in layers.iter().enumerate() {
        let is_base_layer = if !found_base_end {
            // Try to determine if this layer is from the base image
            // This is a heuristic based on layer history
            let is_likely_base = i < layers.len() - 5; // Assume last 5 layers are likely additional

            if !is_likely_base {
                found_base_end = true;
            }

            is_likely_base
        } else {
            false
        };

        layer_info.push(LayerInfo {
            digest: layer.clone(),
            is_base_layer,
        });
    }

    Ok(layer_info)
}

async fn extract_layers(
    docker: &Docker,
    image_name: &str,
    layer_info: &[LayerInfo],
    temp_dir: &TempDir,
) -> Result<(BTreeMap<String, FileEntry>, BTreeMap<String, FileEntry>)> {
    // Create a temporary container to extract the filesystem
    let container_config = ContainerConfig {
        image: Some(image_name.to_string()),
        cmd: Some(vec!["true".to_string()]), // Dummy command
        ..Default::default()
    };

    let container = docker
        .create_container::<String, String>(None, container_config)
        .await
        .context("Failed to create container")?;

    let container_id = &container.id;

    // Export the container filesystem
    let export_stream = docker.export_container(container_id);

    let export_path = temp_dir.path().join("container_export.tar");
    let mut export_file = File::create(&export_path)?;

    use futures_util::stream::StreamExt;
    let mut stream = export_stream;
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.context("Failed to read export stream")?;
        export_file.write_all(&bytes)?;
    }
    export_file.flush()?;

    // Clean up container
    docker
        .remove_container(container_id, None)
        .await
        .context("Failed to remove container")?;

    // Extract and organize files
    let mut base_files = BTreeMap::new();
    let mut additional_files = BTreeMap::new();

    let export_file = File::open(&export_path)?;
    let mut archive = Archive::new(export_file);

    for entry in archive.entries().context("Failed to read archive entries")? {
        let mut entry = entry.context("Failed to read archive entry")?;

        let path = entry.path().context("Failed to get entry path")?;
        let path_str = path.to_string_lossy().to_string();

        // Skip certain system paths that are not relevant
        if should_skip_path(&path_str) {
            continue;
        }

        let header = entry.header();
        let mut content = Vec::new();
        entry.read_to_end(&mut content)?;

        let file_entry = FileEntry {
            path: path_str.clone(),
            content,
            mode: header.mode().unwrap_or(0o644),
            uid: header.uid().unwrap_or(0) as u32,
            gid: header.gid().unwrap_or(0) as u32,
            mtime: DETERMINISTIC_TIMESTAMP, // Use deterministic timestamp
            is_dir: header.entry_type().is_dir(),
            is_symlink: header.entry_type().is_symlink(),
            symlink_target: if header.entry_type().is_symlink() {
                header.link_name().ok().map(|p| p.to_string_lossy().to_string())
            } else {
                None
            },
        };

        // For simplicity, we'll put system files in base_files and assume
        // application-specific files are in additional_files
        if is_likely_base_file(&path_str) {
            base_files.insert(path_str, file_entry);
        } else {
            additional_files.insert(path_str, file_entry);
        }
    }

    Ok((base_files, additional_files))
}

fn should_skip_path(path: &str) -> bool {
    let skip_patterns = [
        ".dockerenv",
        "/proc/",
        "/sys/",
        "/dev/",
        "/run/",
        "/tmp/",
    ];

    skip_patterns.iter().any(|pattern| path.contains(pattern))
}

fn is_likely_base_file(path: &str) -> bool {
    let base_patterns = [
        "/bin/",
        "/sbin/",
        "/lib/",
        "/lib64/",
        "/usr/bin/",
        "/usr/sbin/",
        "/usr/lib/",
        "/etc/passwd",
        "/etc/group",
        "/etc/shadow",
        "/etc/ld.so.cache",
        "/etc/ssl/",
        "/etc/ca-certificates/",
    ];

    base_patterns.iter().any(|pattern| path.starts_with(pattern))
}

async fn create_deterministic_filesystem(
    files: &BTreeMap<String, FileEntry>,
    output_path: &Path,
) -> Result<()> {
    fs::create_dir_all(output_path).context("Failed to create filesystem directory")?;

    // Sort files by path for deterministic ordering
    let sorted_files: BTreeMap<_, _> = files.iter().collect();

    for (path, file_entry) in sorted_files {
        let full_path = output_path.join(path.trim_start_matches('/'));

        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create parent directory for {}", path))?;
        }

        if file_entry.is_dir {
            fs::create_dir_all(&full_path)
                .with_context(|| format!("Failed to create directory {}", path))?;
        } else if file_entry.is_symlink {
            if let Some(target) = &file_entry.symlink_target {
                std::os::unix::fs::symlink(target, &full_path)
                    .with_context(|| format!("Failed to create symlink {}", path))?;
            }
        } else {
            // Regular file
            let mut file = File::create(&full_path)
                .with_context(|| format!("Failed to create file {}", path))?;
            file.write_all(&file_entry.content)
                .with_context(|| format!("Failed to write file content {}", path))?;
        }

        // Set permissions deterministically
        let permissions = Permissions::from_mode(file_entry.mode);
        fs::set_permissions(&full_path, permissions)
            .with_context(|| format!("Failed to set permissions for {}", path))?;

        // Set ownership (requires root privileges, so we'll skip in practice)
        // unsafe {
        //     libc::chown(
        //         full_path.to_str().unwrap().as_ptr() as *const i8,
        //         file_entry.uid as libc::uid_t,
        //         file_entry.gid as libc::gid_t,
        //     );
        // }
    }

    Ok(())
}

async fn create_entrypoint_script(config: &ImageConfig, output_path: &Path) -> Result<()> {
    let mut script_content = String::new();
    script_content.push_str("#!/bin/bash\n");
    script_content.push_str("# Auto-generated entrypoint script\n\n");

    // Set environment variables
    if let Some(env_vars) = &config.env {
        script_content.push_str("# Environment variables\n");
        for env_var in env_vars {
            script_content.push_str(&format!("export {}\n", env_var));
        }
        script_content.push_str("\n");
    }

    // Set working directory
    if let Some(workdir) = &config.working_dir {
        script_content.push_str(&format!("cd {}\n\n", workdir));
    }

    // Set user (if specified)
    if let Some(user) = &config.user {
        script_content.push_str(&format!("# Run as user: {}\n", user));
    }

    // Build command
    let mut command_parts = Vec::new();

    if let Some(entrypoint) = &config.entrypoint {
        command_parts.extend(entrypoint.iter().cloned());
    }

    if let Some(cmd) = &config.cmd {
        command_parts.extend(cmd.iter().cloned());
    }

    if !command_parts.is_empty() {
        script_content.push_str("# Execute command\n");
        let command = command_parts
            .iter()
            .map(|part| shell_escape(part))
            .collect::<Vec<_>>()
            .join(" ");
        script_content.push_str(&format!("exec {}\n", command));
    } else {
        script_content.push_str("# No command specified\n");
        script_content.push_str("exec /bin/bash\n");
    }

    let mut file = File::create(output_path)?;
    file.write_all(script_content.as_bytes())?;

    // Make script executable
    let permissions = Permissions::from_mode(0o755);
    fs::set_permissions(output_path, permissions)?;

    Ok(())
}

fn shell_escape(s: &str) -> String {
    if s.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/') {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}

async fn create_block_chunks(
    filesystem_path: &Path,
    chunks_dir: &Path,
    chunk_size: usize,
) -> Result<()> {
    fs::create_dir_all(chunks_dir)?;

    // Create a deterministic representation of the filesystem as blocks
    let mut file_blocks = Vec::new();

    // Walk through all files in deterministic order
    for entry in WalkDir::new(filesystem_path)
        .sort_by(|a, b| a.path().cmp(b.path()))
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            let file_content = fs::read(entry.path())?;

            // Split file content into chunks
            for (i, chunk) in file_content.chunks(chunk_size).enumerate() {
                let mut padded_chunk = vec![0u8; chunk_size];
                padded_chunk[..chunk.len()].copy_from_slice(chunk);

                // Create chunk hash for content-addressable storage
                let chunk_hash = Sha256::digest(&padded_chunk);
                let chunk_name = format!("{:x}", chunk_hash);

                let chunk_path = chunks_dir.join(format!("{}.chunk", chunk_name));
                if !chunk_path.exists() {
                    fs::write(&chunk_path, &padded_chunk)?;
                }

                file_blocks.push((
                    entry.path().strip_prefix(filesystem_path)?.to_string_lossy().to_string(),
                    i,
                    chunk_name,
                ));
            }
        }
    }

    // Write block mapping for reconstruction
    let mapping_path = chunks_dir.join("block_mapping.json");
    let mapping_json = serde_json::to_string_pretty(&file_blocks)?;
    fs::write(mapping_path, mapping_json)?;

    Ok(())
}
