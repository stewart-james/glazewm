use anyhow::Context;
use tracing::info;
use wm_common::{
  AreaConfig, GapsConfig, TilingDirection, WmEvent, WorkspaceConfig,
};

use super::sort_workspaces;
use crate::{
  commands::container::attach_container,
  models::{Monitor, SplitContainer, Workspace},
  traits::{
    CommonGetters, PositionGetters, TilingDirectionGetters,
    TilingSizeGetters,
  },
  user_config::UserConfig,
  wm_state::WmState,
};

/// Activates a workspace on the target monitor.
///
/// If no workspace name is provided, the first suitable workspace defined
/// in the user's config will be used.
///
/// If no target monitor is provided, the workspace is activated on
/// whichever monitor it is bound to, or the currently focused monitor.
pub fn activate_workspace(
  workspace_name: Option<&str>,
  target_monitor: Option<Monitor>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let workspace_config = workspace_config(
    workspace_name,
    target_monitor.clone(),
    state,
    config,
  )?;

  let target_monitor = target_monitor
    .or_else(|| {
      workspace_config
        .bind_to_monitor
        .and_then(|index| {
          state
            .monitors()
            .into_iter()
            .find(|monitor| monitor.index() == index as usize)
        })
        .or_else(|| {
          state
            .focused_container()
            .and_then(|focused| focused.monitor())
        })
    })
    .context("Failed to get a target monitor for the workspace.")?;

  let workspace = Workspace::new(
    workspace_config.clone(),
    config.value.gaps.clone(),
    monitor_tiling_direction(&target_monitor)?,
  );

  // Attach the created workspace to the specified monitor.
  attach_container(
    &workspace.clone().into(),
    &target_monitor.clone().into(),
    None,
  )?;

  if state.areas_enabled {
    create_workspace_areas(
      &workspace,
      &workspace_config.areas,
      &config.value.gaps,
    )?;
  }

  sort_workspaces(&target_monitor, config)?;

  info!("Activating workspace: {workspace}");

  state.emit_event(WmEvent::WorkspaceActivated {
    activated_workspace: workspace.to_dto()?,
  });

  Ok(())
}

/// Gets config for the workspace to activate.
fn workspace_config(
  workspace_name: Option<&str>,
  target_monitor: Option<Monitor>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<WorkspaceConfig> {
  let found_config = match workspace_name {
    Some(workspace_name) => config
      .inactive_workspace_configs(&state.workspaces())
      .into_iter()
      .find(|config| config.name == workspace_name)
      .with_context(|| {
        format!(
          "Workspace with name '{workspace_name}' doesn't exist or is already active."
        )
      }),
    None => target_monitor
      .and_then(|target_monitor| {
        config.workspace_config_for_monitor(
          &target_monitor,
          &state.workspaces(),
        )
      })
      .or_else(|| {
        config.next_inactive_workspace_config(&state.workspaces())
      })
      .context("No workspace config available to activate workspace."),
  };

  found_config.cloned()
}

/// Gets the default tiling direction for a workspace on the monitor.
///
/// Returns `TilingDirection::Vertical` for portrait monitors, otherwise
/// `TilingDirection::Horizontal`.
pub fn monitor_tiling_direction(
  monitor: &Monitor,
) -> anyhow::Result<TilingDirection> {
  let monitor_rect = monitor.to_rect()?;

  Ok(if monitor_rect.height() > monitor_rect.width() {
    TilingDirection::Vertical
  } else {
    TilingDirection::Horizontal
  })
}

/// Creates the workspace's fixed-width areas (if configured) as
/// persistent top-level split containers, sized according to config.
///
/// Returns the created areas in order. Returns an empty vector if the
/// workspace's config has no `areas` defined.
pub fn create_workspace_areas(
  workspace: &Workspace,
  areas_config: &[AreaConfig],
  gaps_config: &GapsConfig,
) -> anyhow::Result<Vec<SplitContainer>> {
  if areas_config.is_empty() {
    return Ok(Vec::new());
  }

  let workspace_rect = workspace.to_rect()?;
  let total_length = match workspace.tiling_direction() {
    TilingDirection::Horizontal => workspace_rect.width(),
    TilingDirection::Vertical => workspace_rect.height(),
  };

  // Convert each area's configured width to a fraction of the
  // workspace's length, then normalize so the fractions always add up
  // to 1, regardless of what the config values add up to.
  let raw_fractions = areas_config
    .iter()
    .map(|area| area.width.to_percentage(total_length).max(0.))
    .collect::<Vec<_>>();

  let fraction_sum: f32 = raw_fractions.iter().sum();

  #[allow(clippy::cast_precision_loss)]
  let normalized_fractions = if fraction_sum > 0. {
    raw_fractions
      .iter()
      .map(|fraction| fraction / fraction_sum)
      .collect::<Vec<_>>()
  } else {
    vec![1. / areas_config.len() as f32; areas_config.len()]
  };

  // If no area is explicitly marked as default, fall back to the first.
  let has_explicit_default = areas_config.iter().any(|area| area.default);

  let areas = areas_config
    .iter()
    .enumerate()
    .map(|(index, area_config)| {
      let area = SplitContainer::new(
        area_config
          .tiling_direction
          .clone()
          .unwrap_or(workspace.tiling_direction()),
        gaps_config.clone(),
      );

      area.set_is_area(true);
      area.set_is_default_area(if has_explicit_default {
        area_config.default
      } else {
        index == 0
      });

      attach_container(
        &area.clone().into(),
        &workspace.clone().into(),
        None,
      )?;

      Ok(area)
    })
    .collect::<anyhow::Result<Vec<_>>>()?;

  // Set the final tiling sizes only after every area has been attached,
  // since `attach_container` redistributes space equally amongst
  // siblings as each new one is added.
  for (area, fraction) in areas.iter().zip(normalized_fractions) {
    area.set_tiling_size(fraction);
  }

  Ok(areas)
}
