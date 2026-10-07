use tauri_winres::VersionInfo;

#[allow(dead_code)]
#[path = "benchmark_provenance.rs"]
mod benchmark_provenance;

fn main() {
  println!("cargo:rerun-if-env-changed=VERSION_NUMBER");
  println!("cargo:rerun-if-env-changed=GLAZEWM_BENCHMARK_PROVENANCE");
  println!("cargo:rerun-if-env-changed=GLAZEWM_BENCHMARK_MODE");
  write_benchmark_stamp();
  let mut res = tauri_winres::WindowsResource::new();

  // When the `ui_access` feature is enabled, the `uiAccess` attribute is
  // set to `true`. UIAccess is disabled by default because it requires the
  // application to be signed and installed in a secure location.
  let ui_access = {
    #[cfg(feature = "ui_access")]
    {
      "true"
    }
    #[cfg(not(feature = "ui_access"))]
    {
      "false"
    }
  };

  // Conditionally enable UIAccess, which grants privilege to set the
  // foreground window and to set the position of elevated windows.
  //
  // Ref: https://learn.microsoft.com/en-us/previous-versions/windows/it-pro/windows-10/security/threat-protection/security-policy-settings/user-account-control-only-elevate-uiaccess-applications-that-are-installed-in-secure-locations
  //
  // Additionally, declare support for per-monitor DPI awareness.
  let manifest_str = format!(
    r#"
<assembly
  xmlns="urn:schemas-microsoft-com:asm.v1"
  manifestVersion="1.0"
  xmlns:asmv3="urn:schemas-microsoft-com:asm.v3"
>
  <asmv3:trustInfo>
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="{ui_access}" />
      </requestedPrivileges>
    </security>
  </asmv3:trustInfo>

  <asmv3:application>
    <windowsSettings
      xmlns:ws2005="http://schemas.microsoft.com/SMI/2005/WindowsSettings"
      xmlns:ws2016="http://schemas.microsoft.com/SMI/2016/WindowsSettings"
    >
      <ws2005:dpiAware>true</ws2005:dpiAware>
      <ws2016:dpiAwareness>PerMonitorV2</ws2016:dpiAwareness>
    </windowsSettings>
  </asmv3:application>
</assembly>
"#
  );

  res.set_manifest(&manifest_str);
  res.set_icon("../../resources/assets/icon.ico");

  // Set language to English (US).
  res.set_language(0x0409);

  res.set("OriginalFilename", "glazewm.exe");
  res.set("ProductName", "GlazeWM");
  res.set("FileDescription", "GlazeWM");

  let version_parts = env!("VERSION_NUMBER")
    .split('.')
    .take(3)
    .map(|part| part.parse().unwrap_or(0))
    .collect::<Vec<u16>>();

  let [major, minor, patch] =
    <[u16; 3]>::try_from(version_parts).unwrap_or([0, 0, 0]);

  let version_str = format!("{major}.{minor}.{patch}.0");
  res.set("FileVersion", &version_str);
  res.set("ProductVersion", &version_str);

  let version_u64 = (u64::from(major) << 48)
    | (u64::from(minor) << 32)
    | (u64::from(patch) << 16);

  res.set_version_info(VersionInfo::FILEVERSION, version_u64);
  res.set_version_info(VersionInfo::PRODUCTVERSION, version_u64);

  res.compile().unwrap();
}

fn write_benchmark_stamp() {
  use std::{fs, path::PathBuf};
  let enabled =
    std::env::var("GLAZEWM_BENCHMARK_PROVENANCE").as_deref() == Ok("1");
  let mode = std::env::var("GLAZEWM_BENCHMARK_MODE").unwrap_or_default();
  let root = PathBuf::from(
    std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest dir"),
  )
  .join("../..");
  let snapshot = if enabled {
    // Shared inputs are intentionally captured at the evidence build
    // boundary.
    let snapshot = benchmark_provenance::capture_inputs(&root)
      .expect("capture benchmark build provenance");
    for path in snapshot.inputs.keys() {
      if !path.starts_with('@') {
        println!("cargo:rerun-if-changed={}", root.join(path).display());
      }
    }
    for path in benchmark_provenance::cargo_config_rerun_paths(&snapshot) {
      println!("cargo:rerun-if-changed={}", path.display());
    }
    for directory in ["packages", "resources/assets", ".cargo"] {
      println!(
        "cargo:rerun-if-changed={}",
        root.join(directory).display()
      );
    }
    for git_path in ["index", "HEAD", "refs"] {
      if let Ok(path) = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", git_path])
        .current_dir(&root)
        .output()
      {
        if path.status.success() {
          println!(
            "cargo:rerun-if-changed={}",
            root
              .join(String::from_utf8_lossy(&path.stdout).trim())
              .display()
          );
        }
      }
    }
    Some(snapshot)
  } else {
    None
  };
  let rustc = snapshot
    .as_ref()
    .and_then(|value| value.settings.get("build-only:rustc-version"))
    .cloned()
    .unwrap_or_else(|| "not captured for ordinary builds".into());
  let cargo = snapshot
    .as_ref()
    .and_then(|value| value.settings.get("build-only:cargo-version"))
    .cloned()
    .unwrap_or_else(|| "not captured for ordinary builds".into());
  let provenance = benchmark_provenance::BuildProvenance {
    schema_version: benchmark_provenance::SCHEMA_VERSION,
    enabled,
    mode,
    snapshot,
    rustc,
    cargo,
    contract: "conditional provenance; no concurrent covered edits during normal Cargo compilation and measurement; registry sources, toolchain and SDK are trusted; transient edit-and-revert is not detected".into(),
  };
  let output =
    PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo output dir"))
      .join("benchmark-provenance.json");
  fs::write(
    output,
    serde_json::to_vec(&provenance)
      .expect("serialize benchmark provenance"),
  )
  .expect("write benchmark provenance stamp");
}
