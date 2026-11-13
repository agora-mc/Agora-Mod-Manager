/**
 * The crash answer screen's IPC: the stored answer for one crashed session, an instance's earlier
 * answers, and a log the user pastes or picks. Every reply is checked for the shape the backend
 * documents, so a missing answer is an error here, never "no crash".
 *
 * The core answer types travel with snake_case fields (`headline`, `could_not_check`, ...); this
 * module does not rename them, so the page reads exactly what the engine wrote.
 */

import { invoke } from '@tauri-apps/api/core';

export type CrashLevel = 'not_sure' | 'possible' | 'likely' | 'very_likely';
export type CrashCause =
  | 'mod'
  | 'version_mismatch'
  | 'missing_requirement'
  | 'file'
  | 'setting'
  | 'save'
  | 'memory'
  | 'driver'
  | 'game_install'
  | 'other';
export type SaveRisk = 'safe' | 'risky' | 'unknown';
export type NextStepKind =
  | 'apply_repair'
  | 'confirm_experiment'
  | 'bisect'
  | 'recapture'
  | 'restore_save'
  | 'nothing_needed'
  | 'second_opinion';
export type CrashVerdict =
  | { kind: 'crashed'; how?: string }
  | { kind: 'failed_to_start' }
  | { kind: 'froze' }
  | { kind: 'stopped' }
  | { kind: 'normal' }
  | { kind: 'closed_unknown' };

export type CrashOwner =
  | { kind: 'item'; key: string; name: string }
  | { kind: 'base' }
  | { kind: 'generated'; tool: string }
  | { kind: 'writable' }
  | { kind: 'framework'; name: string }
  | { kind: 'system' }
  | { kind: 'not_in_instance' }
  | { kind: 'unknown' };

/** A localized sentence; `en` is the one the desktop shows. */
export type LocalText = Record<string, string>;

export interface CrashSource {
  plugin_id: string;
  plugin_name: string;
  plugin_version: string;
  rule_id?: string;
  rule_version?: number;
  parser_id?: string;
}

export interface CrashEvidenceLine {
  artifact: string;
  span: Record<string, unknown>;
  quote?: string;
}

export interface CrashCandidate {
  owner: CrashOwner;
  level: CrashLevel;
  score: number;
  cause: CrashCause;
  families: string[];
  sources: CrashSource[];
  also_provided_by?: string[];
  save_risk?: SaveRisk;
}

export interface CrashFinding {
  scope: 'root_cause' | 'mechanism' | 'environment';
  level: CrashLevel;
  cause: CrashCause;
  candidate?: CrashOwner;
  headline: LocalText;
  why_not_sure?: LocalText;
  source: CrashSource;
  evidence?: CrashEvidenceLine[];
}

export interface CrashCouldNotCheck {
  reason: string;
  subject: string;
  detail?: string;
  source?: CrashSource;
}

export interface CrashTrustNote {
  text: LocalText;
  source: CrashSource;
}

export interface CrashNextStep {
  kind: NextStepKind;
  keys: string[];
}

export interface CrashArtifact {
  id: string;
  name: string;
  size: number;
  modified_at?: string;
  source?: string;
  bound_by?: string;
  privacy: string;
  retained: { kind: string; why?: string; head?: number; tail?: number };
  attributed_to?: CrashOwner;
  children?: CrashArtifact[];
}

export interface CrashAnswer {
  contract: number;
  verdict: CrashVerdict;
  headline: LocalText;
  level: CrashLevel;
  cause?: CrashCause;
  candidates: CrashCandidate[];
  findings: CrashFinding[];
  inspections?: unknown[];
  could_not_check: CrashCouldNotCheck[];
  trust_notes: CrashTrustNote[];
  next: CrashNextStep;
  repairs: unknown[];
  artifacts: CrashArtifact[];
}

export type AnswerStatus = 'analyzed' | 'nothing_to_read' | 'could_not_analyze';

export interface CrashAnswerSummary {
  sessionId: string;
  instanceId: string;
  verdict: CrashVerdict;
  endedAt: string | null;
  createdAt: string;
  status: AnswerStatus;
  headline?: string;
  level?: CrashLevel;
  cause?: CrashCause;
  reason?: string;
}

export interface CrashAnswerList {
  answers: CrashAnswerSummary[];
  latestVerdict: CrashVerdict | null;
  retainedCrashes: number;
}

export interface CrashChange {
  kind: string;
  key?: string;
  from_version?: string;
  to_version?: string;
}

export interface CrashAnswerDetail {
  sessionId: string;
  instanceId: string;
  verdict: CrashVerdict;
  endedAt: string | null;
  createdAt: string;
  status: AnswerStatus;
  answer?: CrashAnswer;
  files?: unknown[];
  changes?: CrashChange[];
  reason?: string;
}

export interface SuppliedCrashAnswer {
  status: 'analyzed' | 'nothing_to_read';
  answer?: CrashAnswer;
  files?: unknown[];
  reason?: string;
}

const LEVELS: readonly string[] = ['not_sure', 'possible', 'likely', 'very_likely'];
const CAUSES: readonly string[] = [
  'mod',
  'version_mismatch',
  'missing_requirement',
  'file',
  'setting',
  'save',
  'memory',
  'driver',
  'game_install',
  'other',
];
const VERDICTS: readonly string[] = [
  'crashed',
  'failed_to_start',
  'froze',
  'stopped',
  'normal',
  'closed_unknown',
];
const NEXT_STEPS: readonly string[] = [
  'apply_repair',
  'confirm_experiment',
  'bisect',
  'recapture',
  'restore_save',
  'nothing_needed',
  'second_opinion',
];
const STATUSES: readonly string[] = ['analyzed', 'nothing_to_read', 'could_not_analyze'];
const SCOPES: readonly string[] = ['root_cause', 'mechanism', 'environment'];
const SAVE_RISKS: readonly string[] = ['safe', 'risky', 'unknown'];

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function isOptionalString(value: unknown): boolean {
  return value === null || value === undefined || typeof value === 'string';
}

function isText(value: unknown): boolean {
  return isRecord(value) && typeof value.en === 'string';
}

function isLevel(value: unknown): value is CrashLevel {
  return typeof value === 'string' && LEVELS.includes(value);
}

function isCause(value: unknown): value is CrashCause {
  return typeof value === 'string' && CAUSES.includes(value);
}

function isVerdict(value: unknown): value is CrashVerdict {
  return isRecord(value) && typeof value.kind === 'string' && VERDICTS.includes(value.kind);
}

function isOwner(value: unknown): value is CrashOwner {
  return isRecord(value) && typeof value.kind === 'string';
}

function isSource(value: unknown): boolean {
  return isRecord(value)
    && typeof value.plugin_id === 'string'
    && typeof value.plugin_name === 'string'
    && typeof value.plugin_version === 'string';
}

function isCandidate(value: unknown): value is CrashCandidate {
  return isRecord(value)
    && isOwner(value.owner)
    && isLevel(value.level)
    && typeof value.score === 'number'
    && isCause(value.cause)
    && Array.isArray(value.families)
    && Array.isArray(value.sources)
    && value.sources.every(isSource)
    && (value.save_risk === undefined || (typeof value.save_risk === 'string' && SAVE_RISKS.includes(value.save_risk)));
}

function isFinding(value: unknown): value is CrashFinding {
  return isRecord(value)
    && typeof value.scope === 'string'
    && SCOPES.includes(value.scope)
    && isLevel(value.level)
    && isCause(value.cause)
    && (value.candidate === undefined || isOwner(value.candidate))
    && isText(value.headline)
    && isSource(value.source);
}

function isCouldNotCheck(value: unknown): value is CrashCouldNotCheck {
  return isRecord(value)
    && typeof value.reason === 'string'
    && typeof value.subject === 'string'
    && (value.source === undefined || isSource(value.source));
}

function isTrustNote(value: unknown): value is CrashTrustNote {
  return isRecord(value) && isText(value.text) && isSource(value.source);
}

function isArtifact(value: unknown): value is CrashArtifact {
  return isRecord(value)
    && typeof value.id === 'string'
    && typeof value.name === 'string'
    && typeof value.size === 'number'
    && isRecord(value.retained)
    && typeof value.retained.kind === 'string';
}

function isAnswer(value: unknown): value is CrashAnswer {
  return isRecord(value)
    && typeof value.contract === 'number'
    && isVerdict(value.verdict)
    && isText(value.headline)
    && isLevel(value.level)
    && (value.cause === undefined || isCause(value.cause))
    && Array.isArray(value.candidates)
    && value.candidates.every(isCandidate)
    && Array.isArray(value.findings)
    && value.findings.every(isFinding)
    && Array.isArray(value.could_not_check)
    && value.could_not_check.every(isCouldNotCheck)
    && Array.isArray(value.trust_notes)
    && value.trust_notes.every(isTrustNote)
    && isRecord(value.next)
    && typeof value.next.kind === 'string'
    && NEXT_STEPS.includes(value.next.kind)
    && Array.isArray(value.next.keys)
    && Array.isArray(value.artifacts)
    && value.artifacts.every(isArtifact);
}

function isSummary(value: unknown): value is CrashAnswerSummary {
  return isRecord(value)
    && typeof value.sessionId === 'string'
    && typeof value.instanceId === 'string'
    && isVerdict(value.verdict)
    && isOptionalString(value.endedAt)
    && typeof value.createdAt === 'string'
    && typeof value.status === 'string'
    && STATUSES.includes(value.status)
    && (value.headline === undefined || typeof value.headline === 'string')
    && (value.level === undefined || isLevel(value.level))
    && (value.cause === undefined || isCause(value.cause))
    && (value.reason === undefined || typeof value.reason === 'string');
}

function shapeError(command: string): Error {
  return new Error(`${command} did not return what the crash screen expects.`);
}

export async function listCrashAnswers(instanceId: string): Promise<CrashAnswerList> {
  const reply: unknown = await invoke<unknown>('list_crash_answers', { instanceId });
  if (
    !isRecord(reply)
    || !Array.isArray(reply.answers)
    || !reply.answers.every(isSummary)
    || (reply.latestVerdict !== null && !isVerdict(reply.latestVerdict))
    || typeof reply.retainedCrashes !== 'number'
  ) {
    throw shapeError('list_crash_answers');
  }
  return reply as unknown as CrashAnswerList;
}

export async function getCrashAnswer(sessionId: string): Promise<CrashAnswerDetail> {
  const reply: unknown = await invoke<unknown>('get_crash_answer', { sessionId });
  if (
    !isRecord(reply)
    || typeof reply.sessionId !== 'string'
    || typeof reply.instanceId !== 'string'
    || !isVerdict(reply.verdict)
    || !isOptionalString(reply.endedAt)
    || typeof reply.createdAt !== 'string'
    || typeof reply.status !== 'string'
    || !STATUSES.includes(reply.status)
    || (reply.answer !== undefined && !isAnswer(reply.answer))
    || (reply.reason !== undefined && typeof reply.reason !== 'string')
    || (reply.changes !== undefined && !Array.isArray(reply.changes))
  ) {
    throw shapeError('get_crash_answer');
  }
  return reply as unknown as CrashAnswerDetail;
}

export async function analyzeCrashText(instanceId: string | null, text: string): Promise<SuppliedCrashAnswer> {
  const reply: unknown = await invoke<unknown>('analyze_crash_text', { instanceId, text });
  if (
    !isRecord(reply)
    || (reply.status !== 'analyzed' && reply.status !== 'nothing_to_read')
    || (reply.answer !== undefined && !isAnswer(reply.answer))
    || (reply.reason !== undefined && typeof reply.reason !== 'string')
  ) {
    throw shapeError('analyze_crash_text');
  }
  return reply as unknown as SuppliedCrashAnswer;
}

export async function analyzeCrashFile(instanceId: string | null, path: string): Promise<SuppliedCrashAnswer> {
  const reply: unknown = await invoke<unknown>('analyze_crash_file', { instanceId, path });
  if (
    !isRecord(reply)
    || (reply.status !== 'analyzed' && reply.status !== 'nothing_to_read')
    || (reply.answer !== undefined && !isAnswer(reply.answer))
    || (reply.reason !== undefined && typeof reply.reason !== 'string')
  ) {
    throw shapeError('analyze_crash_file');
  }
  return reply as unknown as SuppliedCrashAnswer;
}

export async function revealCrashArtifact(sessionId: string, artifactId: string): Promise<void> {
  await invoke<void>('reveal_crash_artifact', { sessionId, artifactId });
}

/** The text of one kept file, for copying it to another analyzer. */
export async function readCrashArtifact(sessionId: string, artifactId: string): Promise<string> {
  return invoke<string>('read_crash_artifact', { sessionId, artifactId });
}

/**
 * The Phostwood analyzer's page. It analyzes client-side, so Agora copies the log and tells the
 * user to paste it; nothing is uploaded by Agora. From
 * `https://github.com/Phostwood/crash-analyzer` (`skyrim.html`).
 */
export const PHOSTWOOD_ANALYZER_URL = 'https://phostwood.github.io/crash-analyzer/skyrim.html';
