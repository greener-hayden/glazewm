//! Transient Windows ownership observations and serialized band-preserving
//! raises.

use std::{
  collections::{HashMap, HashSet},
  sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
  },
};

use windows::Win32::{
  Foundation::{BOOL, HWND, LPARAM},
  UI::{
    Input::KeyboardAndMouse::IsWindowEnabled,
    WindowsAndMessaging::{
      EnumWindows, GetWindow, GetWindowThreadProcessId, IsIconic,
      IsWindowVisible, PeekMessageW, SetWindowPos, GW_OWNER, HWND_TOP,
      MSG, PM_NOREMOVE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER,
      SWP_NOREDRAW, SWP_NOSENDCHANGING, SWP_NOSIZE,
    },
  },
};

use crate::{
  NativeSession, NativeWindow, NativeWindowWindowsExt, WindowId,
  WindowZOrder,
};

/// One live top-level window or retained owner ancestor, never cached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeStackingEntry {
  pub id: WindowId,
  pub owner: Option<WindowId>,
  pub enabled: bool,
  pub visible: bool,
  /// Process ID followed by UI thread ID.
  pub identity: (u32, u32),
}

/// Collects front-to-back HWNDs without filtering out unmanaged dialogs.
fn native_order() -> crate::Result<Vec<WindowId>> {
  /// Appends each enumerated HWND to the synchronous caller's buffer.
  unsafe extern "system" fn collect(
    hwnd: HWND,
    parameter: LPARAM,
  ) -> BOOL {
    // SAFETY: EnumWindows invokes this callback synchronously with the
    // live buffer.
    let ids = unsafe { &mut *(parameter.0 as *mut Vec<WindowId>) };
    ids.push(WindowId(hwnd.0));
    BOOL(1)
  }
  let mut ids = Vec::new();
  // SAFETY: The buffer remains live until all callbacks return.
  unsafe {
    EnumWindows(
      Some(collect),
      LPARAM(std::ptr::from_mut(&mut ids) as isize),
    )
  }?;
  Ok(ids)
}

/// Reads identity and ownership without messaging the owning UI thread.
fn observe(id: WindowId) -> crate::Result<NativeStackingEntry> {
  let mut process = 0;
  // SAFETY: Reads metadata for an enumerated HWND; the output remains
  // live.
  let (thread, owner, enabled, visible) = unsafe {
    (
      GetWindowThreadProcessId(HWND(id.0), Some(&raw mut process)),
      GetWindow(HWND(id.0), GW_OWNER),
      IsWindowEnabled(HWND(id.0)).as_bool(),
      IsWindowVisible(HWND(id.0)).as_bool()
        && !IsIconic(HWND(id.0)).as_bool(),
    )
  };
  if thread == 0 || process == 0 {
    return Err(crate::Error::WindowNotFound);
  }
  let visible = visible && NativeWindow::from_handle(id.0).is_visible()?;
  Ok(NativeStackingEntry {
    id,
    owner: (owner.0 != 0).then_some(WindowId(owner.0)),
    enabled,
    visible,
    identity: (process, thread),
  })
}

/// Observes every top-level window and any owner missing from enumeration.
///
/// Enumerated entries preserve front-to-back order. Owner-only entries are
/// appended, retaining hidden owners without inventing their visible rank.
pub fn native_stacking_context() -> crate::Result<Vec<NativeStackingEntry>>
{
  let mut entries = native_order()?
    .into_iter()
    .map(observe)
    .collect::<crate::Result<Vec<_>>>()?;
  let mut seen =
    entries.iter().map(|entry| entry.id).collect::<HashSet<_>>();
  let mut index = 0;
  while index < entries.len() {
    if let Some(owner) = entries[index].owner {
      if seen.insert(owner) {
        entries.push(observe(owner)?);
      }
    }
    index += 1;
  }
  Ok(entries)
}

/// Resolves a root in a checked observation index without allocating per
/// ancestor.
fn family_root(
  by_id: &HashMap<WindowId, &NativeStackingEntry>,
  id: WindowId,
) -> crate::Result<WindowId> {
  let mut current = id;
  for _ in 0..by_id.len() {
    match by_id
      .get(&current)
      .ok_or(crate::Error::WindowNotFound)?
      .owner
    {
      Some(owner) => current = owner,
      None => return Ok(current),
    }
  }
  Err(crate::Error::Platform("Cyclic native ownership.".into()))
}

/// Groups complete connected families once for one transient observation.
fn families(
  entries: &[NativeStackingEntry],
) -> crate::Result<HashMap<WindowId, Vec<NativeStackingEntry>>> {
  let by_id = entries
    .iter()
    .map(|entry| (entry.id, entry))
    .collect::<HashMap<_, _>>();
  let mut families: HashMap<_, Vec<_>> = HashMap::new();
  for entry in entries {
    families
      .entry(family_root(&by_id, entry.id)?)
      .or_default()
      .push(entry.clone());
  }
  for family in families.values_mut() {
    family.sort_by_key(|entry| entry.id);
  }
  Ok(families)
}

/// Plans a back-to-front prefix of floats, preserving their mutual order.
///
/// Visible owned descendants and unmanaged owned windows ahead of a float
/// are barriers. Ownership outranks the preference; owned dialogs
/// themselves remain eligible when raising them does not cross another
/// such barrier.
pub fn floating_raise_plan(
  entries: &[NativeStackingEntry],
  floats: &[WindowId],
  tiles: &[WindowId],
) -> crate::Result<Vec<WindowId>> {
  let ranks = entries
    .iter()
    .enumerate()
    .map(|(rank, entry)| (entry.id, rank))
    .collect::<HashMap<_, _>>();
  let by_id = entries
    .iter()
    .map(|entry| (entry.id, entry))
    .collect::<HashMap<_, _>>();
  for id in floats.iter().chain(tiles) {
    family_root(&by_id, *id)?;
  }
  let ordered = entries
    .iter()
    .filter(|entry| floats.contains(&entry.id) && entry.visible)
    .collect::<Vec<_>>();
  let mut prefix = Vec::new();
  for source in &ordered {
    let rank = ranks[&source.id];
    let owned_descendant = entries
      .iter()
      .any(|entry| entry.owner == Some(source.id) && entry.visible);
    let owned_barrier = entries[..rank].iter().any(|entry| {
      entry.visible && entry.owner.is_some() && !floats.contains(&entry.id)
    });
    if !source.enabled || owned_descendant || owned_barrier {
      #[cfg(test)]
      eprintln!("floating blocked {:?}: enabled={}, descendants={owned_descendant}, barrier={owned_barrier}", source.id, source.enabled);
      break;
    }
    prefix.push(source.id);
  }
  let violates = prefix.iter().any(|id| {
    tiles.iter().any(|tile| {
      entries
        .iter()
        .find(|entry| entry.id == *tile)
        .is_some_and(|entry| entry.visible)
        && ranks[tile] < ranks[id]
    })
  });
  if !violates {
    return Ok(Vec::new());
  }
  prefix.reverse();
  Ok(prefix)
}

/// One submitted operation, with weak recovery ownership and transient
/// inputs.
struct StackingJob {
  actors: Vec<NativeSession>,
  groups: Vec<(Vec<WindowId>, Vec<WindowId>)>,
  expected: Vec<NativeStackingEntry>,
}

/// Revalidates affected families before applying one serialized raise
/// group.
fn apply(job: &StackingJob) -> crate::Result<()> {
  for actor in &job.actors {
    if !actor.stacking_ready()? {
      return Err(crate::Error::Platform(
        "Source presentation changed.".into(),
      ));
    }
  }
  #[cfg(test)]
  eprintln!("floating: observe current");
  let current = native_stacking_context()?;
  #[cfg(test)]
  eprintln!("floating: observed {} entries", current.len());
  let current_index = current
    .iter()
    .map(|entry| (entry.id, entry))
    .collect::<HashMap<_, _>>();
  let expected_index = job
    .expected
    .iter()
    .map(|entry| (entry.id, entry))
    .collect::<HashMap<_, _>>();
  let current_families = families(&current)?;
  let expected_families = families(&job.expected)?;
  for id in job
    .groups
    .iter()
    .flat_map(|(floats, tiles)| floats.iter().chain(tiles))
  {
    if current_families.get(&family_root(&current_index, *id)?)
      != expected_families.get(&family_root(&expected_index, *id)?)
    {
      return Err(crate::Error::Platform(
        "Native ownership or visibility changed.".into(),
      ));
    }
  }
  let indexed = job
    .actors
    .iter()
    .map(|actor| {
      let window = actor.window()?;
      Ok((
        window.id(),
        (actor, window.has_z_order(&WindowZOrder::Normal, &[])),
      ))
    })
    .collect::<crate::Result<HashMap<_, _>>>()?;
  for (floats, tiles) in &job.groups {
    for id in floats.iter().chain(tiles) {
      if !indexed.contains_key(id) {
        return Err(crate::Error::WindowNotFound);
      }
    }
    let floats = floats
      .iter()
      .copied()
      .filter(|id| indexed.get(id).is_some_and(|(_, normal)| *normal))
      .collect::<Vec<_>>();
    let tiles = tiles
      .iter()
      .copied()
      .filter(|id| indexed.get(id).is_some_and(|(_, normal)| *normal))
      .collect::<Vec<_>>();
    for id in floating_raise_plan(&current, &floats, &tiles)? {
      let (actor, _) =
        indexed.get(&id).ok_or(crate::Error::WindowNotFound)?;
      if !actor.stacking_ready()? {
        return Err(crate::Error::Platform(
          "Source presentation changed.".into(),
        ));
      }
      actor.validate()?;
      // SAFETY: The live recovery session binds the HWND to its
      // process/thread. HWND_TOP preserves its current native band
      // even after a concurrent style change. Suppressing changing
      // and paint callbacks avoids a blocked app UI; no activation,
      // geometry, visibility, or owner repositioning is requested.
      #[cfg(test)]
      eprintln!("floating: raise {id:?}");
      unsafe {
        SetWindowPos(
          HWND(id.0),
          HWND_TOP,
          0,
          0,
          0,
          0,
          SWP_NOMOVE
            | SWP_NOSIZE
            | SWP_NOACTIVATE
            | SWP_NOOWNERZORDER
            | SWP_NOSENDCHANGING
            | SWP_NOREDRAW,
        )?;
      }
      #[cfg(test)]
      eprintln!("floating: raised {id:?}");
    }
  }
  Ok(())
}

/// Owns one in-flight native job without blocking the WM event loop.
///
/// The coordinator retains pending intent until completion, preventing a
/// new source overlay from racing a raise. Completion is a wake, not a
/// display frame. The worker never polls, retries, caches config, or
/// restores order.
pub struct NativeStackingWorker {
  sender: mpsc::SyncSender<StackingJob>,
  busy: Arc<AtomicBool>,
}

#[cfg(test)]
#[path = "floating_stacking_tests.rs"]
mod tests;

impl NativeStackingWorker {
  /// Creates an in-flight barrier for native-free coordinator tests.
  #[cfg(feature = "test_utils")]
  #[must_use]
  pub fn mock_busy() -> Self {
    let (sender, _) = mpsc::sync_channel(1);
    Self {
      sender,
      busy: Arc::new(AtomicBool::new(true)),
    }
  }

  /// Starts an idle worker; callers create it only for an eligible
  /// operation.
  pub fn start(
    on_complete: impl Fn() + Send + 'static,
  ) -> crate::Result<Self> {
    let (sender, receiver) = mpsc::sync_channel::<StackingJob>(1);
    let busy = Arc::new(AtomicBool::new(false));
    let worker_busy = busy.clone();
    std::thread::Builder::new()
      .name("floating-stacking".into())
      .spawn(move || {
        let mut message = MSG::default();
        // SAFETY: Establishes only this worker's GUI queue; does not
        // dispatch work.
        unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_NOREMOVE) };
        while let Ok(job) = receiver.recv() {
          if let Err(error) = apply(&job) {
            #[cfg(test)]
            eprintln!("floating apply rejected: {error}");
            tracing::debug!(?error, "Floating stacking skipped.");
          }
          drop(job);
          worker_busy.store(false, Ordering::Release);
          on_complete();
        }
      })
      .map_err(|error| crate::Error::Platform(error.to_string()))?;
    Ok(Self { sender, busy })
  }

  /// Reports whether native work still owns the coordinator's commit
  /// barrier.
  #[must_use]
  pub fn is_busy(&self) -> bool {
    self.busy.load(Ordering::Acquire)
  }

  /// Submits at most one job; a busy worker never accumulates a backlog.
  pub fn submit(
    &self,
    actors: Vec<NativeSession>,
    groups: Vec<(Vec<WindowId>, Vec<WindowId>)>,
    expected: Vec<NativeStackingEntry>,
  ) -> crate::Result<()> {
    if self
      .busy
      .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
      .is_err()
    {
      return Err(crate::Error::Platform(
        "Native stacking is busy.".into(),
      ));
    }
    if let Err(error) = self.sender.try_send(StackingJob {
      actors,
      groups,
      expected,
    }) {
      self.busy.store(false, Ordering::Release);
      return Err(crate::Error::Platform(error.to_string()));
    }
    Ok(())
  }
}
