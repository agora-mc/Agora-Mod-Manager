/**
 * The shared instance experience (MASTER_SPEC §29): the pieces every game's instance pages are
 * built from. Presentational only: no IPC and no game knowledge. A game adapts its own data into
 * these props, and shows only the actions its capabilities allow.
 */

export * from './types';
export { PageHero, PageHeroAction } from './PageHero';
export { InstanceCardFrame, InstanceCardBanner, HueChip, RunningIndicator } from './InstanceCardFrame';
export { InstanceActionBar, InstanceActionButton } from './InstanceActionBar';
export { InstanceGrid, InstanceCardSkeleton, INSTANCE_GRID_CLASS, type InstanceGridGroup } from './InstanceGrid';
export { EmptyState } from './EmptyState';
export { InstancePageHeader } from './InstancePageHeader';
export { ReadinessLine, READINESS_TEXT_CLASS } from './ReadinessLine';
export { InstanceTabBar, type InstanceTab } from './InstanceTabBar';
export { CreateInstanceFrame } from './CreateInstanceFrame';
export { FormField, FORM_CONTROL_CLASS, type FormFieldControlProps } from './FormField';
export { ChoiceCards, RecommendedBadge, recommendedLabel, type ChoiceOption } from './ChoiceCards';
