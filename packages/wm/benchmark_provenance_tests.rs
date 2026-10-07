use std::collections::BTreeMap;

use super::provenance::{
  validate_build_stamp, verify_inputs, BuildProvenance, InputSnapshot,
};

#[test]
fn snapshot_covers_repository_build_inputs_and_untracked_helper_sources() {
  let root =
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
  let snapshot = super::provenance::capture_inputs(&root)
    .expect("capture repository inputs");
  for path in [
    "packages/wm/src/benchmarks.rs",
    "packages/wm/benchmark_provenance.rs",
    "Cargo.lock",
    "Taskfile.yml",
    "docs/benchmarks.md",
    "resources/assets/icon.ico",
  ] {
    assert!(
      snapshot.inputs.contains_key(path),
      "missing covered input {path}"
    );
  }
  assert_ne!(snapshot.head, "");
}

#[test]
fn missing_and_wrong_mode_stamps_are_rejected() {
  let stamp = BuildProvenance {
    schema_version: super::provenance::SCHEMA_VERSION,
    enabled: true,
    mode: "headline".into(),
    snapshot: Some(InputSnapshot {
      head: "head".into(),
      index_sha256: "index".into(),
      input_sha256: "inputs".into(),
      inputs: BTreeMap::new(),
      changes: Vec::new(),
      settings: BTreeMap::from([(
        "build-only:PROFILE".into(),
        "release".into(),
      )]),
    }),
    rustc: "rustc".into(),
    cargo: "cargo".into(),
    contract: "conditional".into(),
  };
  assert!(validate_build_stamp(&stamp, "headline").is_ok());
  assert!(validate_build_stamp(&stamp, "allocations").is_err());
  let mut missing = stamp.clone();
  missing.enabled = false;
  assert!(validate_build_stamp(&missing, "headline").is_err());
  let mut debug = stamp;
  debug
    .snapshot
    .as_mut()
    .expect("test snapshot")
    .settings
    .insert("build-only:PROFILE".into(), "debug".into());
  assert!(validate_build_stamp(&debug, "headline").is_err());
}

#[test]
fn cargo_config_candidates_and_rerun_paths_include_absent_files_and_parents(
) {
  let project = std::path::Path::new("C:/benchmark/project");
  let cargo_home = std::path::Path::new("C:/benchmark/cargo-home");
  let candidates =
    super::provenance::cargo_config_paths(project, Some(cargo_home));
  let missing = project.join(".cargo/config.toml");
  assert!(candidates.contains(&missing));
  assert!(candidates.contains(&cargo_home.join("config.toml")));
  let snapshot = InputSnapshot {
    head: String::new(),
    index_sha256: String::new(),
    input_sha256: String::new(),
    inputs: BTreeMap::from([(
      format!("@cargo-config:{}", missing.display()),
      "<missing>".into(),
    )]),
    changes: Vec::new(),
    settings: BTreeMap::new(),
  };
  let rerun_paths = super::provenance::cargo_config_rerun_paths(&snapshot);
  assert!(rerun_paths.iter().any(|path| path == &missing));
  let parent = missing.parent().expect("config parent");
  let grandparent = parent.parent().expect("config grandparent");
  assert!(rerun_paths.iter().any(|path| path == parent));
  assert!(rerun_paths.iter().any(|path| path == grandparent));
}

#[test]
fn provenance_comparison_rejects_any_boundary_change() {
  let base = InputSnapshot {
    head: "head".into(),
    index_sha256: "index".into(),
    input_sha256: "inputs".into(),
    inputs: BTreeMap::from([(
      "packages/wm/src/main.rs".into(),
      "hash".into(),
    )]),
    changes: vec![" M file".into()],
    settings: BTreeMap::from([("RUSTFLAGS".into(), String::new())]),
  };
  assert!(verify_inputs(&base, &base).is_ok());
  let mut changed = base.clone();
  changed
    .inputs
    .insert("packages/wm/src/main.rs".into(), "changed".into());
  assert!(verify_inputs(&base, &changed).is_err());
  let mut staged_only = base.clone();
  staged_only.index_sha256 = "new-index".into();
  assert!(verify_inputs(&base, &staged_only).is_err());
  let mut configuration = base.clone();
  configuration
    .inputs
    .insert("@cargo-config:config.toml".into(), "changed".into());
  assert!(verify_inputs(&base, &configuration).is_err());
  let mut changed_operator_setting = base.clone();
  changed_operator_setting
    .settings
    .insert("VERSION_NUMBER".into(), "9.9.9".into());
  assert!(verify_inputs(&base, &changed_operator_setting).is_err());
  let mut changed_cargo_build_setting = base.clone();
  changed_cargo_build_setting
    .settings
    .insert("build-only:TARGET".into(), "different-target".into());
  assert!(verify_inputs(&base, &changed_cargo_build_setting).is_ok());
}
