use wm_platform::Direction;

use crate::{
  commands::container::move_container_within_tree,
  models::{Container, SplitContainer, TilingWindow, WindowContainer},
  traits::CommonGetters,
  wm_state::WmState,
};

/// Moves a tiling window into the adjacent area in the given direction,
/// bypassing the area boundary that `move_window_in_direction` otherwise
/// enforces.
///
/// Does nothing if the window isn't within an area, or if there's no
/// adjacent area in the given direction (e.g. moving right from the
/// rightmost area).
pub fn move_window_to_area(
  window: WindowContainer,
  direction: &Direction,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let WindowContainer::TilingWindow(window) = window else {
    return Ok(());
  };

  let Some(current_area) = enclosing_area(&window) else {
    return Ok(());
  };

  let Some(target_area) = adjacent_area(&current_area, direction) else {
    return Ok(());
  };

  // Arrive at the edge of the target area closest to where the window
  // came from, mirroring the convention used when moving a window across
  // monitors/workspaces.
  let target_index = match direction {
    Direction::Down | Direction::Right => 0,
    Direction::Up | Direction::Left => target_area.child_count(),
  };

  move_container_within_tree(
    &window.clone().into(),
    &target_area.clone().into(),
    target_index,
    state,
  )?;

  state
    .pending_sync
    .queue_container_to_redraw(current_area)
    .queue_container_to_redraw(target_area);

  Ok(())
}

/// Gets the area that the window is nested within, if any.
fn enclosing_area(window: &TilingWindow) -> Option<SplitContainer> {
  window.self_and_ancestors().find_map(as_area)
}

/// Gets the sibling area in the given direction, if one exists.
fn adjacent_area(
  area: &SplitContainer,
  direction: &Direction,
) -> Option<SplitContainer> {
  match direction {
    Direction::Up | Direction::Left => {
      area.prev_siblings().find_map(as_area)
    }
    Direction::Down | Direction::Right => {
      area.next_siblings().find_map(as_area)
    }
  }
}

/// Converts a container to a `SplitContainer` if it's an area.
fn as_area(container: Container) -> Option<SplitContainer> {
  container.as_split().filter(|split| split.is_area()).cloned()
}
