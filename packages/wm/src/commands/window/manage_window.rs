use anyhow::Context;
use tracing::info;
use wm_common::{try_warn, WindowRuleEvent, WindowState, WmEvent};
use wm_platform::{NativeWindow, RectDelta};

use crate::{
  commands::{
    container::{attach_container, set_focused_descendant},
    window::run_window_rules,
  },
  models::{
    Container, Monitor, NativeWindowProperties, NonTilingWindow,
    TilingWindow, WindowContainer,
  },
  traits::{CommonGetters, PositionGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

pub fn manage_window(
  native_window: NativeWindow,
  target_parent: Option<Container>,
  state: &mut WmState,
  config: &mut UserConfig,
) -> anyhow::Result<()> {
  let Some(native_properties) =
    check_is_manageable(&native_window).unwrap_or(None)
  else {
    return Ok(());
  };

  // Create the window instance. This may fail if the window handle has
  // already been destroyed.
  let window = try_warn!(create_window(
    native_window,
    native_properties,
    target_parent,
    state,
    config
  ));

  // Set the newly added window as focus descendant. This means the window
  // rules will be run as if the window is focused.
  set_focused_descendant(&window.clone().into(), None);

  // Window might be detached if `ignore` command has been invoked.
  let updated_window = run_window_rules(
    window.clone(),
    &WindowRuleEvent::Manage,
    state,
    config,
  )?;

  if let Some(window) = updated_window {
    info!("New window managed: {window}");

    state.emit_event(WmEvent::WindowManaged {
      managed_window: window.to_dto()?,
    });

    // OS focus should be set to the newly added window in case it's not
    // already focused.
    state.pending_sync.queue_focus_change();

    // Normally, a `PlatformEvent::WindowFocused` event is what triggers
    // focus effects and workspace reordering to be applied. However, when
    // a window is first launched, this event can come before the
    // window is managed, and so we need to force an update here.
    state.pending_sync.queue_focused_effect_update();
    state.pending_sync.queue_workspace_to_reorder(
      window.workspace().context("No workspace.")?,
    );

    // Sibling containers need to be redrawn if the window is tiling.
    state.pending_sync.queue_container_to_redraw(
      if window.state() == WindowState::Tiling {
        window.parent().context("No parent.")?
      } else {
        window.into()
      },
    );
  }

  Ok(())
}

/// Checks if a window is manageable and retrieves its native properties.
///
/// Returns `Ok(Some(properties))` if the window is manageable and its
/// properties were retrieved successfully.
fn check_is_manageable(
  native_window: &NativeWindow,
) -> anyhow::Result<Option<NativeWindowProperties>> {
  if !native_window.is_visible()? {
    return Ok(None);
  }

  #[cfg(target_os = "macos")]
  {
    use wm_platform::NativeWindowExtMacOs;

    let is_standard_window = native_window.role()? == "AXWindow"
      && native_window.subrole()? == "AXStandardWindow";

    if !is_standard_window {
      return Ok(None);
    }
  }

  // Ensure window has a valid process name, title, etc.
  let native_properties = NativeWindowProperties::try_from(native_window)?;

  #[cfg(target_os = "windows")]
  {
    use wm_platform::{
      NativeWindowWindowsExt, WS_CAPTION, WS_CHILD, WS_EX_NOACTIVATE,
      WS_EX_TOOLWINDOW,
    };

    // TODO: Temporary fix for managing Flow Launcher until a force manage
    // command is added.
    let is_flow_launcher = native_properties.process_name
      == "Flow.Launcher"
      && native_properties.title == "Flow.Launcher";

    if !is_flow_launcher {
      // Ensure window is top-level (i.e. not a child window). Ignore
      // windows that cannot be focused or if they're unavailable in
      // task switcher (alt+tab menu).
      if native_window.has_window_style(WS_CHILD)
        || native_window
          .has_window_style_ex(WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW)
      {
        return Ok(None);
      }

      // Some applications spawn top-level windows for menus that
      // should be ignored. This includes the autocomplete popup in
      // Notepad++ and title bar menu in Keepass. Although not
      // foolproof, these can typically be identified by having an
      // owner window and no title bar.
      if native_window.has_owner_window()
        && !native_window.has_window_style(WS_CAPTION)
      {
        return Ok(None);
      }
    }
  }

  Ok(Some(native_properties))
}

fn create_window(
  native_window: NativeWindow,
  native_properties: NativeWindowProperties,
  target_parent: Option<Container>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<WindowContainer> {
  let nearest_monitor = state
    .nearest_monitor(&native_window)
    .context("No nearest monitor.")?;

  let nearest_workspace = nearest_monitor
    .displayed_workspace()
    .context("No nearest workspace.")?;

  let gaps_config = config.value.gaps.clone();
  let window_state =
    window_state_to_create(&native_properties, &nearest_monitor, config)?;

  // Attach the new window as the first child of the target parent (if
  // provided), otherwise, add as a sibling of the focused container.
  let (target_parent, target_index) = match target_parent {
    Some(parent) => (explicit_target_parent(parent, &window_state), 0),
    None => insertion_target(&window_state, state)?,
  };

  let target_workspace =
    target_parent.workspace().context("No target workspace.")?;

  let prefers_centered = config
    .value
    .window_behavior
    .state_defaults
    .floating
    .centered;

  // Calculate where window should be placed when floating is enabled. Use
  // the original width/height of the window and optionally position it in
  // the center of the workspace.
  let is_same_workspace = nearest_workspace.id() == target_workspace.id();
  let floating_placement = {
    let placement = if !is_same_workspace || prefers_centered {
      native_properties
        .frame
        .translate_to_center(&target_workspace.to_rect()?)
    } else {
      native_properties.frame.clone()
    };

    // Clamp the window size to be within the workspace's outer gaps. 10px
    // is arbitrary - helps differentiate from tiling windows.
    let max_workspace_rect = target_workspace.max_workspace_rect()?;
    placement.clamp_size(
      max_workspace_rect.width() - 10,
      max_workspace_rect.height() - 10,
    )
  };

  // Window has no border delta unless it's later changed via the
  // `adjust_borders` command.
  let border_delta = RectDelta::zero();

  let window_container: WindowContainer = match window_state {
    WindowState::Tiling => TilingWindow::new(
      None,
      native_window,
      native_properties,
      None,
      border_delta,
      floating_placement,
      false,
      gaps_config,
      Vec::new(),
      None,
    )
    .into(),
    _ => NonTilingWindow::new(
      None,
      native_window,
      native_properties,
      window_state,
      None,
      border_delta,
      None,
      floating_placement,
      !prefers_centered,
      Vec::new(),
      None,
    )
    .into(),
  };

  attach_container(
    &window_container.clone().into(),
    &target_parent,
    Some(target_index),
  )?;

  // The OS might spawn the window on a different monitor to the target
  // parent, so adjustments might need to be made because of DPI.
  if nearest_monitor
    .has_dpi_difference(&window_container.clone().into())?
  {
    window_container.set_has_pending_dpi_adjustment(true);
  }

  Ok(window_container)
}

/// Gets the initial state for a window based on its native state.
///
/// Note that maximized windows are initialized as tiling.
fn window_state_to_create(
  native_properties: &NativeWindowProperties,
  nearest_monitor: &Monitor,
  config: &UserConfig,
) -> anyhow::Result<WindowState> {
  if native_properties.is_minimized {
    return Ok(WindowState::Minimized);
  }

  let nearest_workspace = nearest_monitor
    .displayed_workspace()
    .context("No workspace.")?;

  // Only initialize as fullscreen if the window *exceeds* the workspace
  // bounds (due to the 1px inset).
  //
  // For example, with 0px outer gaps and a window that covers the entire
  // workspace, it would still not be initialized as fullscreen. The window
  // needs to be within the workspace's outer gaps by at least 1px on each
  // side.
  if !native_properties.is_maximized
    && native_properties
      .frame
      .inset(1)
      .contains_rect(&nearest_workspace.max_workspace_rect()?)
  {
    return Ok(WindowState::Fullscreen(
      config
        .value
        .window_behavior
        .state_defaults
        .fullscreen
        .clone(),
    ));
  }

  // Initialize windows that can't be resized as floating.
  if !native_properties.is_resizable {
    return Ok(WindowState::Floating(
      config.value.window_behavior.state_defaults.floating.clone(),
    ));
  }

  Ok(WindowState::default_from_config(&config.value))
}

/// Gets where to insert a new window in the container tree.
///
/// Rules:
/// - For non-tiling windows: Always append to the workspace.
/// - For tiling windows:
///   1. Try to insert after the focused tiling window if one exists.
///   2. Otherwise, if the workspace uses fixed-width areas and one was
///      last clicked/active (see `Workspace::last_active_area`), insert
///      into it. This is what makes clicking empty space within an area
///      "stick" as the target for the next opened window.
///   3. Otherwise, try to insert after the first tiling window found
///      anywhere on the workspace.
///   4. If no tiling windows exist and the workspace has fixed-width
///      areas configured, insert into the default area.
///   5. Otherwise, append to the workspace.
///
/// Returns tuple of (parent container, insertion index).
fn insertion_target(
  window_state: &WindowState,
  state: &WmState,
) -> anyhow::Result<(Container, usize)> {
  let focused_container =
    state.focused_container().context("No focused container.")?;

  let focused_workspace =
    focused_container.workspace().context("No workspace.")?;

  if *window_state == WindowState::Tiling {
    // Insert right after the focused window, if there is one. This takes
    // priority over the last active area, since it's the more precise
    // and more recent of the two signals.
    if let Container::TilingWindow(_) = &focused_container {
      return Ok((
        focused_container.parent().context("No parent.")?,
        focused_container.index() + 1,
      ));
    }

    if let Some(area) = focused_workspace.last_active_area() {
      return Ok((area.clone().into(), area.child_count()));
    }

    if let Some(sibling) = focused_workspace
      .descendant_focus_order()
      .find(Container::is_tiling_window)
    {
      return Ok((
        sibling.parent().context("No parent.")?,
        sibling.index() + 1,
      ));
    }

    // No tiling windows exist yet on the workspace (e.g. it's the first
    // window opened on it). If the workspace uses fixed-width areas,
    // place the window in the default area instead of directly on the
    // workspace, so it doesn't become a rogue extra area of its own.
    if let Some(default_area) = focused_workspace.default_area() {
      return Ok((
        default_area.clone().into(),
        default_area.child_count(),
      ));
    }
  }

  // Default to appending to workspace.
  Ok((
    focused_workspace.clone().into(),
    focused_workspace.child_count(),
  ))
}

/// Resolves an explicitly-given target parent for a new window (as
/// opposed to one derived via [`insertion_target`]).
///
/// If the target is a workspace that uses fixed-width areas and the
/// window will be tiling, redirects into the workspace's default area
/// instead of the workspace directly. Without this, windows that are
/// already open when the WM starts (see `WmState::populate`, which
/// passes the workspace itself as the target parent) would bypass areas
/// entirely and become rogue extra top-level containers.
fn explicit_target_parent(
  parent: Container,
  window_state: &WindowState,
) -> Container {
  if *window_state == WindowState::Tiling {
    if let Container::Workspace(workspace) = &parent {
      if let Some(default_area) = workspace.default_area() {
        return default_area.into();
      }
    }
  }

  parent
}
