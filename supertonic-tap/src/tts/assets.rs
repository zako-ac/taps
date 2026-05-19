use anyhow::{Context, Result, bail};
use std::path::Path;
use std::process::Command;

/// Ensure the Supertonic model directory is present.
///
/// `model_dir` is the unpacked HuggingFace repo root — it should contain
/// `onnx/tts.json` and `voice_styles/*.json` after the clone. If
/// `model_dir/onnx/tts.json` already exists, this is a no-op (typical inside
/// containers that bake assets in at build time). Otherwise the HF repo is
/// cloned into `model_dir`, which must not already exist or must be empty.
///
/// Requires `git` and `git-lfs` on PATH.
pub fn ensure_assets(model_dir: &Path, hf_repo: &str) -> Result<()> {
    if model_dir.join("onnx").join("tts.json").exists() {
        tracing::info!(
            model_dir = %model_dir.display(),
            "supertonic assets already present"
        );
        return Ok(());
    }

    if model_dir.exists() {
        let is_empty = std::fs::read_dir(model_dir)
            .with_context(|| format!("failed to read {}", model_dir.display()))?
            .next()
            .is_none();
        if !is_empty {
            bail!(
                "SUPERTONIC_MODEL_DIR={} already exists and is not empty but does not \
                 contain onnx/tts.json — delete it or point SUPERTONIC_MODEL_DIR \
                 somewhere fresh",
                model_dir.display()
            );
        }
    } else if let Some(parent) = model_dir.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
    }

    tracing::info!(
        repo = hf_repo,
        target = %model_dir.display(),
        "cloning supertonic assets from HuggingFace"
    );

    let status = Command::new("git")
        .args(["clone", "--depth=1", hf_repo])
        .arg(model_dir)
        .status()
        .context("failed to invoke git — is git installed?")?;
    if !status.success() {
        bail!("git clone of {hf_repo} failed with status {status}");
    }

    let lfs_status = Command::new("git")
        .args(["-C"])
        .arg(model_dir)
        .args(["lfs", "pull"])
        .status()
        .context("failed to invoke git lfs — is git-lfs installed?")?;
    if !lfs_status.success() {
        bail!("git lfs pull failed with status {lfs_status}");
    }

    if !model_dir.join("onnx").join("tts.json").exists() {
        bail!(
            "expected {} to exist after clone — the HF repo layout may have changed",
            model_dir.join("onnx").join("tts.json").display()
        );
    }

    tracing::info!("supertonic assets ready");
    Ok(())
}
