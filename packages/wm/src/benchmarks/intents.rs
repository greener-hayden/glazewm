//! Native-free measurements of production intent commands.

use std::time::Instant;

use wm_platform::{LengthValue, NativeWindow};

#[cfg(feature = "benchmark-allocations")]
use super::count_allocations;
use super::{
  count_windows, make_workspace, maximum_split_depth, CaseSpec,
  FloorModel, Sample,
};
use crate::{
  commands::{
    container::{
      attach_container, detach_container, focus_container_by_id,
      set_focused_descendant,
    },
    window::{create_window_with_checked_monitor, resize_window},
    workspace::focus_workspace,
  },
  layout_snapshot::LayoutSnapshot,
  models::{
    Monitor, NativeWindowProperties, TilingContainer, TilingWindow,
    WindowContainer, WorkspaceTarget,
  },
  traits::{CommonGetters, PositionGetters, TilingSizeGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

/// Builds a balanced, two-workspace fixture without platform
/// synchronization.
fn fixture(
  count: usize,
  native: &NativeWindow,
) -> (WmState, UserConfig, Monitor, Vec<TilingWindow>) {
  let state = WmState::mock();
  let config = UserConfig::mock_from_str("general:\n  toggle_workspace_on_refocus: false\nworkspaces:\n  - name: '1'\n    keep_alive: true\n  - name: '2'\n    keep_alive: true\n").expect("in-memory benchmark configuration");
  let windows = (0..count)
    .map(|index| {
      TilingWindow::mock()
        .native(native.clone())
        .title(format!("fixture-{index}"))
        .call()
    })
    .collect::<Vec<_>>();
  let containers = windows
    .iter()
    .cloned()
    .map(Into::into)
    .collect::<Vec<TilingContainer>>();
  let midpoint = count / 2;
  let monitor = Monitor::mock()
    .workspaces(vec![
      make_workspace("1", &containers[..midpoint]),
      make_workspace("2", &containers[midpoint..]),
    ])
    .call();
  attach_container(
    &monitor.clone().into(),
    &state.root_container.clone().into(),
    None,
  )
  .expect("attach benchmark monitor");
  set_focused_descendant(&windows[0].clone().into(), None);
  assert_eq!(count_windows(&state.root_container), count);
  (state, config, monitor, windows)
}

/// Measures commands while keeping setup, output validation, and cleanup
/// outside.
fn measure<I, O>(
  state: &mut WmState,
  spec: &CaseSpec,
  mut prepare: impl FnMut(&mut WmState, usize) -> I,
  mut operation: impl FnMut(&mut WmState, &mut I) -> O,
  mut validate: impl FnMut(&WmState, usize, &O),
  mut cleanup: impl FnMut(&mut WmState, O),
) -> Vec<Sample> {
  let mut samples = Vec::with_capacity(
    spec.repetitions * spec.measured_samples_per_repetition,
  );
  for repetition in 0..spec.repetitions {
    for index in 0..spec.warmups_per_repetition {
      let mut input = prepare(state, index);
      let output = operation(state, &mut input);
      validate(state, index, &output);
      cleanup(state, output);
    }
    for index in 0..spec.measured_samples_per_repetition {
      let mut input = prepare(state, index);
      let start = Instant::now();
      let output = std::hint::black_box(operation(state, &mut input));
      let end = Instant::now();
      std::hint::black_box(&output);
      validate(state, index, &output);
      samples.push(Sample {
        repetition, index, duration_ns: end.duration_since(start).as_nanos(),
        outcome: "success".into(),
        native_call_coverage: "zero native mutations in inspected intent boundary; native acquisition, synchronization, timer/harness activity, and visual completion excluded".into(),
        allocations: None,
      });
      cleanup(state, output);
    }
  }
  #[cfg(feature = "benchmark-allocations")]
  for repetition in 0..spec.repetitions {
    for index in 0..spec.warmups_per_repetition {
      let mut input = prepare(state, index);
      let output = operation(state, &mut input);
      validate(state, index, &output);
      cleanup(state, output);
    }
    for sample in samples
      .iter_mut()
      .filter(|sample| sample.repetition == repetition)
    {
      let mut input = prepare(state, sample.index);
      let mut output = None;
      sample.allocations = Some(count_allocations(|| {
        output = Some(std::hint::black_box(operation(state, &mut input)));
      }));
      let output = output.expect("allocation pass output");
      validate(state, sample.index, &output);
      cleanup(state, output);
    }
  }
  samples
}

/// Describes one intent case using the same sample protocol as kernel
/// cases.
fn specification(
  name: &str,
  count: usize,
  state: &WmState,
  warmups: usize,
  samples: usize,
  repetitions: usize,
) -> CaseSpec {
  CaseSpec {
    name: format!("{name}-n{count}"),
    boundary: format!("production {name} invocation through returned intent; output retained through end timestamp; preparation, validation, cleanup, native synchronization, and rendering excluded"),
    window_count: count,
    topology: "two workspaces; recursively balanced alternating-axis splits; creation restores one removed leaf, N-1 to N".into(),
    maximum_depth: maximum_split_depth(&state.root_container),
    workspace_distribution: vec![count / 2, count - count / 2],
    warmups_per_repetition: warmups,
    measured_samples_per_repetition: samples,
    repetitions,
    timing_mode: if cfg!(feature = "benchmark-allocations") { "allocation-only; timing not headline" } else { "headline; default allocator; no counting instrumentation" }.into(),
  }
}

/// Makes a deliberately incomplete traffic bound for the case's intent
/// input.
fn traffic_floor(spec: &CaseSpec, bytes: u64) -> FloorModel {
  FloorModel {
    boundary: spec.boundary.clone(),
    assumptions: vec![format!("Illustrative minimum intent traffic: {bytes} bytes; optimistic 64 B/cycle at 3 GHz = 192 B/ns, not measured host throughput. Creation includes UUID generation; resize is a 256-command batch, not one-event latency.")],
    irreducible_bytes: bytes,
    assumed_bandwidth_bytes_per_ns: 192.0,
    traffic_only_floor_ns: bytes as f64 / 192.0,
    complete_floor_ns: None,
    closure: "UNRESOLVED: no validated instruction/dependency or host-throughput floor; traffic-only bound cannot certify proximity to physics".into(),
  }
}

/// Measures focus, workspace switches, creation, and 256-command resize
/// bursts.
#[allow(clippy::too_many_lines)] // Four cases share protocol, but not
                                 // mutable fixtures.
pub(super) fn cases(
  warmups: usize,
  samples: usize,
  repetitions: usize,
) -> Vec<(CaseSpec, Vec<Sample>, FloorModel)> {
  let mut cases = Vec::new();
  let native = NativeWindow::mock();
  for count in [10usize, 50, 100] {
    let (mut state, config, _, windows) = fixture(count, &native);
    let ids = [windows[0].id(), windows[1].id()];
    let spec = specification(
      "focus-intent",
      count,
      &state,
      warmups,
      samples,
      repetitions,
    );
    let measured = measure(
      &mut state,
      &spec,
      |state, index| {
        state.pending_sync.clear();
        ids[index % 2]
      },
      |state, id| {
        focus_container_by_id(id, state, &config).expect("focus intent");
      },
      |state, index, ()| {
        assert_eq!(
          state.focused_container().expect("focused leaf").id(),
          ids[index % 2]
        );
        assert!(
          state.pending_sync.needs_focus_update()
            && state.pending_sync.needs_cursor_jump()
        );
      },
      |_, ()| {},
    );
    let floor = traffic_floor(&spec, 32);
    cases.push((spec, measured, floor));

    let (mut state, config, monitor, _) = fixture(count, &native);
    let workspaces = monitor.workspaces();
    let spec = specification(
      "workspace-switch-intent",
      count,
      &state,
      warmups,
      samples,
      repetitions,
    );
    let measured = measure(
      &mut state,
      &spec,
      |state, index| {
        state.pending_sync.clear();
        Some(WorkspaceTarget::Name(
          if index % 2 == 0 { "2" } else { "1" }.into(),
        ))
      },
      |state, target| {
        focus_workspace(
          target.take().expect("prepared target"),
          state,
          &config,
        )
        .expect("workspace intent");
      },
      |state, index, ()| {
        assert_eq!(
          monitor
            .displayed_workspace()
            .expect("displayed workspace")
            .id(),
          workspaces[usize::from(index % 2 == 0)].id()
        );
        assert_eq!(
          state.recent_workspace_name.as_deref(),
          Some(if index % 2 == 0 { "1" } else { "2" })
        );
        assert!(
          state.pending_sync.needs_focus_update()
            && state.pending_sync.needs_cursor_jump()
        );
        assert_eq!(state.pending_sync.containers_to_redraw().len(), 2);
      },
      |_, ()| {},
    );
    let floor = traffic_floor(&spec, 32);
    cases.push((spec, measured, floor));

    let (mut state, _, _, windows) = fixture(count, &native);
    let nodes = state
      .root_container
      .descendants()
      .filter_map(|node| node.as_tiling_container().ok())
      .collect::<Vec<_>>();
    let initial_sizes = nodes
      .iter()
      .map(TilingSizeGetters::tiling_size)
      .collect::<Vec<_>>();
    let target: WindowContainer = windows[0].clone().into();
    let deltas = [LengthValue::from_px(1), LengthValue::from_px(-1)];
    for index in 0..256 {
      resize_window(
        &target,
        Some(deltas[index % 2].clone()),
        None,
        &mut state,
      )
      .expect("golden resize burst");
    }
    let expected_sizes = nodes
      .iter()
      .map(TilingSizeGetters::tiling_size)
      .collect::<Vec<_>>();
    let spec = specification(
      "resize-command-burst-256",
      count,
      &state,
      warmups,
      samples,
      repetitions,
    );
    let measured = measure(
      &mut state,
      &spec,
      |state, _| {
        state.pending_sync.clear();
        for (node, size) in nodes.iter().zip(&initial_sizes) {
          node.set_tiling_size(*size);
        }
      },
      |state, ()| {
        for index in 0..256 {
          resize_window(
            &target,
            Some(deltas[index % 2].clone()),
            None,
            state,
          )
          .expect("resize intent");
        }
      },
      |state, _, ()| {
        assert!(state.pending_sync.has_changes());
        for (node, expected) in nodes.iter().zip(&expected_sizes) {
          assert_eq!(node.tiling_size(), *expected);
        }
      },
      |_, ()| {},
    );
    let floor = traffic_floor(&spec, 256 * 8);
    cases.push((spec, measured, floor));

    let (mut state, config, monitor, windows) = fixture(count, &native);
    let parent = windows[0].parent().expect("creation parent");
    let removed_id = windows[0].id();
    let expected_rect =
      windows[0].to_rect().expect("golden restored rectangle");
    detach_container(windows[0].clone().into())
      .expect("prepare N-1 creation fixture");
    assert_eq!(count_windows(&state.root_container), count - 1);
    let spec = specification(
      "checked-creation-intent",
      count,
      &state,
      warmups,
      samples,
      repetitions,
    );
    let measured = measure(
      &mut state,
      &spec,
      |state, _| {
        state.pending_sync.clear();
        Some((
          native.clone(),
          NativeWindowProperties::mock()
            .title("fixture-created".into())
            .call(),
        ))
      },
      |state, input| {
        let (native, properties) = input
          .take()
          .expect("prepared native identity and properties");
        create_window_with_checked_monitor(
          native,
          properties,
          Some(parent.clone()),
          &monitor,
          state,
          &config,
        )
        .expect("checked creation intent")
      },
      |state, _, created| {
        assert_eq!(count_windows(&state.root_container), count);
        assert_ne!(
          created.id(),
          removed_id,
          "creation allocates a fresh managed identity"
        );
        assert_eq!(
          created.parent().expect("created parent").id(),
          parent.id()
        );
        assert_eq!(
          created.to_rect().expect("created rectangle"),
          expected_rect
        );
      },
      |state, created| {
        detach_container(created.into())
          .expect("remove creation output outside timing");
        assert_eq!(count_windows(&state.root_container), count - 1);
      },
    );
    let floor = traffic_floor(&spec, 32);
    cases.push((spec, measured, floor));

    let snapshot = LayoutSnapshot::capture(&state.root_container)
      .expect("fixture cleanup layout");
    for window in state.windows() {
      snapshot
        .rect(window.id())
        .expect("remaining fixture rectangle");
    }
  }
  cases
}

/// Checks all measured cases with small budgets before release
/// measurements.
#[test]
fn case_protocol_and_cleanup_are_valid() {
  let cases = cases(2, 4, 1);
  assert_eq!(cases.len(), 12);
  for (spec, samples, floor) in cases {
    assert!(matches!(spec.window_count, 10 | 50 | 100));
    assert_eq!(samples.len(), 4);
    assert!(samples.iter().all(|sample| sample.outcome == "success"));
    assert!(floor.complete_floor_ns.is_none());
  }
}
