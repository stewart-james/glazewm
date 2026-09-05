use anyhow::Context;
#[cfg(target_os = "macos")]
use wm_common::try_warn;
use wm_platform::{MouseButton, MouseEvent, Point};

use crate::{
  commands::container::set_focused_descendant,
  traits::{CommonGetters, PositionGetters},
  user_config::UserConfig,
  wm_state::WmState,
};
#[cfg(target_os = "macos")]
use crate::{
  events::handle_window_moved_or_resized_end, traits::WindowGetters,
};

pub fn handle_mouse_move(
  event: &MouseEvent,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  // Ignore mouse move events if the WM is paused. The mouse listener
  // should anyways be disabled when the WM is paused, but this is just in
  // case any events slipped through while disabling.
  if state.is_paused {
    return Ok(());
  }

  // On macOS, detect when a window drag operation has ended by listening
  // to the release of left click.
  //
  // This cannot be used for Windows, since it leads to race conditions
  // where the mouse event comes in before the `MovedOrResized` event with
  // `is_interactive_end`. For example, if the user drags to maximize a
  // window, the WS_MAXIMIZED state is sometimes set after the mouse event.
  #[cfg(target_os = "macos")]
  if let MouseEvent::ButtonUp { button, .. } = event {
    if *button == MouseButton::Left {
      let active_drag_windows = state
        .windows()
        .into_iter()
        .filter(|window| window.active_drag().is_some());

      // Only one window should ever be actively dragged at a time, but
      // just in case, iterate over all active drag windows.
      for window in active_drag_windows {
        let new_rect = try_warn!(window.native().frame());

        window.update_native_properties(|properties| {
          properties.frame = new_rect;
        });

        handle_window_moved_or_resized_end(&window, state, config)?;
      }
    }

    return Ok(());
  }

  // A click that doesn't land on any window is otherwise invisible to the
  // WM (there's no native focus event for it), but it's still a useful
  // signal: it's how a user marks an (often empty) area as where they
  // want the next opened window to go. On macOS this is unreachable,
  // since the block above already returns on every left-click release.
  if let MouseEvent::ButtonUp {
    button: MouseButton::Left,
    position,
    ..
  } = event
  {
    handle_area_click(position, state)?;
  }

  if let MouseEvent::Move {
    pressed_buttons,
    // LINT: `window_below_cursor` is only used on macOS.
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    window_below_cursor,
    position,
    ..
  } = event
  {
    // Ignore event if left/right-click is down. Otherwise, this causes
    // focus to jitter when a window is being resized by its drag
    // handles. Also ignore if the OS focused window isn't the same as
    // the WM's focused window.
    if pressed_buttons.contains(&MouseButton::Left)
      || pressed_buttons.contains(&MouseButton::Right)
      || !state.is_focus_synced
      || !config.value.general.focus_follows_cursor
    {
      return Ok(());
    }

    let window_under_cursor = {
      #[cfg(target_os = "macos")]
      {
        window_below_cursor.and_then(|window_id| {
          use crate::traits::WindowGetters;

          state
            .windows()
            .into_iter()
            .find(|w| w.native().id() == window_id)
        })
      }
      #[cfg(target_os = "windows")]
      {
        state
          .dispatcher
          .window_from_point(position)?
          .and_then(|native| state.window_from_native(&native))
      }
    };

    // Set focus to whichever window is currently under the cursor.
    if let Some(window) = window_under_cursor {
      let focused_container =
        state.focused_container().context("No focused container.")?;

      if focused_container.id() != window.id() {
        set_focused_descendant(&window.as_container(), None);
        state.pending_sync.queue_focus_change();
      }
    } else {
      // Focus the monitor if no window is under the cursor.
      let cursor_monitor = state
        .monitor_at_point(position)
        .context("No monitor under cursor.")?;

      let focused_monitor = state
        .focused_container()
        .context("No focused container.")?
        .monitor()
        .context("Focused container has no monitor.")?;

      // Avoid setting focus to the same monitor.
      if cursor_monitor.id() != focused_monitor.id() {
        set_focused_descendant(&cursor_monitor.as_container(), None);
        state.pending_sync.queue_focus_change();
      }
    }
  }

  Ok(())
}

/// Records the workspace's clicked-on area (if any) so that the next
/// opened window is placed there. See `Workspace::last_active_area`.
fn handle_area_click(
  position: &Point,
  state: &mut WmState,
) -> anyhow::Result<()> {
  // If the click landed on a window, that window's own focus already
  // reflects the user's intent - nothing extra to track here.
  if state.dispatcher.window_from_point(position)?.is_some() {
    return Ok(());
  }

  let Some(workspace) = state
    .monitor_at_point(position)
    .and_then(|monitor| monitor.displayed_workspace())
  else {
    return Ok(());
  };

  let clicked_area = workspace.tiling_children().find_map(|child| {
    let area = child.as_split()?.clone();

    if !area.is_area() {
      return None;
    }

    area
      .to_rect()
      .ok()
      .filter(|rect| rect.contains_point(position))
      .map(|_| area)
  });

  if let Some(area) = clicked_area {
    workspace.set_last_active_area_id(Some(area.id()));
  }

  Ok(())
}
