use std::{
  collections::BTreeMap,
  fs,
  path::{Path, PathBuf},
  process::Command,
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const SCHEMA_VERSION: u32 = 3;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputSnapshot {
  pub head: String,
  pub index_sha256: String,
  pub input_sha256: String,
  pub inputs: BTreeMap<String, String>,
  pub changes: Vec<String>,
  pub settings: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BuildProvenance {
  pub schema_version: u32,
  pub enabled: bool,
  pub mode: String,
  pub snapshot: Option<InputSnapshot>,
  pub rustc: String,
  pub cargo: String,
  pub contract: String,
}

/// Captures covered repository inputs and semantic Git state.
#[allow(clippy::too_many_lines)]
pub fn capture_inputs(root: &Path) -> Result<InputSnapshot> {
  let head = git(root, &["rev-parse", "HEAD"])?;
  let index_raw = git_bytes(root, &["ls-files", "--stage", "-z"])?;
  let index_sha256 = hash(&index_raw);
  let mut paths = BTreeMap::<String, String>::new();
  let head_paths =
    git_bytes(root, &["ls-tree", "-r", "--name-only", "-z", "HEAD"])?;
  for item in head_paths
    .split(|byte| *byte == 0)
    .filter(|item| !item.is_empty())
  {
    let path = std::str::from_utf8(item)
      .context("unsupported non-UTF8 HEAD path")?;
    paths.insert(path.to_owned(), String::new());
  }
  let tracked = git_bytes(root, &["ls-files", "-z"])?;
  for item in tracked
    .split(|byte| *byte == 0)
    .filter(|item| !item.is_empty())
  {
    let path = std::str::from_utf8(item)
      .context("unsupported non-UTF8 repository path")?;
    paths.insert(path.to_owned(), String::new());
  }
  let untracked = git_bytes(
    root,
    &["ls-files", "--others", "--exclude-standard", "-z"],
  )?;
  for item in untracked
    .split(|byte| *byte == 0)
    .filter(|item| !item.is_empty())
  {
    let path = std::str::from_utf8(item)
      .context("unsupported non-UTF8 repository path")?;
    paths.insert(path.to_owned(), String::new());
  }
  let explicit = [
    "Cargo.toml",
    "Cargo.lock",
    "Taskfile.yml",
    "docs/benchmarks.md",
    "rust-toolchain.toml",
  ];
  for path in explicit {
    paths.entry(path.into()).or_default();
  }
  // Walk build roots so ignored source/config files and new files are
  // covered too.
  for directory in ["packages", "resources/assets", ".cargo"] {
    collect_files(root, &root.join(directory), &mut paths)?;
  }
  let mut inputs = BTreeMap::new();
  for relative in paths.keys() {
    if !covered(relative) {
      continue;
    }
    let path = root.join(relative);
    if !path.exists() {
      inputs.insert(relative.clone(), "<deleted>".into());
      continue;
    }
    let metadata = fs::symlink_metadata(&path)
      .with_context(|| format!("stat {}", path.display()))?;
    if metadata.file_type().is_symlink() {
      bail!("covered symlink is unsupported: {}", path.display());
    }
    if metadata.is_file() {
      inputs.insert(
        relative.clone(),
        hash(
          &fs::read(&path)
            .with_context(|| format!("read {}", path.display()))?,
        ),
      );
    }
  }
  // Track present and absent Cargo configs so additions/deletions rerun
  // builds.
  let cargo_home = std::env::var_os("CARGO_HOME")
    .map(PathBuf::from)
    .or_else(|| {
      std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|home| PathBuf::from(home).join(".cargo"))
    });
  for path in cargo_config_paths(root, cargo_home.as_deref()) {
    let name = format!("@cargo-config:{}", path.display());
    let fingerprint = if path.is_file() {
      hash(
        &fs::read(&path)
          .with_context(|| format!("read {}", path.display()))?,
      )
    } else {
      "<missing>".into()
    };
    inputs.insert(name, fingerprint);
  }
  let input_sha256 = hash_map(&inputs);
  let mut settings = BTreeMap::new();
  for key in [
    "CARGO_PROFILE_RELEASE_OPT_LEVEL",
    "CARGO_PROFILE_RELEASE_DEBUG",
    "CARGO_PROFILE_RELEASE_LTO",
    "CARGO_PROFILE_RELEASE_CODEGEN_UNITS",
    "CARGO_PROFILE_RELEASE_PANIC",
    "CARGO_PROFILE_RELEASE_STRIP",
    "CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS",
    "CARGO_BUILD_TARGET",
    "CARGO_BUILD_RUSTFLAGS",
    "GLAZEWM_OPERATOR_RUSTFLAGS",
    "GLAZEWM_OPERATOR_CARGO_ENCODED_RUSTFLAGS",
    "GLAZEWM_OPERATOR_RUSTC",
    "GLAZEWM_OPERATOR_RUSTC_WRAPPER",
    "GLAZEWM_OPERATOR_RUSTC_WORKSPACE_WRAPPER",
    "GLAZEWM_OPERATOR_VERSION_NUMBER",
    "GLAZEWM_BENCHMARK_PROVENANCE",
    "GLAZEWM_BENCHMARK_MODE",
  ] {
    settings.insert(key.into(), std::env::var(key).unwrap_or_default());
  }
  for key in [
    "PROFILE",
    "HOST",
    "TARGET",
    "OPT_LEVEL",
    "DEBUG",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTFLAGS",
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "VERSION_NUMBER",
    "CARGO_FEATURE_UI_ACCESS",
    "CARGO_FEATURE_BENCHMARK_ALLOCATIONS",
    "CARGO_CFG_TARGET_OS",
    "CARGO_CFG_TARGET_ARCH",
    "CARGO_CFG_TARGET_ENV",
    "CARGO_CFG_TARGET_ENDIAN",
    "CARGO_CFG_TARGET_POINTER_WIDTH",
    "CARGO_CFG_DEBUG_ASSERTIONS",
  ] {
    settings.insert(
      format!("build-only:{key}"),
      std::env::var(key).unwrap_or_default(),
    );
  }
  let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
  let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
  settings.insert(
    "build-only:rustc-version".into(),
    command(rustc, &["--version", "--verbose"])?,
  );
  settings.insert(
    "build-only:cargo-version".into(),
    command(cargo, &["--version"])?,
  );
  let status = git_bytes(
    root,
    &["status", "--porcelain=v1", "--untracked-files=all", "-z"],
  )?;
  let changes = String::from_utf8_lossy(&status)
    .split('\0')
    .filter(|s| !s.is_empty())
    .map(str::to_owned)
    .collect::<Vec<_>>();
  if changes.iter().any(|change| {
    let status = change.as_bytes().get(..2).unwrap_or_default();
    status.contains(&b'U') || status == b"AA" || status == b"DD"
  }) {
    bail!("unresolved Git conflicts are not supported for benchmark provenance");
  }
  Ok(InputSnapshot {
    head,
    index_sha256,
    input_sha256,
    inputs,
    changes,
    settings,
  })
}

/// Rejects missing stamps and allocator-mode mismatches.
pub fn validate_build_stamp<'a>(
  provenance: &'a BuildProvenance,
  expected_mode: &str,
) -> Result<&'a InputSnapshot> {
  anyhow::ensure!(
    provenance.enabled && provenance.schema_version == SCHEMA_VERSION,
    "missing or unsupported benchmark build stamp"
  );
  anyhow::ensure!(
    provenance.mode == expected_mode,
    "benchmark stamp mode does not match harness mode"
  );
  let snapshot = provenance
    .snapshot
    .as_ref()
    .context("benchmark stamp lacks snapshot")?;
  anyhow::ensure!(
    snapshot
      .settings
      .get("build-only:PROFILE")
      .is_some_and(|profile| profile == "release"),
    "benchmark evidence requires a Cargo release profile"
  );
  Ok(snapshot)
}

fn operator_settings(
  settings: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
  settings
    .iter()
    .filter(|(key, _)| !key.starts_with("build-only:"))
    .map(|(key, value)| (key.clone(), value.clone()))
    .collect()
}

/// Rejects any difference between two snapshots.
pub fn verify_inputs(
  expected: &InputSnapshot,
  actual: &InputSnapshot,
) -> Result<()> {
  if expected.head != actual.head
    || expected.index_sha256 != actual.index_sha256
    || expected.input_sha256 != actual.input_sha256
    || expected.inputs != actual.inputs
    || expected.changes != actual.changes
    || operator_settings(&expected.settings)
      != operator_settings(&actual.settings)
  {
    bail!("covered build inputs changed since provenance capture");
  }
  Ok(())
}

/// Hashes a file using SHA-256.
pub fn sha256_file(path: &Path) -> Result<String> {
  Ok(hash(
    &fs::read(path).with_context(|| format!("read {}", path.display()))?,
  ))
}

/// Lists every Cargo config candidate from the repository ancestry and
/// Cargo home.
pub fn cargo_config_paths(
  root: &Path,
  cargo_home: Option<&Path>,
) -> Vec<PathBuf> {
  let mut paths = Vec::new();
  let mut ancestor = Some(root);
  while let Some(directory) = ancestor {
    paths.extend([
      directory.join(".cargo/config"),
      directory.join(".cargo/config.toml"),
    ]);
    ancestor = directory.parent();
  }
  if let Some(home) = cargo_home {
    paths.extend([home.join("config"), home.join("config.toml")]);
  }
  paths.sort();
  paths.dedup();
  paths
}

/// Lists Cargo rerun paths that detect config edits and config-file
/// additions/deletions.
pub fn cargo_config_rerun_paths(snapshot: &InputSnapshot) -> Vec<PathBuf> {
  let mut paths = Vec::new();
  for entry in snapshot
    .inputs
    .keys()
    .filter_map(|key| key.strip_prefix("@cargo-config:"))
  {
    let config = PathBuf::from(entry);
    paths.push(config.clone());
    if let Some(parent) = config.parent() {
      paths.push(parent.to_path_buf());
      if let Some(grandparent) = parent.parent() {
        paths.push(grandparent.to_path_buf());
      }
    }
  }
  paths.sort();
  paths.dedup();
  paths
}

fn collect_files(
  root: &Path,
  directory: &Path,
  paths: &mut BTreeMap<String, String>,
) -> Result<()> {
  if !directory.exists() {
    return Ok(());
  }
  for entry in fs::read_dir(directory)
    .with_context(|| format!("read directory {}", directory.display()))?
  {
    let entry = entry?;
    let path = entry.path();
    let metadata = fs::symlink_metadata(&path)?;
    if metadata.file_type().is_symlink() {
      bail!("covered symlink is unsupported: {}", path.display());
    }
    if metadata.is_dir() {
      if matches!(entry.file_name().to_str(), Some("target" | "out")) {
        continue;
      }
      collect_files(root, &path, paths)?;
    } else if metadata.is_file() {
      let relative = path
        .strip_prefix(root)
        .context("covered path escaped repository")?
        .to_str()
        .context("unsupported non-UTF8 repository path")?
        .replace('\\', "/");
      if covered(&relative) {
        paths.entry(relative).or_default();
      }
    }
  }
  Ok(())
}
fn covered(path: &str) -> bool {
  path.starts_with("packages/")
    && !path
      .split('/')
      .any(|part| part == "target" || part == "out")
    || path.starts_with("resources/assets/")
    || path.starts_with(".cargo/")
    || matches!(
      path,
      "Cargo.toml"
        | "Cargo.lock"
        | "Taskfile.yml"
        | "docs/benchmarks.md"
        | "rust-toolchain.toml"
    )
}
fn hash(bytes: &[u8]) -> String {
  format!("{:x}", Sha256::digest(bytes))
}
fn hash_map(values: &BTreeMap<String, String>) -> String {
  let mut hasher = Sha256::new();
  for (key, value) in values {
    hasher.update((key.len() as u64).to_le_bytes());
    hasher.update(key.as_bytes());
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
  }
  format!("{:x}", hasher.finalize())
}
fn git(root: &Path, args: &[&str]) -> Result<String> {
  String::from_utf8(git_bytes(root, args)?)
    .context("git output is not UTF-8")
}
fn git_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
  let output = Command::new("git")
    .args(args)
    .current_dir(root)
    .output()
    .context("run git")?;
  if !output.status.success() {
    bail!(
      "git {:?} failed: {}",
      args,
      String::from_utf8_lossy(&output.stderr)
    );
  }
  Ok(output.stdout)
}
fn command(
  program: impl AsRef<std::ffi::OsStr>,
  args: &[&str],
) -> Result<String> {
  let program = program.as_ref();
  let label = program.to_string_lossy();
  let output = Command::new(program)
    .args(args)
    .output()
    .with_context(|| format!("run {label}"))?;
  if !output.status.success() {
    bail!("{label} {:?} failed", args);
  }
  Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}
