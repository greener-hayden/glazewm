// The `windows` or `console` subsystem (default is `console`) determines
// whether a console window is spawned on launch, if not already ran
// through a console. The following prevents this additional console window
// in release mode.
#![cfg_attr(
  all(not(debug_assertions), target_os = "windows"),
  windows_subsystem = "windows"
)]
#![warn(clippy::all, clippy::pedantic)]
#![feature(iterator_try_collect)]

#[cfg(target_os = "macos")]
use std::io::IsTerminal;
use std::{env, path::PathBuf, process, time::Duration};

use anyhow::{Context, Error};
use tokio::{process::Command, signal};
use tracing_subscriber::{
  filter::LevelFilter,
  fmt,
  layer::{Layer, SubscriberExt},
};
use wm_common::{AppCommand, InvokeCommand, Verbosity, WmEvent};
#[cfg(target_os = "macos")]
use wm_platform::DispatcherExtMacOs;
use wm_platform::{
  Dispatcher, DisplayListener, EventLoop, KeybindingListener,
  MouseEventKind, MouseListener, PlatformEvent, SingleInstance,
  WindowListener,
};

use crate::{
  ipc_server::IpcServer, sys_tray::SystemTray, user_config::UserConfig,
  wm::WindowManager,
};

mod animation_manager;
mod commands;
mod event_batch;
mod events;
mod ipc_server;
mod layout_snapshot;
mod models;
mod native_reconciler;
mod pending_sync;
mod placement;
mod presentation;
mod sys_tray;
mod traits;
mod user_config;
mod wm;
mod wm_state;

#[cfg(test)]
mod benchmarks;
#[cfg(test)]
mod test_utils;

/// Main entry point for the application.
///
/// Conditionally starts the WM or runs a CLI command based on the given
/// subcommand.
fn main() -> anyhow::Result<()> {
  let args = std::env::args().collect::<Vec<_>>();
  let app_command = AppCommand::parse_with_default(&args);

  if let AppCommand::Start {
    config_path,
    verbosity,
  } = app_command
  {
    let rt = tokio::runtime::Runtime::new()?;
    let (event_loop, dispatcher) = EventLoop::new()?;

    let task_handle = std::thread::spawn(move || {
      rt.block_on(async {
        let start_res =
          start_wm(config_path, verbosity, &dispatcher).await;

        if let Err(err) = &start_res {
          // If unable to start the WM, the error is fatal and a message
          // dialog is shown.
          tracing::error!("{:?}", err);
          dispatcher.show_error_dialog("Fatal error", &err.to_string());
        }

        if let Err(err) = dispatcher.stop_event_loop() {
          // Forcefully exit the process to ensure the event loop is
          // stopped.
          tracing::error!("Failed to stop event loop gracefully: {}", err);
          process::exit(1);
        }

        start_res
      })
    });

    // Run event loop (blocks until shutdown). This must be on the main
    // thread for macOS compatibility.
    event_loop.run()?;

    // Wait for clean exit of the WM.
    task_handle.join().unwrap()
  } else {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(wm_cli::start(args))
  }
}

#[allow(clippy::too_many_lines)]
async fn start_wm(
  config_path: Option<PathBuf>,
  verbosity: Verbosity,
  dispatcher: &Dispatcher,
) -> anyhow::Result<()> {
  setup_logging(&verbosity)?;

  // Ensure that only one instance of the WM is running.
  let _single_instance = SingleInstance::new()?;

  #[cfg(target_os = "macos")]
  {
    if !dispatcher.has_ax_permission(true) {
      anyhow::bail!(
        "Accessibility permissions are not granted. In System Preferences, \
         go to Privacy & Security > Accessibility and enable GlazeWM."
      );
    }
  }

  // Parse and validate user config.
  let mut config = UserConfig::new(config_path)?;

  // Add application icon to system tray.
  let mut tray = SystemTray::new(&config.path, dispatcher.clone())?;

  let mut wm = WindowManager::new(&mut config, dispatcher.clone())?;

  let mut ipc_server = IpcServer::start().await?;

  // On Windows, start watcher process for restoring hidden windows on
  // crash. macOS' hidden windows are always accessible.
  #[cfg(target_os = "windows")]
  if let Err(err) = start_watcher_process() {
    tracing::warn!(
      "Failed to start watcher process: {err}{}",
      cfg!(debug_assertions)
        .then_some(".\n Run `cargo build -p wm-watcher` to build it.")
        .unwrap_or_default()
    );
  }

  // On macOS, update the current process' PATH variable so that
  // `shell-exec` can resolve programs defined in the shell's PATH. Skip if
  // running via a terminal.
  #[cfg(target_os = "macos")]
  if !std::io::stdin().is_terminal() {
    update_path_env();
  }

  // Start listening for platform events after populating initial state.
  let mut window_listener = WindowListener::new(dispatcher)?;
  let mut display_listener = DisplayListener::new(dispatcher)?;
  let mut mouse_listener = MouseListener::new(
    if config.value.general.focus_follows_cursor {
      &[MouseEventKind::Move, MouseEventKind::LeftButtonUp]
    } else {
      &[MouseEventKind::LeftButtonUp]
    },
    dispatcher,
  )?;
  let mut keybinding_listener =
    KeybindingListener::new(&config.listener_bindings(&[]), dispatcher)?;

  start_event_loop_watchdog(dispatcher);

  // Run user's startup commands.
  if let Err(err) = wm.process_commands(
    &config.value.general.startup_commands.clone(),
    None,
    &mut config,
  ) {
    tracing::error!("{:?}", err);
    dispatcher.show_error_dialog("Non-fatal error", &err.to_string());
  }

  let mut cleanup_interval = tokio::time::interval(Duration::from_secs(5));
  cleanup_interval
    .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

  let mut topology = event_batch::TopologyDebounce::default();
  let mut input_failure = None;
  loop {
    window_listener.set_opening_concealment(
      !wm.state.is_paused && config.value.animations.window_open.is_some(),
    );
    let topology_deadline = topology.deadline();
    let follow_deadline = wm.state.pending_follow_deadline();
    let placement_deadline =
      wm.state.native_sync.deadline(wm.state.is_paused);

    let res = tokio::select! {
      _ = signal::ctrl_c() => {
        tracing::info!("Received SIGINT signal.");
        break;
      },
      Some(()) = wm.exit_rx.recv() => {
        tracing::info!("Exiting through WM command.");
        break;
      },
      Some(()) = tray.exit_rx.recv() => {
        tracing::info!("Exiting through system tray.");
        break;
      },
      Some(event) = mouse_listener.next_event() => {
        tracing::debug!("Received mouse event: {:?}", event);
        wm.process_event(PlatformEvent::Mouse(event), &mut config)
      },
      Some(event) = window_listener.next_event() => {
        tracing::debug!("Received window event: {:?}", event);
        let mut batch = event_batch::WindowBatch::default();
        batch.push(event);
        for _ in 1..64 {
          let Some(event) = window_listener.try_next_event() else { break; };
          batch.push(event);
        }
        wm.process_window_batch(batch, &mut config)
      },
      Some(()) = display_listener.next_event() => {
        tracing::debug!("Received display settings changed event.");
        topology.notify(std::time::Instant::now());
        Ok(())
      },
      event = keybinding_listener.next_event() => {
        let Some(event) = event else {
          input_failure = Some(anyhow::anyhow!("Keyboard input stopped."));
          break;
        };
        tracing::debug!("Received keyboard event: {:?}", event);
        wm.process_event(PlatformEvent::Keybinding(event), &mut config)
      }
      () = wait_until(placement_deadline) => {
        wm.recover_placement(&config)
      },
      _ = cleanup_interval.tick() => {
        let dropped = keybinding_listener.take_dropped();
        if dropped != 0 {
          tracing::warn!("Dropped {dropped} keyboard commands.");
        }
        if wm.state.is_paused {
          Ok(())
        } else {
          wm.state.cleanup_invalid_windows()
        }
      },
      () = wait_until(topology_deadline) => {
        if topology.take_due(std::time::Instant::now()) {
          wm.process_event(PlatformEvent::DisplaySettingsChanged, &mut config)
        } else { Ok(()) }
      },
      () = wait_until(follow_deadline) => {
        if wm.state.is_paused {
          wm.state.cancel_pending_follow();
          Ok(())
        } else {
          wm.commit_pending_follow(&config)
        }
      },
      Some(signal) = wm.state.animation_manager.tick_rx.recv() => {
        wm.update_animations(signal, &config)
      },
      Some((
        message,
        response_tx,
        disconnection_tx
      )) = ipc_server.message_rx.recv() => {
        tracing::info!("Received IPC message: {:?}", message);

        if let Err(err) = ipc_server.process_message(
          message,
          &response_tx,
          &disconnection_tx,
          &mut wm,
          &mut config,
        ) {
          tracing::error!("{:?}", err);
        }

        Ok(())
      },
      Some(wm_event) = wm.event_rx.recv() => {
        tracing::debug!("Received WM event: {:?}", wm_event);

        // Disable mouse listener when the WM is paused.
        if let WmEvent::PauseChanged { is_paused } = wm_event {
          let _ = mouse_listener.enable(!is_paused);
        }

        // Update keybinding and mouse listeners on config changes.
        if matches!(
          wm_event,
          WmEvent::UserConfigChanged { .. }
            | WmEvent::BindingModesChanged { .. }
            | WmEvent::PauseChanged { .. }
        ) {
          if let Err(error) = keybinding_listener.update(
            &config.listener_bindings(&wm.state.binding_modes),
          ) {
            input_failure = Some(error.into());
            break;
          }

          mouse_listener.set_enabled_events(
            if config.value.general.focus_follows_cursor {
              &[MouseEventKind::Move, MouseEventKind::LeftButtonUp]
            } else {
              &[MouseEventKind::LeftButtonUp]
            },
          )?;
        }

        if let Err(err) = ipc_server.process_event(wm_event) {
          tracing::error!("{:?}", err);
        }

        Ok(())
      },
      Some(()) = tray.config_reload_rx.recv() => {
        wm.process_commands(
          &vec![InvokeCommand::WmReloadConfig],
          None,
          &mut config,
        ).map(|_| ())
      },
    };

    if let Err(err) = res {
      tracing::error!("{:?}", err);
      dispatcher.show_error_dialog("Non-fatal error", &err.to_string());
    }
  }

  tracing::info!("Window manager shutting down.");
  #[cfg(target_os = "windows")]
  if let Err(error) = keybinding_listener.terminate() {
    input_failure = Some(error.into());
  }
  wm.cleanup(&mut config, &mut ipc_server);

  input_failure.map_or(Ok(()), Err)
}

/// Threshold for reporting window-dispatch stalls.
const STALL_THRESHOLD: Duration = Duration::from_millis(2500);

/// Reports window-dispatch stalls, not keyboard delivery.
fn start_event_loop_watchdog(dispatcher: &Dispatcher) {
  let dispatcher = dispatcher.clone();
  std::thread::spawn(move || {
    let mut stalled_since: Option<std::time::Instant> = None;
    loop {
      std::thread::sleep(Duration::from_secs(1));
      let started = std::time::Instant::now();
      let answered = dispatcher.dispatch_sync(|| {}).is_ok();
      let waited = started.elapsed();

      if !answered || waited >= STALL_THRESHOLD {
        // Report the leading edge once, not every second, so a long stall
        // is one line rather than a flood.
        if stalled_since.is_none() {
          stalled_since = Some(started);
          tracing::error!(
            "Window dispatcher stalled: {}ms{}.",
            waited.as_millis(),
            if answered { "" } else { " (timed out)" }
          );
        }
      } else if let Some(since) = stalled_since.take() {
        tracing::error!(
          "Event loop recovered after {}ms.",
          since.elapsed().as_millis()
        );
      }
    }
  });
}

/// Waits until `deadline`, or never resolves when no deadline exists.
///
/// Keeps deferred off-screen follows event-driven while idle.
async fn wait_until(deadline: Option<std::time::Instant>) {
  match deadline {
    Some(deadline) => {
      tokio::time::sleep_until(tokio::time::Instant::from_std(deadline))
        .await;
    }
    None => std::future::pending::<()>().await,
  }
}

/// Initialize logging with the specified verbosity level.
///
/// Error logs are saved to `~/.glzr/glazewm/errors.log`.
fn setup_logging(verbosity: &Verbosity) -> anyhow::Result<()> {
  let error_log_dir = home::home_dir()
    .context("Unable to get home directory.")?
    .join(".glzr/glazewm/");

  let error_writer =
    tracing_appender::rolling::never(error_log_dir, "errors.log");

  // Filter per layer rather than per writer. A writer filter formats
  // every event before discarding it, so each disabled `debug!` still
  // paid for its `Debug` output, including LaunchServices round trips
  // for `NSRunningApplication`. Layer filters also lower the global max
  // level, which disables those callsites outright.
  let subscriber = tracing_subscriber::registry()
    .with(
      // Output to stdout with specified verbosity level.
      fmt::Layer::new()
        .with_writer(std::io::stdout)
        .with_filter(LevelFilter::from_level(verbosity.level())),
    )
    .with(
      // Output to error log file.
      fmt::Layer::new()
        .with_writer(error_writer)
        .with_filter(LevelFilter::ERROR),
    );

  tracing::subscriber::set_global_default(subscriber)?;

  tracing::info!(
    "Starting WM with log level {:?}.",
    verbosity.level().to_string()
  );

  Ok(())
}

/// Launches watcher binary (Windows-only). This is a separate process that
/// is responsible for restoring hidden windows in case the main WM process
/// crashes.
///
/// This assumes the watcher binary exists in the same directory as the
/// WM binary.
#[allow(unused)]
fn start_watcher_process() -> anyhow::Result<tokio::process::Child, Error>
{
  let watcher_path = env::current_exe()?
    .parent()
    .context("Failed to resolve path to the watcher process.")?
    .join("glazewm-watcher");

  Command::new(&watcher_path)
    .spawn()
    .context("Failed to start watcher process.")
}

/// Updates the current process' PATH by querying the login shell.
///
/// Apps launched outside a terminal (Spotlight, Finder, login items)
/// inherit a PATH that only contains `/usr/bin:/bin:/usr/sbin:/sbin`. This
/// causes `shell-exec` to fail for binaries that aren't in the system
/// PATH.
#[cfg(target_os = "macos")]
fn update_path_env() {
  let shell =
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());

  // Use `-l` and `-i` (login + interactive) so that both profile and rc
  // files are sourced.
  let path_var = match std::process::Command::new(&shell)
    .args(["-lic", "printf '%s' \"$PATH\""])
    .output()
  {
    Ok(output) if output.status.success() => {
      String::from_utf8(output.stdout)
        .ok()
        .filter(|path| !path.is_empty())
    }
    _ => None,
  };

  if let Some(path) = path_var {
    std::env::set_var("PATH", path);
  } else {
    tracing::warn!(
      "Failed to query login shell for PATH. Keeping existing PATH."
    );
  }
}
