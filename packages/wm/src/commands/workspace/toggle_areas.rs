use std::collections::HashMap;

use anyhow::Context;
use uuid::Uuid;
use wm_common::GapsConfig;

use super::{create_workspace_areas, monitor_tiling_direction};
use crate::{
  commands::container::{
    attach_container, detach_container, flatten_split_container,
    set_focused_descendant,
  },
  models::{Container, SplitContainer, Workspace},
  traits::{CommonGetters, TilingDirectionGetters, TilingSizeGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

/// Placement of containers in their areas, remembered while areas are
/// turned off.
#[derive(Debug, Default)]
pub struct RememberedAreas {
  /// Area index of each window, by window ID.
  area_indices: HashMap<Uuid, usize>,

  /// Tiling size of each direct child of an area within the area, by
  /// container ID.
  tiling_sizes: HashMap<Uuid, f32>,
}

/// Turns fixed-width workspace areas on or off for all workspaces.
///
/// Turning areas off removes the area containers, but keeps the windows
/// and their sizes. The area of each window is remembered, so that
/// turning areas on again puts the window back into the same area.
/// Windows without a remembered area go into the default area.
pub fn toggle_areas(
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  state.areas_enabled = !state.areas_enabled;

  for workspace in state.workspaces() {
    if state.areas_enabled {
      enable_areas(
        &workspace,
        &state.remembered_areas,
        &config.value.gaps,
      )?;
    } else {
      disable_areas(&workspace, &mut state.remembered_areas)?;
    }
  }

  if state.areas_enabled {
    state.remembered_areas = RememberedAreas::default();
  }

  state
    .pending_sync
    .queue_container_to_redraw(state.root_container.clone());

  Ok(())
}

/// Removes the areas of the workspace and remembers the area index of
/// each window and the size of each container in the areas.
fn disable_areas(
  workspace: &Workspace,
  remembered: &mut RememberedAreas,
) -> anyhow::Result<()> {
  workspace.set_last_active_area_id(None);

  for (index, area) in workspace.areas().into_iter().enumerate() {
    for window in area
      .descendants()
      .filter(|descendant| descendant.as_window_container().is_ok())
    {
      remembered.area_indices.insert(window.id(), index);
    }

    for child in area.tiling_children() {
      remembered
        .tiling_sizes
        .insert(child.id(), child.tiling_size());
    }

    // The area becomes an ordinary split container. Remove it if it is
    // redundant, and keep it if it holds a layout in a different tiling
    // direction (e.g. a vertical stack of windows).
    area.set_is_area(false);
    area.set_is_default_area(false);

    if area.child_count() == 0 {
      detach_container(area.into())?;
    } else if area.child_count() == 1
      || area.tiling_direction() == workspace.tiling_direction()
    {
      flatten_split_container(area)?;
    }
  }

  Ok(())
}

/// Creates the configured areas of the workspace and moves its tiling
/// containers into them.
///
/// Does nothing if the workspace has no configured areas, or already
/// has areas.
fn enable_areas(
  workspace: &Workspace,
  remembered: &RememberedAreas,
  gaps_config: &GapsConfig,
) -> anyhow::Result<()> {
  let areas_config = workspace.config().areas;

  if areas_config.is_empty() || !workspace.areas().is_empty() {
    return Ok(());
  }

  let focused_window = workspace
    .descendant_focus_order()
    .find(Container::is_tiling_window);

  // Each tiling child goes into the remembered area of its first window.
  let children = workspace
    .tiling_children()
    .map(|child| {
      let area_index =
        child.self_and_descendants().find_map(|descendant| {
          remembered.area_indices.get(&descendant.id()).copied()
        });

      (Container::from(child.clone()), area_index)
    })
    .collect::<Vec<_>>();

  // Detach the children first, so that the areas get their configured
  // sizes without existing siblings.
  for (child, ..) in &children {
    detach_container(child.clone())?;
  }

  // The tiling direction of the workspace can change while areas are
  // off. Reset it, because the areas are laid out along it.
  workspace.set_tiling_direction(monitor_tiling_direction(
    &workspace.monitor().context("No monitor.")?,
  )?);

  let areas =
    create_workspace_areas(workspace, &areas_config, gaps_config)?;
  let default_index = areas
    .iter()
    .position(SplitContainer::is_default_area)
    .unwrap_or(0);

  let mut area_children = vec![Vec::new(); areas.len()];

  for (child, area_index) in children {
    let index = area_index
      .unwrap_or(default_index)
      .min(areas.len().saturating_sub(1));

    let area = areas.get(index).context("No area.")?;
    attach_container(&child, &area.clone().into(), None)?;
    area_children[index].push(child);
  }

  // Restore the remembered sizes. Containers without a remembered size
  // (e.g. windows opened while areas were off) get an equal share.
  for children in area_children {
    #[allow(clippy::cast_precision_loss)]
    let equal_size = 1. / children.len().max(1) as f32;

    let sizes = children
      .iter()
      .map(|child| {
        remembered
          .tiling_sizes
          .get(&child.id())
          .copied()
          .unwrap_or(equal_size)
      })
      .collect::<Vec<_>>();

    let size_sum: f32 = sizes.iter().sum();

    if size_sum > 0. {
      for (child, size) in children.iter().zip(sizes) {
        child
          .as_tiling_container()?
          .set_tiling_size(size / size_sum);
      }
    }
  }

  // Keep the last focused window as the focused descendant of the
  // workspace.
  if let Some(focused_window) = focused_window {
    if let Some(area) = focused_window.ancestors().find(|ancestor| {
      ancestor.as_split().is_some_and(SplitContainer::is_area)
    }) {
      set_focused_descendant(&focused_window, Some(&area));
    }
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use wm_common::{AreaConfig, GapsConfig, TilingDirection};

  use super::{disable_areas, enable_areas, RememberedAreas};
  use crate::{
    models::{Monitor, SplitContainer, TilingWindow, Workspace},
    traits::{CommonGetters, TilingSizeGetters},
  };

  fn area_config(percent: f32, default: bool) -> AreaConfig {
    AreaConfig {
      width: format!("{percent}%").parse().unwrap(),
      default,
      tiling_direction: None,
    }
  }

  /// Creates a workspace on a mock monitor with left/center/right areas.
  fn workspace_with_areas() -> Workspace {
    let workspace = Workspace::mock().call();
    let mut config = workspace.config();
    config.areas = vec![
      area_config(25., false),
      area_config(50., true),
      area_config(25., false),
    ];
    workspace.set_config(config);
    let _monitor =
      Monitor::mock().workspaces(vec![workspace.clone()]).call();
    workspace
  }

  fn area_window_ids(workspace: &Workspace) -> Vec<Vec<uuid::Uuid>> {
    workspace
      .areas()
      .iter()
      .map(|area| area.children().iter().map(CommonGetters::id).collect())
      .collect()
  }

  #[test]
  fn toggle_off_and_on_restores_areas() {
    let workspace = workspace_with_areas();
    let mut remembered = RememberedAreas::default();
    let gaps = GapsConfig::default();

    enable_areas(&workspace, &remembered, &gaps).unwrap();
    let areas = workspace.areas();
    assert_eq!(areas.len(), 3);

    let left = TilingWindow::mock().call();
    let right_a = TilingWindow::mock().call();
    let right_b = TilingWindow::mock().call();
    let attach = |window: &TilingWindow, area: &SplitContainer| {
      crate::commands::container::attach_container(
        &window.clone().into(),
        &area.clone().into(),
        None,
      )
      .unwrap();
    };
    attach(&left, &areas[0]);
    attach(&right_a, &areas[2]);
    attach(&right_b, &areas[2]);
    right_a.set_tiling_size(0.75);
    right_b.set_tiling_size(0.25);
    let before = area_window_ids(&workspace);

    disable_areas(&workspace, &mut remembered).unwrap();
    assert!(workspace.areas().is_empty());
    assert_eq!(workspace.tiling_children().count(), 3);

    // A window opened while areas are off goes into the default area.
    let new_window = TilingWindow::mock().call();
    crate::commands::container::attach_container(
      &new_window.clone().into(),
      &workspace.clone().into(),
      None,
    )
    .unwrap();

    enable_areas(&workspace, &remembered, &gaps).unwrap();
    let after = area_window_ids(&workspace);
    assert_eq!(after[0], before[0]);
    assert_eq!(after[1], vec![new_window.id()]);
    assert_eq!(after[2], before[2]);
    assert!((right_a.tiling_size() - 0.75).abs() < 0.001);
    assert!((right_b.tiling_size() - 0.25).abs() < 0.001);

    let sizes = workspace
      .areas()
      .iter()
      .map(TilingSizeGetters::tiling_size)
      .collect::<Vec<_>>();
    assert!((sizes[1] - 0.5).abs() < 0.001);
  }

  #[test]
  fn toggle_off_keeps_stack_in_different_direction() {
    let workspace = workspace_with_areas();
    let mut config = workspace.config();
    config.areas[0].tiling_direction = Some(TilingDirection::Vertical);
    workspace.set_config(config);

    enable_areas(
      &workspace,
      &RememberedAreas::default(),
      &GapsConfig::default(),
    )
    .unwrap();
    let left = workspace.areas()[0].clone();
    for _ in 0..2 {
      crate::commands::container::attach_container(
        &TilingWindow::mock().call().into(),
        &left.clone().into(),
        None,
      )
      .unwrap();
    }

    disable_areas(&workspace, &mut RememberedAreas::default()).unwrap();
    let children = workspace.tiling_children().collect::<Vec<_>>();
    assert_eq!(children.len(), 1);
    assert!(children[0].as_split().is_some_and(|s| !s.is_area()));
  }
}
