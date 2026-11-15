use anyhow::Context;

use crate::{
  commands::container::move_container_within_tree,
  models::{Container, SplitContainer, WindowContainer, Workspace},
  traits::{CommonGetters, TilingSizeGetters},
  wm_state::WmState,
};

/// Moves a tiling window to the center of its workspace, at half of the
/// workspace's width.
///
/// If the workspace has areas, the window goes into the default area
/// and the other windows in that area move to the adjacent areas.
/// Otherwise, the window goes to the middle of the workspace with 50% of
/// the width, and the windows on each side share the remaining space.
/// Does nothing for non-tiling windows.
pub fn move_window_to_center(
  window: WindowContainer,
  state: &mut WmState,
) -> anyhow::Result<()> {
  let WindowContainer::TilingWindow(window) = window else {
    return Ok(());
  };

  let workspace = window.workspace().context("No workspace.")?;
  let window: Container = window.into();

  match workspace.default_area() {
    Some(area) => center_in_area(&window, &workspace, &area, state)?,
    None => center_in_workspace(&window, &workspace, state)?,
  }

  state.pending_sync.queue_container_to_redraw(workspace);

  Ok(())
}

/// Makes the window the only tiling child of the area. The other tiling
/// children of the area move to the adjacent area with fewer windows.
fn center_in_area(
  window: &Container,
  workspace: &Workspace,
  area: &SplitContainer,
  state: &WmState,
) -> anyhow::Result<()> {
  move_container_within_tree(window, &area.clone().into(), 0, state)?;

  let areas = workspace.areas();
  let area_index = areas
    .iter()
    .position(|other| other.id() == area.id())
    .context("Area is not in workspace.")?;

  let side_areas = [area_index.checked_sub(1), Some(area_index + 1)]
    .into_iter()
    .flatten()
    .filter_map(|index| areas.get(index).cloned())
    .collect::<Vec<_>>();

  let others = area
    .tiling_children()
    .map(Container::from)
    .filter(|child| child != window)
    .collect::<Vec<_>>();

  // With a single area, there is no place to move the other windows to.
  if side_areas.is_empty() {
    return Ok(());
  }

  for other in others {
    // `min_by_key` returns the first minimum, so the left area wins ties.
    let target = side_areas
      .iter()
      .min_by_key(|side_area| {
        side_area
          .descendants()
          .filter(Container::is_tiling_window)
          .count()
      })
      .context("No side area.")?;

    move_container_within_tree(
      &other,
      &target.clone().into(),
      target.child_count(),
      state,
    )?;
  }

  window.as_tiling_container()?.set_tiling_size(1.0);

  Ok(())
}

/// Moves the window to the middle of the workspace's top-level tiling
/// containers and sizes it to half of the workspace.
fn center_in_workspace(
  window: &Container,
  workspace: &Workspace,
  state: &WmState,
) -> anyhow::Result<()> {
  let parent: Container = workspace.clone().into();
  let target_index = center_index(&parent, window);
  move_container_within_tree(window, &parent, target_index, state)?;

  let children = workspace.tiling_children().collect::<Vec<_>>();
  let window_index = children
    .iter()
    .position(|child| child.id() == window.id())
    .context("Window is not in workspace.")?;

  for (child, size) in children
    .iter()
    .zip(balanced_sizes(children.len(), window_index))
  {
    child.set_tiling_size(size);
  }

  Ok(())
}

/// Gets the child index in `parent` that puts `container` in the middle
/// of the parent's tiling children.
///
/// The index is for the list of children without `container`, which is
/// what `move_container_within_tree` expects. With an even number of
/// tiling children, the left (or top) of the two middle positions is
/// used.
fn center_index(parent: &Container, container: &Container) -> usize {
  let others = parent
    .children()
    .into_iter()
    .filter(|child| child != container)
    .collect::<Vec<_>>();

  let tiling_positions = others
    .iter()
    .enumerate()
    .filter(|(_, child)| child.is_tiling_window() || child.is_split())
    .map(|(index, _)| index)
    .collect::<Vec<_>>();

  tiling_positions
    .get(tiling_positions.len() / 2)
    .copied()
    .unwrap_or(others.len())
}

/// Gets tiling sizes that give the container at `center` half of the
/// space, and split the other half equally between the two sides.
///
/// If one side is empty, the other side gets the full other half. If
/// there is only one container, it gets all of the space.
#[allow(clippy::cast_precision_loss)]
fn balanced_sizes(count: usize, center: usize) -> Vec<f32> {
  let left = center;
  let right = count.saturating_sub(center + 1);

  let (left_share, right_share) = match (left, right) {
    (0, 0) => (0., 0.),
    (0, _) => (0., 0.5),
    (_, 0) => (0.5, 0.),
    _ => (0.25, 0.25),
  };

  (0..count)
    .map(|index| {
      if index < center {
        left_share / left as f32
      } else if index > center {
        right_share / right as f32
      } else if count == 1 {
        1.
      } else {
        0.5
      }
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::{balanced_sizes, center_index};
  use crate::models::{Container, TilingWindow, Workspace};

  fn workspace_with_windows(count: usize) -> (Workspace, Vec<Container>) {
    let windows = (0..count)
      .map(|_| TilingWindow::mock().call())
      .collect::<Vec<_>>();

    let workspace = Workspace::mock()
      .tiling_containers(windows.iter().cloned().map(Into::into).collect())
      .call();

    (workspace, windows.into_iter().map(Into::into).collect())
  }

  #[test]
  fn centers_window_in_same_parent() {
    let (workspace, windows) = workspace_with_windows(3);
    let workspace = workspace.into();

    assert_eq!(center_index(&workspace, &windows[0]), 1);
    assert_eq!(center_index(&workspace, &windows[2]), 1);
  }

  #[test]
  fn centers_window_from_other_parent() {
    let (workspace, _) = workspace_with_windows(2);
    let window = TilingWindow::mock().call().into();

    assert_eq!(center_index(&workspace.into(), &window), 1);
  }

  #[test]
  fn empty_parent_uses_first_index() {
    let (workspace, _) = workspace_with_windows(0);
    let window = TilingWindow::mock().call().into();

    assert_eq!(center_index(&workspace.into(), &window), 0);
  }

  #[test]
  fn balances_sizes_around_center() {
    assert_eq!(balanced_sizes(1, 0), vec![1.]);
    assert_eq!(balanced_sizes(2, 0), vec![0.5, 0.5]);
    assert_eq!(balanced_sizes(3, 1), vec![0.25, 0.5, 0.25]);
    assert_eq!(balanced_sizes(4, 2), vec![0.125, 0.125, 0.5, 0.25]);
  }
}
