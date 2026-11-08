//! Re-exports of `agora_core::registry` functions/types for
//! backward-compatible `crate::registry::*` resolution in callers.

pub use agora_core::registry::{
    browse_items, get_all_mod_aliases, get_curated_annotation, get_item_by_id, get_known_conflicts,
    get_manifest_dependencies, list_categories, list_mod_reviews, pack_mods_for_pack,
    resolve_alias, row_to_item, CategoryInfo, CuratedAnnotation, KnownConflict, ManifestDeps,
    ModReview, PackModRow, RegistryItem, SortOption, REGISTRY_ITEM_COLUMNS,
};
