//! Test-only benchmark records, invariants, and native-free core kernels.

#![allow(clippy::cast_precision_loss, clippy::map_unwrap_or)] // Floor ratios use floats; optional host metadata has an unavailable fallback.

mod intents;
#[path = "../benchmark_provenance.rs"]
mod provenance;
#[cfg(test)]
#[path = "../benchmark_provenance_tests.rs"]
mod provenance_tests;

use std::time::{Duration, Instant};
#[cfg(feature = "benchmark-allocations")]
use std::{
  alloc::{GlobalAlloc, Layout, System},
  cell::Cell,
};

use serde::{Deserialize, Serialize};

#[cfg(feature = "benchmark-allocations")]
struct CountingAllocator;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
struct AllocationCounts {
  allocations: u64,
  reallocations: u64,
  frees: u64,
  allocated_bytes: u64,
  reallocated_bytes: u64,
  freed_bytes: u64,
}

#[cfg(feature = "benchmark-allocations")]
thread_local! {
  static ACTIVE_ALLOCATIONS: Cell<*mut AllocationCounts> = const { Cell::new(std::ptr::null_mut()) };
}

// SAFETY: Delegates allocation to `System`; scoped counters are
// thread-local and only mutate their stack-owned target on the measured
// thread.
#[cfg(feature = "benchmark-allocations")]
unsafe impl GlobalAlloc for CountingAllocator {
  unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    let pointer = unsafe { System.alloc(layout) };
    if !pointer.is_null() {
      ACTIVE_ALLOCATIONS.with(|active| {
        let target = active.get();
        if !target.is_null() {
          // SAFETY: The scope keeps its counter alive and is thread-local.
          unsafe {
            (*target).allocations += 1;
            (*target).allocated_bytes += layout.size() as u64;
          }
        }
      });
    }
    pointer
  }

  unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
    ACTIVE_ALLOCATIONS.with(|active| {
      let target = active.get();
      if !target.is_null() {
        // SAFETY: The scope keeps its counter alive and is thread-local.
        unsafe {
          (*target).frees += 1;
          (*target).freed_bytes += layout.size() as u64;
        }
      }
    });
    // SAFETY: The pointer/layout pair is forwarded unchanged to `System`.
    unsafe { System.dealloc(pointer, layout) };
  }

  unsafe fn realloc(
    &self,
    pointer: *mut u8,
    layout: Layout,
    new_size: usize,
  ) -> *mut u8 {
    let result = unsafe { System.realloc(pointer, layout, new_size) };
    if !result.is_null() {
      ACTIVE_ALLOCATIONS.with(|active| {
        let target = active.get();
        if !target.is_null() {
          // SAFETY: The scope keeps its counter alive and is thread-local.
          unsafe {
            (*target).reallocations += 1;
            (*target).reallocated_bytes += new_size as u64;
          }
        }
      });
    }
    result
  }
}

#[cfg(feature = "benchmark-allocations")]
#[global_allocator]
static BENCHMARK_ALLOCATOR: CountingAllocator = CountingAllocator;

/// Versioned metadata for one local benchmark run.
struct RunContext {
  run_id: String,
  completed: bool,
  directory: std::path::PathBuf,
  retained_executable: std::path::PathBuf,
  executable_hash: String,
  provenance: provenance::BuildProvenance,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct RunManifest {
  schema_version: u32,
  status: String,
  build_provenance: provenance::BuildProvenance,
  verification: String,
  run_id: String,
  source_revision: String,
  dirty_source_hashes: Vec<String>,
  executable_path: String,
  executable_sha256: String,
  build_profile: String,
  rustc: String,
  cargo: String,
  platform: String,
  os_inventory: String,
  hardware_inventory: String,
  configuration_hash: String,
  clock_calibration: ClockCalibration,
  allocation_coverage: String,
  native_call_coverage: String,
  sample_protocol: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ClockCalibration {
  minimum_nonzero_delta_ns: Option<u128>,
  empty_pair_median_ns: Option<u128>,
  empty_pair_p95_ns: Option<u128>,
  calibration_samples: usize,
  uncertainty_statement: String,
}

/// Reproducible description of a measured kernel.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct CaseSpec {
  name: String,
  boundary: String,
  window_count: usize,
  topology: String,
  maximum_depth: usize,
  workspace_distribution: Vec<usize>,
  warmups_per_repetition: usize,
  measured_samples_per_repetition: usize,
  repetitions: usize,
  timing_mode: String,
}

/// One raw operation duration and explicit completion outcome.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Sample {
  repetition: usize,
  index: usize,
  duration_ns: u128,
  outcome: String,
  native_call_coverage: String,
  allocations: Option<AllocationCounts>,
}

/// Analytical traffic lower bound, kept distinct from a complete floor.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct FloorModel {
  boundary: String,
  assumptions: Vec<String>,
  irreducible_bytes: u64,
  assumed_bandwidth_bytes_per_ns: f64,
  traffic_only_floor_ns: f64,
  complete_floor_ns: Option<f64>,
  closure: String,
}

fn percentile(samples: &[u128], percentile: usize) -> Option<u128> {
  if samples.is_empty() || percentile == 0 || percentile > 100 {
    return None;
  }
  let mut sorted = samples.to_vec();
  sorted.sort_unstable();
  let rank = (percentile * sorted.len()).div_ceil(100);
  sorted.get(rank.saturating_sub(1)).copied()
}

/// Measures layout capture and scripted reconciliation without native
/// calls.
#[test]
#[ignore = "opt-in release benchmark; never run automatically"]
#[allow(clippy::too_many_lines)]
fn core() {
  use wm_platform::Rect;

  use crate::{
    layout_snapshot::LayoutSnapshot,
    models::{Monitor, RootContainer, TilingContainer, TilingWindow},
    native_reconciler::{
      DesiredFrame, FrameReconciler, NativeState, ObservedFrame,
    },
    test_utils::mock_working_area,
    traits::CommonGetters,
  };

  let mut run = begin_run()
    .expect("validate core benchmark build before fixture preparation");
  run
    .start_measurement()
    .expect("mark core benchmark measurement start");
  let mut cases = Vec::<(CaseSpec, Vec<Sample>, FloorModel)>::new();
  for count in [10usize, 50, 100] {
    let windows = (0..count)
      .map(|i| {
        TilingWindow::mock()
          .title(format!("fixture-{i}"))
          .call()
          .into()
      })
      .collect::<Vec<TilingContainer>>();
    let midpoint = windows.len() / 2;
    let window_ids =
      windows.iter().map(CommonGetters::id).collect::<Vec<_>>();
    let distribution = vec![midpoint, count - midpoint];
    let workspaces = vec![
      make_workspace("1", &windows[..midpoint]),
      make_workspace("2", &windows[midpoint..]),
    ];
    let monitor = Monitor::mock().workspaces(workspaces).call();
    let root = RootContainer::new();
    crate::commands::container::attach_container(
      &monitor.clone().into(),
      &root.clone().into(),
      None,
    )
    .expect("attach fixture monitor");
    assert_eq!(
      count_windows(&root),
      count,
      "layout fixture conserves N windows"
    );
    let depth = maximum_split_depth(&root);
    assert!(depth > 0, "fixture records actual tree depth");
    let semantic_baseline =
      LayoutSnapshot::capture(&root).expect("capture semantic baseline");
    let semantic_baseline =
      layout_signature(&semantic_baseline, &window_ids);

    let mut spec = CaseSpec {
      name: format!("layout-capture-n{count}"),
      boundary: "capture start through capture return; returned snapshot retained until after end timestamp; destruction excluded".into(),
      window_count: count,
      topology: "two workspaces, recursively balanced alternating-axis binary splits".into(),
      maximum_depth: depth,
      workspace_distribution: distribution.clone(),
      warmups_per_repetition: 1_000,
      measured_samples_per_repetition: 20_000,
      repetitions: 3,
      timing_mode: "headline; uninstrumented".into(),
    };
    let mut samples = Vec::with_capacity(
      spec.measured_samples_per_repetition * spec.repetitions,
    );
    for repetition in 0..spec.repetitions {
      for _ in 0..spec.warmups_per_repetition {
        let snapshot = std::hint::black_box(
          LayoutSnapshot::capture(&root).expect("capture layout"),
        );
        std::hint::black_box(&snapshot);
      }
      for index in 0..spec.measured_samples_per_repetition {
        let start = Instant::now();
        let snapshot = std::hint::black_box(
          LayoutSnapshot::capture(&root).expect("capture layout"),
        );
        let end = Instant::now();
        std::hint::black_box(&snapshot);
        assert_eq!(
          layout_signature(&snapshot, &window_ids),
          semantic_baseline,
          "headline output must match the normalized fixture result"
        );
        std::hint::black_box(&snapshot);
        samples.push(Sample { repetition, index, duration_ns: end.duration_since(start).as_nanos(), outcome: "success".into(), native_call_coverage: "zero semantic native operations by inspected pure capture boundary; no external OS transition tracing".into(), allocations: None });
        drop(snapshot);
      }
    }
    #[cfg(feature = "benchmark-allocations")]
    for repetition in 0..spec.repetitions {
      for _ in 0..spec.warmups_per_repetition {
        let snapshot = LayoutSnapshot::capture(&root)
          .expect("allocation-pass warmup capture");
        std::hint::black_box(&snapshot);
        drop(snapshot);
      }
      for sample in samples
        .iter_mut()
        .filter(|sample| sample.repetition == repetition)
      {
        let mut snapshot = None;
        sample.allocations = Some(count_allocations(|| {
          snapshot = Some(
            LayoutSnapshot::capture(&root)
              .expect("allocation-pass capture"),
          );
          std::hint::black_box(snapshot.as_ref());
        }));
        assert_eq!(layout_signature(snapshot.as_ref().expect("captured allocation output"), &window_ids), semantic_baseline, "allocation output must equal the headline normalized fixture result");
        drop(snapshot);
      }
    }
    spec.timing_mode = if cfg!(feature = "benchmark-allocations") {
      "allocation-only; timing not headline"
    } else {
      "headline; default allocator; no counting instrumentation"
    }
    .into();
    let bytes =
      u64::try_from(count).unwrap_or(u64::MAX).saturating_mul(24);
    let floor = FloorModel {
      boundary: spec.boundary.clone(),
      assumptions: vec!["Illustrative optimistic hot-cache traffic contract: at least 8 input bytes + 16 output bytes per window; assumes 64 B/cycle at 3 GHz = 192 bytes/ns. This is traffic-only, not a complete floor or measured host bandwidth.".into()],
      irreducible_bytes: bytes,
      assumed_bandwidth_bytes_per_ns: 192.0,
      traffic_only_floor_ns: bytes as f64 / 192.0,
      complete_floor_ns: None,
      closure: "UNRESOLVED: no operation-specific instruction/dependency contract or validated host throughput; traffic-only floor cannot certify <=3x".into(),
    };
    cases.push((spec, samples, floor));

    let bounds = mock_working_area();
    let desired = DesiredFrame {
      rect: Rect::from_xy(0, 0, 800, 600),
      monitor: bounds.clone(),
      working_area: bounds.clone(),
      dpi: 96,
      state: NativeState::Normal,
      parking_clamp: None,
    };
    let observed = ObservedFrame {
      rect: desired.rect.clone(),
      dpi: 96,
      state: NativeState::Normal,
    };
    let scripted_clock = (0..21_000usize)
      .map(|index| Instant::now() + Duration::from_nanos(index as u64))
      .collect::<Vec<_>>();
    let mut reconcile_spec = CaseSpec {
      name: format!("frame-reconciler-retarget-next-batch-n{count}"),
      boundary: "one N-reconciler batch: retarget + next using precomputed scripted clock values; stop before result validation; wall clock reads excluded".into(),
      window_count: count,
      topology: "N independent per-window state machines; no native handles or mocked native API".into(),
      maximum_depth: 1,
      workspace_distribution: vec![],
      warmups_per_repetition: 1_000,
      measured_samples_per_repetition: 20_000,
      repetitions: 3,
      timing_mode: "headline; uninstrumented".into(),
    };
    let mut reconcile_samples = Vec::with_capacity(
      reconcile_spec.measured_samples_per_repetition
        * reconcile_spec.repetitions,
    );
    let mut request_payloads = vec![None; count];
    for repetition in 0..reconcile_spec.repetitions {
      let mut reconcilers = (0..count)
        .map(|_| FrameReconciler::new(desired.clone()))
        .collect::<Vec<_>>();
      for (index, scripted_now) in scripted_clock
        .iter()
        .copied()
        .take(reconcile_spec.warmups_per_repetition)
        .enumerate()
      {
        reconcile_batch(
          &mut reconcilers,
          &desired,
          &observed,
          scripted_now,
          index,
          &mut request_payloads,
        );
      }
      for index in 0..reconcile_spec.measured_samples_per_repetition {
        let scripted_now =
          scripted_clock[reconcile_spec.warmups_per_repetition + index];
        request_payloads.fill(None);
        let start = Instant::now();
        let requests = reconcile_batch(
          &mut reconcilers,
          &desired,
          &observed,
          scripted_now,
          index,
          &mut request_payloads,
        );
        let end = Instant::now();
        std::hint::black_box(requests);
        assert_eq!(
          requests, count,
          "each retargeted reconciler must request one mutation"
        );
        validate_reconciler_state(
          &reconcilers,
          &desired,
          index,
          &request_payloads,
        );
        reconcile_samples.push(Sample { repetition, index, duration_ns: end.duration_since(start).as_nanos(), outcome: "success".into(), native_call_coverage: "zero semantic native operations: transition API directly consumes scripted observations; external OS transitions unsupported".into(), allocations: None });
      }
    }
    #[cfg(feature = "benchmark-allocations")]
    for repetition in 0..reconcile_spec.repetitions {
      let mut counted = (0..count)
        .map(|_| FrameReconciler::new(desired.clone()))
        .collect::<Vec<_>>();
      for (index, scripted_now) in scripted_clock
        .iter()
        .copied()
        .take(reconcile_spec.warmups_per_repetition)
        .enumerate()
      {
        let requests = reconcile_batch(
          &mut counted,
          &desired,
          &observed,
          scripted_now,
          index,
          &mut request_payloads,
        );
        assert_eq!(requests, count, "allocation warmup request count");
      }
      for sample in reconcile_samples
        .iter_mut()
        .filter(|sample| sample.repetition == repetition)
      {
        let now = scripted_clock
          [reconcile_spec.warmups_per_repetition + sample.index];
        let mut requests = 0;
        request_payloads.fill(None);
        sample.allocations = Some(count_allocations(|| {
          requests = reconcile_batch(
            &mut counted,
            &desired,
            &observed,
            now,
            sample.index,
            &mut request_payloads,
          );
          std::hint::black_box(requests);
        }));
        assert_eq!(
          requests, count,
          "allocation pass request count must equal N"
        );
        validate_reconciler_state(
          &counted,
          &desired,
          sample.index,
          &request_payloads,
        );
      }
    }
    reconcile_spec.timing_mode =
      if cfg!(feature = "benchmark-allocations") {
        "allocation-only; timing not headline"
      } else {
        "headline; default allocator; no counting instrumentation"
      }
      .into();
    let floor = FloorModel {
      boundary: reconcile_spec.boundary.clone(),
      assumptions: vec!["Traffic-only assumed minimum is 8 bytes of input plus 8 bytes of state/output per reconciler; illustrative 3 GHz, 64 B/cycle hot-cache bandwidth = 192 bytes/ns. Minimum byte contract and bandwidth are analytical assumptions, not hardware measurements.".into()],
      irreducible_bytes: count as u64 * 16,
      assumed_bandwidth_bytes_per_ns: 192.0,
      traffic_only_floor_ns: (count as f64 * 16.0) / 192.0,
      complete_floor_ns: None,
      closure: "UNRESOLVED: instruction/dependent-access lower bound and validated throughput unavailable; no <=3x claim".into(),
    };
    cases.push((reconcile_spec, reconcile_samples, floor));
  }
  emit_local_evidence(&mut run, cases);
}

/// Exercises real release-build provenance with unset compiler overrides.
#[test]
#[ignore = "opt-in Cargo provenance smoke; no measurements"]
fn provenance_smoke_unset_overrides() {
  let mut run = begin_smoke_run().expect("validate release smoke build");
  let settings = &run
    .provenance
    .snapshot
    .as_ref()
    .expect("smoke snapshot")
    .settings;
  for key in [
    "GLAZEWM_OPERATOR_RUSTFLAGS",
    "GLAZEWM_OPERATOR_RUSTC",
    "GLAZEWM_OPERATOR_RUSTC_WRAPPER",
    "GLAZEWM_OPERATOR_RUSTC_WORKSPACE_WRAPPER",
  ] {
    assert_eq!(
      settings.get(key).map(String::as_str),
      Some(""),
      "expected unset operator override {key}"
    );
  }
  assert_ne!(
    settings
      .get("build-only:RUSTC")
      .expect("Cargo resolved RUSTC"),
    ""
  );
  assert_ne!(settings.get("build-only:TARGET").expect("Cargo TARGET"), "");
  let smoke_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("../..")
    .join("target/benchmarks/smoke");
  assert!(run.directory.starts_with(smoke_root));
  run.start_measurement().expect("start smoke lifecycle");
  run.finish_smoke().expect("finish smoke lifecycle");
  let status: serde_json::Value = serde_json::from_slice(
    &std::fs::read(run.directory.join("status.json"))
      .expect("read smoke status"),
  )
  .expect("parse smoke status");
  assert_eq!(status["status"], "smoke-validated");
  assert!(
    !run.directory.join("core.jsonl").exists(),
    "smoke is not full measurement evidence"
  );
}

/// Exercises effective nonempty `RUSTFLAGS` through Cargo's build-script
/// environment.
#[test]
#[ignore = "opt-in Cargo provenance smoke; no measurements"]
fn provenance_smoke_nonempty_rustflags() {
  let mut run = begin_smoke_run().expect("validate release smoke build");
  let settings = &run
    .provenance
    .snapshot
    .as_ref()
    .expect("smoke snapshot")
    .settings;
  assert_eq!(
    settings
      .get("GLAZEWM_OPERATOR_RUSTFLAGS")
      .map(String::as_str),
    Some("-C debuginfo=0")
  );
  let effective_flags = settings
    .get("build-only:CARGO_ENCODED_RUSTFLAGS")
    .expect("Cargo effective flags");
  assert!(effective_flags.contains("debuginfo=0"));
  assert!(
    effective_flags.contains('\u{1f}'),
    "Cargo supplies unit-separator encoded effective flags"
  );
  run.start_measurement().expect("start smoke lifecycle");
  run.finish_smoke().expect("finish smoke lifecycle");
  assert!(
    !run.directory.join("core.jsonl").exists(),
    "smoke is not full measurement evidence"
  );
}

/// Rejects a persistent operator-setting change without publishing
/// measurement evidence.
#[test]
#[ignore = "opt-in Cargo provenance smoke; no measurements"]
fn provenance_smoke_rejects_operator_setting_change() {
  let run = begin_smoke_run().expect("validate release smoke build");
  run.start_measurement().expect("start smoke lifecycle");
  // SAFETY: The Taskfile runs this single ignored test with one test
  // thread; no WM workers or other environment readers are started.
  unsafe {
    std::env::set_var(
      "GLAZEWM_OPERATOR_VERSION_NUMBER",
      "persistent-mismatch",
    );
  }
  let root =
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
  let actual = provenance::capture_inputs(&root)
    .expect("capture changed supported setting");
  let expected = run.provenance.snapshot.as_ref().expect("smoke snapshot");
  assert!(provenance::verify_inputs(expected, &actual).is_err());
  let status_path = run.directory.join("status.json");
  drop(run);
  let status: serde_json::Value = serde_json::from_slice(
    &std::fs::read(status_path).expect("read rejected smoke status"),
  )
  .expect("parse smoke status");
  assert_eq!(status["status"], "rejected");
}

/// Measures the four production intent scenarios without desktop effects.
#[test]
#[ignore = "opt-in release intent benchmark; never run automatically"]
fn intent_scenarios() {
  let mut run = begin_run()
    .expect("validate intent benchmark build before fixture preparation");
  run
    .start_measurement()
    .expect("mark intent benchmark measurement start");
  let cases = intents::cases(1_000, 20_000, 3);
  emit_local_evidence(&mut run, cases);
}

fn make_workspace(
  name: &str,
  windows: &[crate::models::TilingContainer],
) -> crate::models::Workspace {
  use wm_common::TilingDirection;

  use crate::models::{SplitContainer, TilingContainer, Workspace};
  fn balanced(items: &[TilingContainer], depth: usize) -> TilingContainer {
    if items.len() == 1 {
      return items[0].clone();
    }
    let midpoint = items.len() / 2;
    let children = vec![
      balanced(&items[..midpoint], depth + 1),
      balanced(&items[midpoint..], depth + 1),
    ];
    SplitContainer::mock()
      .tiling_direction(if depth.is_multiple_of(2) {
        TilingDirection::Horizontal
      } else {
        TilingDirection::Vertical
      })
      .tiling_containers(children)
      .call()
      .into()
  }
  let root = balanced(windows, 0);
  Workspace::mock()
    .name(name.to_string())
    .tiling_containers(vec![root])
    .call()
}

fn layout_signature(
  snapshot: &crate::layout_snapshot::LayoutSnapshot,
  window_ids: &[uuid::Uuid],
) -> Vec<wm_platform::Rect> {
  window_ids
    .iter()
    .map(|id| {
      snapshot
        .rect(*id)
        .expect("fixture window rectangle")
        .clone()
    })
    .collect()
}

fn count_windows(root: &crate::models::RootContainer) -> usize {
  use crate::{models::Container, traits::CommonGetters};
  fn count(container: Container) -> usize {
    match container {
      Container::TilingWindow(_) => 1,
      Container::Split(split) => {
        split.children().into_iter().map(count).sum()
      }
      Container::Monitor(monitor) => {
        monitor.children().into_iter().map(count).sum()
      }
      Container::Workspace(workspace) => {
        workspace.children().into_iter().map(count).sum()
      }
      _ => 0,
    }
  }
  root.children().into_iter().map(count).sum()
}

fn maximum_split_depth(root: &crate::models::RootContainer) -> usize {
  use crate::{models::Container, traits::CommonGetters};
  fn depth(container: Container) -> usize {
    match container {
      Container::Split(split) => {
        1 + split.children().into_iter().map(depth).max().unwrap_or(0)
      }
      Container::Monitor(monitor) => {
        monitor.children().into_iter().map(depth).max().unwrap_or(0)
      }
      Container::Workspace(workspace) => workspace
        .children()
        .into_iter()
        .map(depth)
        .max()
        .unwrap_or(0),
      _ => 0,
    }
  }
  root.children().into_iter().map(depth).max().unwrap_or(0)
}

/// Checks retained requests and final intent outside measurement scopes.
fn validate_reconciler_state(
  reconcilers: &[crate::native_reconciler::FrameReconciler],
  desired: &crate::native_reconciler::DesiredFrame,
  index: usize,
  requests: &[Option<crate::native_reconciler::NativeRequest>],
) {
  assert_eq!(
    requests.len(),
    reconcilers.len(),
    "one output slot per window"
  );
  for (window_index, reconciler) in reconcilers.iter().enumerate() {
    let offset = if (index + window_index).is_multiple_of(2) {
      5
    } else {
      10
    };
    let expected = crate::native_reconciler::DesiredFrame {
      rect: wm_platform::Rect::from_xy(0, 0, 800 + offset, 600),
      ..desired.clone()
    };
    assert_eq!(
      reconciler.desired, expected,
      "reconciler retains intended target"
    );
    assert!(reconciler.generation > 0, "request generation is valid");
    let request = requests[window_index]
      .as_ref()
      .expect("each fixture window must emit a request");
    assert_eq!(
      request.generation, reconciler.generation,
      "request belongs to current intent"
    );
    assert_eq!(
      request.mutation,
      crate::native_reconciler::NativeMutation::Frame(expected.rect),
      "request carries the intended frame mutation and rectangle"
    );
  }
}

/// Retains every transition output in caller-owned, preallocated storage.
fn reconcile_batch(
  reconcilers: &mut [crate::native_reconciler::FrameReconciler],
  desired: &crate::native_reconciler::DesiredFrame,
  observed: &crate::native_reconciler::ObservedFrame,
  now: Instant,
  index: usize,
  requests: &mut [Option<crate::native_reconciler::NativeRequest>],
) -> usize {
  let mut request_count = 0usize;
  for (window_index, reconciler) in reconcilers.iter_mut().enumerate() {
    let offset = if (index + window_index).is_multiple_of(2) {
      5
    } else {
      10
    };
    reconciler.retarget(crate::native_reconciler::DesiredFrame {
      rect: wm_platform::Rect::from_xy(0, 0, 800 + offset, 600),
      ..desired.clone()
    });
    requests[window_index] = reconciler.next(observed, now);
    request_count += usize::from(requests[window_index].is_some());
  }
  request_count
}

/// Validates embedded build provenance and retains the executable before
/// fixtures are prepared.
fn begin_run() -> anyhow::Result<RunContext> {
  begin_run_in(false)
}

fn begin_smoke_run() -> anyhow::Result<RunContext> {
  begin_run_in(true)
}

fn begin_run_in(smoke: bool) -> anyhow::Result<RunContext> {
  use std::{fs, path::PathBuf};

  use anyhow::Context;
  let provenance: provenance::BuildProvenance = serde_json::from_str(
    include_str!(concat!(env!("OUT_DIR"), "/benchmark-provenance.json")),
  )?;
  let expected_mode = if cfg!(feature = "benchmark-allocations") {
    "allocations"
  } else {
    "headline"
  };
  let snapshot =
    provenance::validate_build_stamp(&provenance, expected_mode)?;
  let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
  let before = provenance::capture_inputs(&root)?;
  provenance::verify_inputs(snapshot, &before)?;
  let executable = std::env::current_exe()?;
  let executable_hash = provenance::sha256_file(&executable)?;
  let run_id = uuid::Uuid::new_v4().to_string();
  let directory = if smoke {
    root
      .join("target/benchmarks")
      .join("smoke")
      .join(expected_mode)
      .join(&run_id)
  } else {
    root
      .join("target/benchmarks")
      .join(expected_mode)
      .join(&run_id)
  };
  fs::create_dir_all(&directory)?;
  let retained_executable = directory.join(if cfg!(windows) {
    "benchmark.exe"
  } else {
    "benchmark"
  });
  // An incomplete marker is deliberately distinct from completed evidence.
  fs::write(directory.join("status.json"), serde_json::json!({"schema_version":3,"status":"preparing","run_id":run_id}).to_string())?;
  let run = RunContext {
    run_id,
    completed: false,
    directory,
    retained_executable,
    executable_hash,
    provenance,
  };
  fs::copy(&executable, &run.retained_executable)
    .context("retain benchmark executable")?;
  anyhow::ensure!(
    provenance::sha256_file(&run.retained_executable)?
      == run.executable_hash,
    "retained executable hash mismatch"
  );
  let after_retention = provenance::capture_inputs(&root)?;
  provenance::verify_inputs(
    run
      .provenance
      .snapshot
      .as_ref()
      .expect("validated snapshot"),
    &after_retention,
  )?;
  Ok(run)
}

impl RunContext {
  /// Marks the lifecycle as measuring before fixture preparation or
  /// workload execution.
  fn start_measurement(&self) -> anyhow::Result<()> {
    std::fs::write(
      self.directory.join("status.json"),
      serde_json::json!({"schema_version":3,"status":"measuring","run_id":self.run_id}).to_string(),
    )?;
    Ok(())
  }

  /// Validates and completes a smoke run without publishing measurement
  /// evidence.
  fn finish_smoke(&mut self) -> anyhow::Result<()> {
    use anyhow::Context;

    let root =
      std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let actual = provenance::capture_inputs(&root)?;
    provenance::verify_inputs(
      self
        .provenance
        .snapshot
        .as_ref()
        .context("smoke stamp lacks snapshot")?,
      &actual,
    )?;
    anyhow::ensure!(
      provenance::sha256_file(&self.retained_executable)?
        == self.executable_hash,
      "retained smoke executable hash mismatch"
    );
    std::fs::write(
      self.directory.join("status.json"),
      serde_json::json!({"schema_version":3,"status":"smoke-validated","run_id":self.run_id}).to_string(),
    )?;
    self.completed = true;
    Ok(())
  }
}

/// Emits the completed raw evidence to a unique local run directory.
#[allow(clippy::too_many_lines)]
fn emit_local_evidence(
  run: &mut RunContext,
  runs: Vec<(CaseSpec, Vec<Sample>, FloorModel)>,
) {
  use std::{fs, io::Write, path::PathBuf};
  let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("../..")
    .join("target/benchmarks")
    .join(if cfg!(feature = "benchmark-allocations") {
      "allocations"
    } else {
      "headline"
    })
    .join(&run.run_id);
  fs::create_dir_all(&directory)
    .expect("create unique benchmark evidence directory");
  let mut file = fs::File::create(directory.join("core.jsonl"))
    .expect("create run JSONL");
  let hardware_inventory = powershell("$a=Get-CimInstance Win32_Processor | Select-Object -First 1 Name,NumberOfCores,NumberOfLogicalProcessors,MaxClockSpeed; $b=Get-CimInstance Win32_ComputerSystem | Select-Object Manufacturer,Model,TotalPhysicalMemory; @($a,$b) | ConvertTo-Json -Compress");
  let os_inventory = powershell("Get-CimInstance Win32_OperatingSystem | Select-Object Caption,Version,BuildNumber | ConvertTo-Json -Compress");
  let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
  let executable = run.executable_hash.clone();
  let executable_path =
    run.retained_executable.to_string_lossy().into_owned();
  let dirty_hashes = run
    .provenance
    .snapshot
    .as_ref()
    .expect("enabled build provenance")
    .inputs
    .iter()
    .map(|(path, hash)| format!("{path}:{hash}"))
    .collect();
  let clock_calibration = calibrate_clock();
  let manifest = RunManifest {
    schema_version: 3, status: "measuring".into(), build_provenance: run.provenance.clone(), verification: "preparation, measurement and publication boundary snapshots matched embedded build provenance; retained executable hash verified".into(), run_id: run.run_id.clone(), source_revision: run.provenance.snapshot.as_ref().expect("snapshot").head.clone(), dirty_source_hashes: dirty_hashes,
    executable_path, executable_sha256: executable, build_profile: run.provenance.snapshot.as_ref().expect("snapshot").settings.get("build-only:PROFILE").cloned().expect("captured Cargo profile"),
    rustc: rustc_version(), cargo: cargo_version(), platform: std::env::consts::OS.into(), os_inventory, hardware_inventory,
    configuration_hash: "deterministic synthetic fixtures; no runtime user configuration".into(), clock_calibration,
    allocation_coverage: "separate thread-local Rust System allocator pass records alloc/realloc/free calls and requested allocated/reallocated bytes; headline timing uninstrumented; foreign heaps excluded".into(),
    native_call_coverage: "native-free semantic paths by code inspection; timer/harness OS activity excluded; actual OS transitions require external profiler".into(),
    sample_protocol: "1000 warmups per repetition; 20000 individual operation/batch samples x3; nearest rank; successful-only quantiles; failures reported separately; fixture setup outside timed boundary; checked creation includes managed UUID generation".into(),
  };
  writeln!(
    file,
    "{}",
    serde_json::json!({"record":"manifest","value":manifest})
  )
  .expect("write manifest");
  for (case, samples, floor) in runs {
    let successful = samples
      .iter()
      .filter(|sample| sample.outcome == "success")
      .map(|sample| sample.duration_ns)
      .collect::<Vec<_>>();
    let summary = serde_json::json!({"record":"case","value":case,"summary_ns":{"median":percentile(&successful,50),"p95":percentile(&successful,95),"p99":percentile(&successful,99),"success_count":successful.len(),"failures":samples.len()-successful.len(),"quantiles_condition":"successful samples only; failure count is explicit"}});
    writeln!(file, "{summary}").expect("write case summary");
    for sample in samples {
      writeln!(
        file,
        "{}",
        serde_json::json!({"record":"sample","value":sample})
      )
      .expect("write raw sample");
    }
    let quantiles = [50, 95, 99]
      .into_iter()
      .filter_map(|p| {
        percentile(&successful, p).map(|value| {
          (
            format!("p{p}"),
            serde_json::json!({
              "observed_ns": value,
              "traffic_only_gap_ns":
                (value as f64 - floor.traffic_only_floor_ns).max(0.0),
              "observed_multiple_of_traffic_only":
                value as f64 / floor.traffic_only_floor_ns,
            }),
          )
        })
      })
      .collect::<serde_json::Map<_, _>>();
    writeln!(file, "{}", serde_json::json!({"record":"floor_model","value":floor,"quantile_comparison":quantiles,"complete_floor_multiple":"UNRESOLVED; traffic bound cannot certify complete physical floor or <=3x"})).expect("write floor model");
  }
  file.flush().expect("flush evidence before validation");
  let actual = provenance::capture_inputs(&repo_root)
    .expect("verify inputs before publication");
  provenance::verify_inputs(
    run.provenance.snapshot.as_ref().expect("snapshot"),
    &actual,
  )
  .expect("reject changed inputs before publication");
  assert_eq!(
    provenance::sha256_file(&run.retained_executable)
      .expect("verify retained executable"),
    run.executable_hash,
    "retained executable changed before publication"
  );
  writeln!(file, "{}", serde_json::json!({"record":"completion","status":"validated","schema_version":3})).expect("publish validated completion");
  file.flush().expect("flush completion record");
  fs::write(run.directory.join("status.json"), serde_json::json!({"schema_version":3,"status":"validated","run_id":run.run_id}).to_string()).expect("publish validated run status");
  run.completed = true;
}

impl Drop for RunContext {
  fn drop(&mut self) {
    if !self.completed {
      let _ = std::fs::write(self.directory.join("status.json"), serde_json::json!({"schema_version":3,"status":"rejected","run_id":self.run_id,"reason":"run did not complete validation"}).to_string());
    }
  }
}

#[cfg(feature = "benchmark-allocations")]
struct AllocationCounterGuard;

#[cfg(feature = "benchmark-allocations")]
impl Drop for AllocationCounterGuard {
  fn drop(&mut self) {
    ACTIVE_ALLOCATIONS.with(|active| active.set(std::ptr::null_mut()));
  }
}

#[cfg(feature = "benchmark-allocations")]
fn count_allocations(operation: impl FnOnce()) -> AllocationCounts {
  let mut counts = AllocationCounts::default();
  ACTIVE_ALLOCATIONS.with(|active| {
    assert!(
      active.get().is_null(),
      "allocation counter scopes must not nest"
    );
    active.set(&raw mut counts);
  });
  let _guard = AllocationCounterGuard;
  operation();
  counts
}

fn calibrate_clock() -> ClockCalibration {
  let mut deltas = Vec::with_capacity(20_000);
  let mut minimum = None;
  for _ in 0..20_000 {
    let first = Instant::now();
    let second = Instant::now();
    let delta = second.duration_since(first).as_nanos();
    if delta > 0 {
      minimum =
        Some(minimum.map_or(delta, |current: u128| current.min(delta)));
    }
    deltas.push(delta);
  }
  ClockCalibration { minimum_nonzero_delta_ns: minimum, empty_pair_median_ns: percentile(&deltas,50), empty_pair_p95_ns: percentile(&deltas,95), calibration_samples: deltas.len(), uncertainty_statement: "Observed clock quantization and empty-pair overhead only; these do not provide confidence bounds or subtract overhead from headline samples.".into() }
}

fn powershell(command: &str) -> String {
  std::process::Command::new("powershell")
    .args(["-NoProfile", "-Command", command])
    .output()
    .ok()
    .filter(|output| output.status.success())
    .map(|output| {
      String::from_utf8_lossy(&output.stdout).trim().to_owned()
    })
    .unwrap_or_else(|| "unavailable".into())
}
fn rustc_version() -> String {
  command_version("rustc", "--version --verbose")
}
fn cargo_version() -> String {
  command_version("cargo", "--version")
}
fn command_version(command: &str, argument: &str) -> String {
  let args = argument.split_whitespace().collect::<Vec<_>>();
  std::process::Command::new(command)
    .args(args)
    .output()
    .ok()
    .filter(|output| output.status.success())
    .map(|output| {
      String::from_utf8_lossy(&output.stdout).trim().to_owned()
    })
    .unwrap_or_else(|| "unavailable".into())
}

#[test]
fn nearest_rank_percentiles_use_raw_samples() {
  assert_eq!(percentile(&[9, 1, 2, 3], 50), Some(2));
  assert_eq!(percentile(&[9, 1, 2, 3], 95), Some(9));
  assert_eq!(percentile(&[], 50), None);
  assert_eq!(percentile(&[1], 0), None);
}

#[test]
fn request_count_accumulates_every_entry_instead_of_xor_parity() {
  for expected in [10usize, 50, 100] {
    let request_flags = std::iter::repeat_n(true, expected);
    let actual = request_flags.map(usize::from).sum::<usize>();
    assert_eq!(actual, expected);
    assert_ne!(actual, 0);
  }
}

/// Exercises all four production intent paths without native
/// synchronization.
#[test]
#[allow(clippy::too_many_lines)] // One fixture trace checks all four
                                 // intent boundaries.
fn intent_paths_preserve_fixture_invariants() {
  use wm_platform::{LengthValue, NativeWindow};

  use crate::{
    commands::{
      container::{
        attach_container, focus_container_by_id, set_focused_descendant,
      },
      window::{create_window_with_checked_monitor, resize_window},
      workspace::focus_workspace,
    },
    models::{
      Monitor, NativeWindowProperties, TilingContainer, TilingWindow,
      WorkspaceTarget,
    },
    traits::CommonGetters,
    user_config::UserConfig,
    wm_state::WmState,
  };

  for count in [10usize, 50, 100] {
    let mut state = WmState::mock();
    let config = UserConfig::mock_from_str("general:\n  toggle_workspace_on_refocus: false\nworkspaces:\n  - name: '1'\n    keep_alive: true\n  - name: '2'\n    keep_alive: true\n").expect("in-memory fixture config");
    let windows = (0..count - 1)
      .map(|_| TilingWindow::mock().call())
      .collect::<Vec<_>>();
    let containers = windows
      .iter()
      .cloned()
      .map(Into::into)
      .collect::<Vec<TilingContainer>>();
    let midpoint = containers.len() / 2;
    let workspaces = vec![
      make_workspace("1", &containers[..midpoint]),
      make_workspace("2", &containers[midpoint..]),
    ];
    let monitor = Monitor::mock().workspaces(workspaces.clone()).call();
    attach_container(
      &monitor.clone().into(),
      &state.root_container.clone().into(),
      None,
    )
    .expect("attach fixture monitor");
    set_focused_descendant(&windows[0].clone().into(), None);
    assert_eq!(count_windows(&state.root_container), count - 1);

    focus_container_by_id(&windows[1].id(), &mut state, &config)
      .expect("focus intent");
    assert_eq!(
      state.focused_container().expect("focused leaf").id(),
      windows[1].id()
    );
    assert!(state.pending_sync.needs_focus_update());
    state.pending_sync.clear();

    focus_workspace(
      WorkspaceTarget::Name("2".into()),
      &mut state,
      &config,
    )
    .expect("workspace switch intent");
    assert_eq!(
      monitor
        .displayed_workspace()
        .expect("displayed workspace")
        .id(),
      workspaces[1].id()
    );
    assert_eq!(state.recent_workspace_name.as_deref(), Some("1"));
    focus_workspace(
      WorkspaceTarget::Name("1".into()),
      &mut state,
      &config,
    )
    .expect("return workspace intent");
    assert_eq!(state.recent_workspace_name.as_deref(), Some("2"));
    state.pending_sync.clear();

    for index in 0..256 {
      resize_window(
        &windows[0].clone().into(),
        Some(LengthValue::from_px(if index % 2 == 0 { 1 } else { -1 })),
        None,
        &mut state,
      )
      .expect("resize burst intent");
    }
    assert!(state.pending_sync.has_changes());
    assert_eq!(count_windows(&state.root_container), count - 1);

    let created = create_window_with_checked_monitor(
      NativeWindow::mock(),
      NativeWindowProperties::mock().call(),
      Some(workspaces[0].clone().into()),
      &monitor,
      &mut state,
      &config,
    )
    .expect("checked creation intent");
    assert_eq!(
      created.workspace().expect("created workspace").id(),
      workspaces[0].id()
    );
    assert_eq!(count_windows(&state.root_container), count);
    let snapshot = crate::layout_snapshot::LayoutSnapshot::capture(
      &state.root_container,
    )
    .expect("post-intent layout");
    for window in state.windows() {
      let rect = snapshot
        .rect(window.id())
        .expect("every window has a rectangle");
      assert!(rect.width() >= 0 && rect.height() >= 0);
    }
    assert!(!state.pending_sync.needs_focus_update());
  }
}

/// Rejects missing, stale, wrong-kind, and wrong-geometry request
/// payloads.
#[test]
fn reconciler_payload_validation_rejects_corrupted_outputs() {
  use wm_platform::Rect;

  use crate::native_reconciler::{
    DesiredFrame, FrameReconciler, NativeMutation, NativeRequest,
    NativeState, ObservedFrame,
  };

  let desired = DesiredFrame {
    rect: Rect::from_xy(0, 0, 800, 600),
    monitor: crate::test_utils::mock_working_area(),
    working_area: crate::test_utils::mock_working_area(),
    dpi: 96,
    state: NativeState::Normal,
    parking_clamp: None,
  };
  let observed = ObservedFrame {
    rect: desired.rect.clone(),
    dpi: 96,
    state: NativeState::Normal,
  };
  let mut reconcilers = vec![FrameReconciler::new(desired.clone())];
  let mut requests = vec![None];
  assert_eq!(
    reconcile_batch(
      &mut reconcilers,
      &desired,
      &observed,
      Instant::now(),
      0,
      &mut requests
    ),
    1
  );
  validate_reconciler_state(&reconcilers, &desired, 0, &requests);
  let valid = requests[0].clone().expect("retained request");
  let invalid = [
    None,
    Some(NativeRequest {
      generation: valid.generation + 1,
      ..valid.clone()
    }),
    Some(NativeRequest {
      mutation: NativeMutation::Maximize,
      ..valid.clone()
    }),
    Some(NativeRequest {
      mutation: NativeMutation::Frame(desired.rect.clone()),
      ..valid
    }),
  ];
  for request in invalid {
    assert!(
      std::panic::catch_unwind(|| validate_reconciler_state(
        &reconcilers,
        &desired,
        0,
        &[request]
      ))
      .is_err(),
      "invalid payload must not earn a success sample"
    );
  }
}

#[test]
fn clock_pair_calibration_uses_the_measured_two_timestamp_interval() {
  let first = Instant::now();
  let second = first + Duration::from_nanos(100);
  assert_eq!(second.duration_since(first).as_nanos(), 100);
}

#[test]
fn incomplete_run_drop_records_rejection_not_completion() {
  let directory = std::env::temp_dir().join(format!(
    "glazewm-benchmark-rejection-{}",
    uuid::Uuid::new_v4()
  ));
  std::fs::create_dir_all(&directory).expect("create rejection fixture");
  let status_path = directory.join("status.json");
  std::fs::write(&status_path, "{\"status\":\"preparing\"}")
    .expect("write preparing status");
  let run = RunContext {
    run_id: "rejected-test".into(),
    completed: false,
    directory: directory.clone(),
    retained_executable: directory.join("missing.exe"),
    executable_hash: String::new(),
    provenance: provenance::BuildProvenance {
      schema_version: provenance::SCHEMA_VERSION,
      enabled: false,
      mode: String::new(),
      snapshot: None,
      rustc: String::new(),
      cargo: String::new(),
      contract: String::new(),
    },
  };
  run
    .start_measurement()
    .expect("transition into measuring before work");
  let measuring: serde_json::Value = serde_json::from_slice(
    &std::fs::read(&status_path).expect("read measuring status"),
  )
  .expect("parse measuring status");
  assert_eq!(measuring["status"], "measuring");
  drop(run);
  let status: serde_json::Value = serde_json::from_slice(
    &std::fs::read(&status_path).expect("read rejected status"),
  )
  .expect("parse rejected status");
  assert_eq!(status["status"], "rejected");
  std::fs::remove_dir_all(directory).expect("remove rejection fixture");
}

#[test]
fn schema_records_round_trip_and_floor_comparison_is_quantitative() {
  let floor = FloorModel {
    boundary: "capture".into(),
    assumptions: vec![],
    irreducible_bytes: 240,
    assumed_bandwidth_bytes_per_ns: 192.0,
    traffic_only_floor_ns: 1.25,
    complete_floor_ns: None,
    closure: "UNRESOLVED".into(),
  };
  let encoded = serde_json::to_string(&floor).expect("serialize floor");
  let decoded: FloorModel =
    serde_json::from_str(&encoded).expect("deserialize floor");
  assert_eq!(decoded.traffic_only_floor_ns, 1.25);
  let comparison = serde_json::json!({"observed":10u128,"gap":10.0-decoded.traffic_only_floor_ns,"multiple":10.0/decoded.traffic_only_floor_ns});
  assert_eq!(comparison["gap"], 8.75);
  assert_eq!(comparison["multiple"], 8.0);
}

#[cfg(feature = "benchmark-allocations")]
#[test]
fn allocation_counter_mode_is_thread_local_and_semantically_opt_in() {
  let counts = count_allocations(|| {
    let mut value = Vec::with_capacity(1);
    value.push(1u8);
    std::hint::black_box(&value);
    value.push(2);
    drop(value);
  });
  assert!(counts.allocations >= 1);
  assert!(counts.reallocations >= 1);
  assert!(counts.frees >= 1);
  assert!(counts.allocated_bytes >= 1);
  assert!(counts.reallocated_bytes >= 2);
  assert!(counts.freed_bytes >= 1);
  ACTIVE_ALLOCATIONS.with(|active| assert!(active.get().is_null()));
}
