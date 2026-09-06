# Placement and presentation

`placement.rs` is the common coordinator for Windows and macOS. Layout
snapshots are final intent; `FrameReconciler` verifies operational native
targets, which can include temporary source parking. Successful submission
does not establish convergence. Native events and fresh observations advance
placement; a per-request 250 ms deadline recovers silence, with three attempts
per unchanged operation. No periodic placement interval is used.

Overlay creation, motion, and source visibility have separate owners:

- `AnimationManager` retains overlay resources independently of prepared
  effect specifications and running motion clocks. Preparing an overlay
  starts no motion clock. A retained overlay can remain stationary before
  release and after motion completion.
- `MotionPreparation` requires a native frame boundary after overlay creation,
  observed placement convergence, and another native boundary after that
  observation. Generation changes invalidate geometry readiness. The 750 ms
  preparation deadline cancels presentation; it never establishes readiness.
- `SourceLease` retains visibility ownership through handoff and recovery.
  A departing workspace is hidden only after its presentation completes.
  Failed restoration cannot retire a cover over a still-parked source.
  Opening windows acquire concealment immediately: their first intended
  frame is transparent, so waiting for a cover would flash the native
  window before the fade-in. Windows sources hide before overlay creation;
  parking backends capture first. Existing visible content still waits for
  its cover. Both paths retain the same geometry and compositor release
  fences, and cloaking precedes native placement writes.

On Windows, `opening_windows` acquires a provisional native session in the
CREATE/first-SHOW event callback, before event batching, window rules, or
layout. Only new ordinary application windows are candidates; the listener
records existing HWNDs and excludes tool, child, minimized, and maximized
windows. This gate runs only while opening animations are enabled and the WM
is unpaused. CREATE attempts concealment early; SHOW refreshes it or retries
if the native surface was not ready. Shell calls can reenter event delivery,
so nested callbacks forward notifications without recursively concealing.

The SHOW notification retains the reservation through native sync. Placement
adopts its existing alpha/cloak ownership without restoration in between;
an inherited source lease restores normally if the final plan declines an
animation. Rejected windows recover when the notification is dropped, and a
750 ms native timer recovers reservations whose SHOW never arrives or whose
consumer stalls. Timers and old notifications cannot release an adopted
session. The registry lock is never held across native mutations. Windows
still delivers out-of-context events asynchronously; this removes the queue
and layout delay, but cannot retract a frame composed before hook delivery.

The sole trigger selector lives on `AnimationTrigger`: skip, workspace slide,
visible opening, then visible movement. Both platforms consume the same plans
and release batches only when all surviving participants are ready. Native
capability rejection falls back to normal placement, not another trigger.

`wm-platform::PlacementSession` implements native operations and recovery.
Windows observes outer native geometry and presents every window through a
live DWM thumbnail. `ConcealMethod` decides how the source hides behind it:
attribute alpha where the window has a redirection surface, shell cloaking
for DirectComposition and layered windows. `reconcile_managed` computes the
one cloak state that workspace hiding and presentation both want, so the two
never fight over the bit. While a source lease is held, the window carries the
`PRESENTED` bit in its `GlazeWM.greener.changes.v1` property; border renderers
read it to hide their ring instead of inferring the motion from alpha or cloak
side effects. macOS observes Window Server bounds rather than AX write echoes,
and uses source parking with captured overlays and Core Animation.

Frame notifications come from DirectComposition/DWM or CVDisplayLink.
Fences use fresh native serials sampled after operations, so queued older
notifications cannot satisfy them. A frame indicates compositor scheduling
progress, not proof that another application finished painting. Core Animation
completion callbacks belong to individual motion instances and cannot complete
a replacement motion after retargeting.

## Verification

Run the standard formatting, Clippy, and workspace test gates on each OS.
The Windows platform harness also provides opt-in fixture tests:

```text
cargo test -p wm-platform --test test live_ -- --ignored --test-threads=1
```

The overlay tests verify native DWM preparation, alpha concealment,
placement, frame progression, restoration, and cleanup. The `_pixels`
variant additionally reads desktop pixels; it needs a desktop that permits
screen capture. The opening variants conceal before overlay creation and
check that transparent preparation stays invisible, the thumbnail can
subsequently appear, and cancellation without an overlay restores the
source. The composition test checks that a source without a
redirection surface is still eligible and is assigned the cloak method. The
tool-window test checks that a source the shell cannot cloak fails cleanly:
no recovery flag, no error on uncloak. Cloaked presentation itself can only
be checked on the desktop, because a fixture the WM ignores is by definition
not an application window with a shell view. Neither replaces
desktop checks for all four triggers, rapid workspace reversal, interrupted
motion, and movement across displays with different scales. macOS CI compiles
the platform backend and runs the shared tests; local Windows results do not
establish macOS desktop behavior.
