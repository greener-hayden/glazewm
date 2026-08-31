use std::time::{Duration, Instant};

use tracing::info;
use wm_common::{try_warn, WindowRuleEvent};
use wm_platform::NativeWindow;

use crate::{
  commands::window::run_window_rules, traits::WindowGetters,
  user_config::UserConfig, wm_state::WmState,
};

/// Shortest gap between two title reads for the same window.
///
/// Short enough that a title-matching window rule still reacts as the
/// user perceives it, long enough to collapse a burst into one read.
const TITLE_READ_INTERVAL: Duration = Duration::from_millis(200);

pub fn handle_window_title_changed(
  native_window: &NativeWindow,
  state: &mut WmState,
  config: &mut UserConfig,
) -> anyhow::Result<()> {
  let found_window = state.window_from_native(native_window);

  if let Some(window) = found_window {
    // Reading a title is a blocking call into the app that just changed
    // it, on the thread that also handles input. A terminal rewrites its
    // title several times a second while busy, so answering every event
    // means waiting on the slowest app at its slowest moment, over and
    // over. Coalesce: the newest title still lands, just once per gap.
    if window.native_properties().title_read_at.elapsed()
      < TITLE_READ_INTERVAL
    {
      return Ok(());
    }

    info!("Window title changed: {window}");

    let title = try_warn!(window.native().title());

    window.update_native_properties(|properties| {
      properties.title = title;
      properties.title_read_at = Instant::now();
    });

    // Run window rules for title change events.
    run_window_rules(
      window,
      &WindowRuleEvent::TitleChange,
      state,
      config,
    )?;
  }

  Ok(())
}
