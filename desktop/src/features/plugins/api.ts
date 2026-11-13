/**
 * IPC wrappers for the plugin commands.
 *
 * Thin on purpose: every one of these is a single `invoke`, because the
 * decisions live in `agora-core` and duplicating any of them here would mean
 * the GUI and the CLI could disagree about what a plugin is allowed to do.
 */

import { invoke } from '@tauri-apps/api/core';
import type {
  DiagnosticReport,
  InstallPreview,
  LaunchCheckOutcome,
  NamespacedContribution,
  OfficialPluginList,
  PluginStartFailure,
  PluginSummary,
  RepairOutcome,
  RepairProposal,
  SurfaceChoice,
  UpdateOutcome,
  UpdateVerdict,
  ViewModel,
} from './types';

/**
 * Whether the emergency stop ("Stop all plugin code") is engaged. While it is,
 * no plugin script runs, official plugins included. Not engaged by default.
 */
export const pluginsEmergencyStop = () => invoke<boolean>('plugins_emergency_stop');

/**
 * Whether plugin code may run at all. Only an explicit "not stopped" counts: a
 * rejection, or an answer that is not a boolean, reads as stopped.
 */
export const pluginCodeAllowed = async () => (await pluginsEmergencyStop()) === false;

export const setPluginsEmergencyStop = (engaged: boolean) =>
  invoke<void>('set_plugins_emergency_stop', { engaged });

/** Allow or disallow one plugin's code. Independent per plugin; disallowing stops it now. */
export const setPluginScriptsAllowed = (pluginId: string, allowed: boolean) =>
  invoke<PluginSummary>('set_plugin_scripts_allowed', { pluginId, allowed });

export const startPlugins = () => invoke<PluginStartFailure[]>('start_plugins');

export const listPlugins = () => invoke<PluginSummary[]>('list_plugins');

export const listPluginContributions = () =>
  invoke<NamespacedContribution[]>('list_plugin_contributions');

export const previewPluginPackage = (path: string) =>
  invoke<InstallPreview>('preview_plugin_package', { path });

export const previewPluginFolder = (path: string) =>
  invoke<InstallPreview>('preview_plugin_folder', { path });

/**
 * `allowScripts` is the separate "<name> runs code. Allow it?" answer. Left out,
 * the permission answer stands for both.
 */
export const installPluginPackage = (
  path: string,
  acceptCapabilities: boolean,
  allowScripts?: boolean,
) => invoke<PluginSummary>('install_plugin_package', { path, acceptCapabilities, allowScripts });

export const checkPluginUpdate = (pluginId: string) =>
  invoke<UpdateVerdict | null>('plugin_check_update', { pluginId });

export const applyPluginUpdate = (
  pluginId: string,
  acceptCapabilities: boolean,
  allowScripts?: boolean,
) =>
  invoke<UpdateOutcome | null>('plugin_apply_update', {
    pluginId,
    acceptCapabilities,
    allowScripts,
  });

/** The official plugins the signed catalog offers; empty with a reason when it cannot be verified. */
export const listOfficialPlugins = () => invoke<OfficialPluginList>('list_official_plugins');

export const installOfficialPlugin = (
  pluginId: string,
  acceptCapabilities: boolean,
  allowScripts?: boolean,
) =>
  invoke<UpdateOutcome>('install_official_plugin', {
    pluginId,
    acceptCapabilities,
    allowScripts,
  });

export const updateOfficialPlugin = (pluginId: string, acceptCapabilities: boolean) =>
  invoke<UpdateOutcome>('update_official_plugin', { pluginId, acceptCapabilities });

export const rollbackOfficialPlugin = (pluginId: string, acceptCapabilities: boolean) =>
  invoke<UpdateOutcome>('rollback_official_plugin', { pluginId, acceptCapabilities });

export const addPluginDevelopmentFolder = (
  path: string,
  acceptCapabilities: boolean,
  allowScripts?: boolean,
) =>
  invoke<PluginSummary>('add_plugin_development_folder', {
    path,
    acceptCapabilities,
    allowScripts,
  });

export const setPluginEnabled = (pluginId: string, enabled: boolean) =>
  invoke<void>('set_plugin_enabled', { pluginId, enabled });

export const uninstallPlugin = (pluginId: string, purgeData: boolean) =>
  invoke<void>('uninstall_plugin', { pluginId, purgeData });

/** The recovery path: stop and disable everything, without plugin cooperation. */
export const disableAllPlugins = () => invoke<number>('disable_all_plugins');

export const renderPluginView = (pluginId: string, exportName: string, args?: unknown) =>
  invoke<ViewModel>('render_plugin_view', { pluginId, export: exportName, args: args ?? null });

export const runPluginCommand = (pluginId: string, exportName: string, args?: unknown) =>
  invoke<unknown>('run_plugin_command', { pluginId, export: exportName, args: args ?? null });

export const runPluginDiagnostic = (pluginId: string, exportName: string, instanceId: string) =>
  invoke<DiagnosticReport>('run_plugin_diagnostic', { pluginId, export: exportName, instanceId });

export const applyPluginRepair = (pluginId: string, proposal: RepairProposal) =>
  invoke<RepairOutcome>('apply_plugin_repair', { pluginId, proposal });

/** Starts any plugin whose only activation event is `onInstanceOpened`. */
export const pluginInstanceOpened = () => invoke<number>('plugin_instance_opened');

export const runPluginLaunchChecks = (instanceId: string) =>
  invoke<LaunchCheckOutcome>('run_plugin_launch_checks', { instanceId });

export const setPluginSetting = (pluginId: string, key: string, value: unknown) =>
  invoke<void>('set_plugin_setting', { pluginId, key, value });

export const readPluginLog = (pluginId: string, lines?: number) =>
  invoke<string[]>('read_plugin_log', { pluginId, lines: lines ?? null });

export const pluginSurfaces = () => invoke<SurfaceChoice[]>('plugin_surfaces');

/** `contributionId: null` restores Agora's own view for that surface. */
export const setPluginSurface = (surface: string, contributionId: string | null) =>
  invoke<void>('plugin_set_surface', { surface, contributionId });

/** Returns how many stored entries were put back. */
export const restorePluginData = (pluginId: string) =>
  invoke<number>('restore_plugin_data', { pluginId });
