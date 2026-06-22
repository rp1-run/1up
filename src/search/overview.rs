//! Repository orientation digest engine backing the `oneup_overview` MCP tool.
//!
//! Computes a deterministic, size-bounded digest (statistics, most-referenced
//! types, module map, cross-module dependencies, entry points) from bounded
//! SQL aggregates over the existing index tables. Pure read path: no schema
//! changes, no embedding runtime, no persisted artifacts (REQ-014 is
//! structural; this module never touches `segment_vectors`).
#![allow(dead_code)] // The MCP surface task wires `oneup_overview` through this engine.

use std::collections::BTreeMap;

use libsql::Connection;

use crate::search::impact::is_low_signal_path;
use crate::shared::errors::OneupError;
use crate::shared::symbols::{
    EDGE_IDENTITY_BARE_IDENTIFIER, EDGE_IDENTITY_CONSTRUCTOR_LIKE, EDGE_IDENTITY_MACRO_LIKE,
    EDGE_IDENTITY_QUALIFIED_PATH,
};
use crate::storage::relations;
use crate::storage::segments::{self, QualifyingTypeDefinition};

/// Maximum languages reported in the statistics section.
pub const LANGUAGE_CAP: usize = 10;

/// Maximum entries in the most-referenced types section.
pub const TOP_SYMBOL_CAP: usize = 10;

/// Maximum modules reported in the module map.
pub const MODULE_CAP: usize = 12;

/// Maximum directed cross-module dependency edges reported.
pub const MODULE_DEPENDENCY_CAP: usize = 15;

/// Maximum entry points reported.
pub const ENTRY_POINT_CAP: usize = 8;

/// Ranked symbol keys fetched before Rust-side path exclusion and the
/// ambiguity skip reduce them to `TOP_SYMBOL_CAP`.
pub const SYMBOL_OVERSAMPLE: usize = 120;

/// Entry-point candidates fetched before Rust-side path exclusion reduces
/// them to `ENTRY_POINT_CAP`.
pub const ENTRY_POINT_OVERSAMPLE: usize = 64;

/// Raw depth-2 dependency pairs fetched before the rollup merges them to the
/// module-map granularity and caps them at `MODULE_DEPENDENCY_CAP`.
pub const MODULE_DEPENDENCY_PAIR_OVERSAMPLE: usize = 256;

/// Upper bound on qualifying definition rows resolved for the oversampled
/// symbol keys, so the lookup stays bounded without loading full tables.
/// Keys whose fetched rows fall short of their SQL-reported definition count
/// are treated as truncated and skipped as ambiguous, never misattributed.
pub const DEFINITION_RESOLUTION_LIMIT: usize = 1024;

/// A symbol key qualifies for the top-types section only while its
/// post-exclusion qualifying definition count stays within this limit;
/// wider duplication is skipped as ambiguous rather than misattributed.
pub const AMBIGUITY_DEFINITION_LIMIT: usize = 3;

/// Dominant-module expansion threshold (design value 0.60), expressed in
/// percent so the share comparison stays exact integer arithmetic.
pub const DOMINANT_MODULE_SHARE_PERCENT: u64 = 60;

/// Type-definition block kinds; kind-rank-first attribution sorts these
/// before any non-type definition kind.
pub const TYPE_DEFINITION_KINDS: [&str; 5] = ["struct", "enum", "trait", "class", "interface"];

/// Shipped qualifying-definition kind policy: Branch B, types only (HYP-001
/// v3 verdict, design D19 documented REQ-003 downscope). Must stay aligned
/// with `queries::OVERVIEW_QUALIFYING_TYPE_KINDS_SQL`.
pub const QUALIFYING_DEFINITION_KINDS: [&str; 5] = TYPE_DEFINITION_KINDS;

/// Roles a segment must carry for its symbol rows to qualify as definitions.
/// Must stay aligned with `queries::OVERVIEW_QUALIFYING_ROLES_SQL`.
pub const QUALIFYING_DEFINITION_ROLES: [&str; 3] =
    ["DEFINITION", "IMPLEMENTATION", "ORCHESTRATION"];

/// Relation edge kinds carrying usable target identity for aggregate ranking
/// (design D13: receiver/member edges resolve identity only through per-pair
/// owner alignment, which a bounded aggregate cannot compute). Must stay
/// aligned with `queries::OVERVIEW_IDENTITY_BEARING_EDGE_KINDS_SQL`.
pub const IDENTITY_BEARING_EDGE_KINDS: [&str; 4] = [
    EDGE_IDENTITY_BARE_IDENTIFIER,
    EDGE_IDENTITY_QUALIFIED_PATH,
    EDGE_IDENTITY_CONSTRUCTOR_LIKE,
    EDGE_IDENTITY_MACRO_LIKE,
];

/// Full orientation digest for one worktree context. Sections use structs
/// and `Vec`s only, so serialization order is fixed and repeated computes on
/// an unchanged index are identical (REQ-008).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepositoryOverview {
    pub stats: OverviewStats,
    pub top_symbols: Vec<TopSymbolEntry>,
    pub modules: Vec<ModuleEntry>,
    pub module_dependencies: Vec<ModuleDependencyEntry>,
    pub entry_points: Vec<EntryPointEntry>,
}

/// Repository shape and scale for the active context.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverviewStats {
    pub indexed_files: u64,
    pub total_segments: u64,
    pub languages: Vec<LanguageBreakdown>,
}

/// Per-language file and segment counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageBreakdown {
    pub language: String,
    pub files: u64,
    pub segments: u64,
}

/// One most-referenced type: the attributed definition plus the breadth of
/// incoming references measured as distinct referencing files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopSymbolEntry {
    pub name: String,
    pub handle: String,
    pub path: String,
    pub line_start: i64,
    pub line_end: i64,
    pub referencing_files: u64,
    /// Post-exclusion qualifying definition count, always within
    /// `1..=AMBIGUITY_DEFINITION_LIMIT`.
    pub definition_count: u64,
}

/// One module-map entry at the chosen granularity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleEntry {
    pub module: String,
    pub segments: u64,
}

/// One directed cross-module dependency edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleDependencyEntry {
    pub source: String,
    pub target: String,
    pub count: u64,
}

/// One likely entry point derived from the existing role classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryPointEntry {
    pub handle: String,
    pub path: String,
    pub line_start: i64,
    pub line_end: i64,
    pub role: String,
    pub symbol: Option<String>,
    pub breadcrumb: Option<String>,
}

/// Read-only engine assembling the orientation digest from the bounded
/// storage aggregates added for `oneup_overview`.
pub struct OverviewEngine<'a> {
    conn: &'a Connection,
}

impl<'a> OverviewEngine<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Compute the orientation digest for one worktree context. Every
    /// section is deterministically ordered with documented tie-breaks; a
    /// context with no segments yields zeroed statistics and empty sections.
    pub async fn compute(&self, context_id: &str) -> Result<RepositoryOverview, OneupError> {
        let stats = self.compute_stats(context_id).await?;
        let top_symbols = self.compute_top_symbols(context_id).await?;
        let (modules, expanded_module) = self
            .compute_modules(context_id, stats.total_segments)
            .await?;
        let module_dependencies = self
            .compute_module_dependencies(context_id, expanded_module.as_deref())
            .await?;
        let entry_points = self.compute_entry_points(context_id).await?;

        Ok(RepositoryOverview {
            stats,
            top_symbols,
            modules,
            module_dependencies,
            entry_points,
        })
    }

    async fn compute_stats(&self, context_id: &str) -> Result<OverviewStats, OneupError> {
        let indexed_files = segments::count_files_for_context(self.conn, context_id).await?;
        let total_segments = segments::count_segments_for_context(self.conn, context_id).await?;
        let languages =
            segments::get_language_stats_for_context(self.conn, context_id, LANGUAGE_CAP)
                .await?
                .into_iter()
                .map(|stat| LanguageBreakdown {
                    language: stat.language,
                    files: stat.files,
                    segments: stat.segments,
                })
                .collect();

        Ok(OverviewStats {
            indexed_files,
            total_segments,
            languages,
        })
    }

    /// Most-referenced types: SQL ranks oversampled keys by distinct
    /// referencing files (DESC, then key ASC); the engine resolves their
    /// qualifying definitions, skips any key whose definitions may have been
    /// cut by `DEFINITION_RESOLUTION_LIMIT`, applies path exclusions BEFORE
    /// the ambiguity count, skips keys outside the
    /// `1..=AMBIGUITY_DEFINITION_LIMIT` post-exclusion range, and attributes
    /// each survivor kind-rank-first.
    async fn compute_top_symbols(
        &self,
        context_id: &str,
    ) -> Result<Vec<TopSymbolEntry>, OneupError> {
        let ranked = relations::get_top_type_symbol_references_for_context(
            self.conn,
            context_id,
            SYMBOL_OVERSAMPLE,
        )
        .await?;
        if ranked.is_empty() {
            return Ok(Vec::new());
        }

        let keys: Vec<String> = ranked
            .iter()
            .map(|entry| entry.symbol_key.clone())
            .collect();
        let definitions = segments::get_qualifying_type_definitions_for_context(
            self.conn,
            context_id,
            &keys,
            DEFINITION_RESOLUTION_LIMIT,
        )
        .await?;

        // Per-key fetched rows are counted BEFORE path exclusion so they are
        // comparable with the SQL-reported pre-exclusion definition counts;
        // `is_low_signal_path` subsumes `is_test_path`, covering both
        // documented exclusion classes in one check.
        let mut fetched_counts_by_key: BTreeMap<String, u64> = BTreeMap::new();
        let mut definitions_by_key: BTreeMap<String, Vec<QualifyingTypeDefinition>> =
            BTreeMap::new();
        for definition in definitions {
            *fetched_counts_by_key
                .entry(definition.symbol_key.clone())
                .or_insert(0) += 1;
            if is_low_signal_path(&definition.file_path) {
                continue;
            }
            definitions_by_key
                .entry(definition.symbol_key.clone())
                .or_default()
                .push(definition);
        }

        let mut entries = Vec::with_capacity(TOP_SYMBOL_CAP);
        for ranked_key in &ranked {
            if entries.len() == TOP_SYMBOL_CAP {
                break;
            }
            // The global resolution LIMIT can truncate alphabetically-late
            // keys. A key whose fetched rows fall short of its SQL-reported
            // definition count may be missing definitions, so its ambiguity
            // gate cannot be trusted: skip it as ambiguous rather than
            // misattribute a partial fetch.
            let fetched = fetched_counts_by_key
                .get(&ranked_key.symbol_key)
                .copied()
                .unwrap_or(0);
            if fetched < ranked_key.definition_count {
                continue;
            }
            let Some(candidates) = definitions_by_key.get(&ranked_key.symbol_key) else {
                continue;
            };
            if !(1..=AMBIGUITY_DEFINITION_LIMIT).contains(&candidates.len()) {
                continue;
            }
            let Some(attributed) = candidates
                .iter()
                .min_by_key(|definition| definition_attribution_key(definition))
            else {
                continue;
            };
            entries.push(TopSymbolEntry {
                name: attributed.symbol.clone(),
                handle: attributed.segment_id.clone(),
                path: attributed.file_path.clone(),
                line_start: attributed.line_start,
                line_end: attributed.line_end,
                referencing_files: ranked_key.referencing_files,
                definition_count: candidates.len() as u64,
            });
        }

        Ok(entries)
    }

    /// Module map at depth-1 granularity, expanding the dominant module one
    /// level exactly once when it holds at least
    /// `DOMINANT_MODULE_SHARE_PERCENT` of all segments and has true depth-2
    /// children. Returns the expanded module key (if any) so the dependency
    /// rollup shares the same granularity mapping.
    async fn compute_modules(
        &self,
        context_id: &str,
        total_segments: u64,
    ) -> Result<(Vec<ModuleEntry>, Option<String>), OneupError> {
        let mut rows =
            segments::get_module_segment_counts_for_context(self.conn, context_id, MODULE_CAP)
                .await?;

        let dominant_module = rows
            .first()
            .filter(|largest| {
                total_segments > 0
                    && largest.segments.saturating_mul(100)
                        >= total_segments.saturating_mul(DOMINANT_MODULE_SHARE_PERCENT)
            })
            .map(|largest| largest.module.clone());

        let mut expanded_module = None;
        if let Some(parent) = dominant_module {
            let children = segments::get_module_child_segment_counts_for_context(
                self.conn, context_id, &parent, MODULE_CAP,
            )
            .await?;
            // Files directly inside the dominant module keep its depth-1
            // key, so expansion only applies when a true child exists.
            if children.iter().any(|child| child.module != parent) {
                rows.retain(|row| row.module != parent);
                rows.extend(children);
                rows.sort_by(|a, b| {
                    b.segments
                        .cmp(&a.segments)
                        .then_with(|| a.module.cmp(&b.module))
                });
                rows.truncate(MODULE_CAP);
                expanded_module = Some(parent);
            }
        }

        let entries = rows
            .into_iter()
            .map(|row| ModuleEntry {
                module: row.module,
                segments: row.segments,
            })
            .collect();
        Ok((entries, expanded_module))
    }

    /// Directed cross-module dependency edges: raw depth-2 pairs are mapped
    /// through the module-map granularity, self-edges are dropped, and any
    /// edge whose TARGET module is test- or low-signal-classified is dropped
    /// (the SQL stage cannot path-exclude definition files). Referencing-side
    /// test modules are retained: `tests -> src` is genuine dependency
    /// information.
    async fn compute_module_dependencies(
        &self,
        context_id: &str,
        expanded_module: Option<&str>,
    ) -> Result<Vec<ModuleDependencyEntry>, OneupError> {
        let pairs = relations::get_module_dependency_pairs_for_context(
            self.conn,
            context_id,
            MODULE_DEPENDENCY_PAIR_OVERSAMPLE,
        )
        .await?;

        let mut rolled: BTreeMap<(String, String), u64> = BTreeMap::new();
        for pair in pairs {
            let source = module_key_at_granularity(&pair.source_module, expanded_module);
            let target = module_key_at_granularity(&pair.target_module, expanded_module);
            if source == target {
                continue;
            }
            // The target module key is normalized as a directory path so
            // nested keys like `tests/support` classify correctly;
            // `is_low_signal_path` subsumes `is_test_path`.
            if is_low_signal_path(&format!("{target}/")) {
                continue;
            }
            *rolled.entry((source, target)).or_insert(0) += pair.pair_count;
        }

        let mut entries: Vec<ModuleDependencyEntry> = rolled
            .into_iter()
            .map(|((source, target), count)| ModuleDependencyEntry {
                source,
                target,
                count,
            })
            .collect();
        entries.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| a.source.cmp(&b.source))
                .then_with(|| a.target.cmp(&b.target))
        });
        entries.truncate(MODULE_DEPENDENCY_CAP);
        Ok(entries)
    }

    /// Entry points: the SQL oversample is already ordered by path depth
    /// ASC, role rank ASC (orchestration first), path ASC, line ASC; the
    /// engine only excludes test/low-signal paths and caps the result.
    async fn compute_entry_points(
        &self,
        context_id: &str,
    ) -> Result<Vec<EntryPointEntry>, OneupError> {
        let candidates = segments::get_entry_point_candidates_for_context(
            self.conn,
            context_id,
            ENTRY_POINT_OVERSAMPLE,
        )
        .await?;

        Ok(candidates
            .into_iter()
            .filter(|candidate| !is_low_signal_path(&candidate.file_path))
            .take(ENTRY_POINT_CAP)
            .map(|candidate| {
                let symbol = serde_json::from_str::<Vec<String>>(&candidate.defined_symbols)
                    .unwrap_or_default()
                    .into_iter()
                    .next();
                EntryPointEntry {
                    handle: candidate.segment_id,
                    path: candidate.file_path,
                    line_start: candidate.line_start,
                    line_end: candidate.line_end,
                    role: candidate.role,
                    symbol,
                    breadcrumb: candidate.breadcrumb,
                }
            })
            .collect())
    }
}

/// Kind-rank-first attribution order: type-kind definitions before non-type,
/// then path ASC, then line start ASC, then segment id ASC. Under the
/// shipped Branch B policy every qualifying definition is a type kind, so
/// this reduces to the documented path/line tie-break.
fn definition_attribution_key(definition: &QualifyingTypeDefinition) -> (u8, &str, i64, &str) {
    let kind_rank = if TYPE_DEFINITION_KINDS.contains(&definition.block_type.as_str()) {
        0
    } else {
        1
    };
    (
        kind_rank,
        definition.file_path.as_str(),
        definition.line_start,
        definition.segment_id.as_str(),
    )
}

/// Map a raw depth-2 module key onto the granularity chosen for the module
/// map: keys inside the expanded dominant module stay depth-2, everything
/// else collapses to its first path component. Module and edge granularity
/// always agree because both sections share this mapping.
fn module_key_at_granularity(depth2_key: &str, expanded_module: Option<&str>) -> String {
    if let Some(parent) = expanded_module {
        let inside_expanded = depth2_key == parent
            || depth2_key
                .strip_prefix(parent)
                .is_some_and(|rest| rest.starts_with('/'));
        if inside_expanded {
            return depth2_key.to_string();
        }
    }
    depth2_key
        .split('/')
        .next()
        .unwrap_or(depth2_key)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::symbols::EDGE_IDENTITY_METHOD_RECEIVER;
    use crate::shared::types::ParsedRelation;
    use crate::storage::{
        db::Db,
        queries, schema,
        segments::{upsert_segment_for_context, SegmentInsert},
    };

    async fn setup() -> (Db, Connection) {
        let db = Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
        (db, conn)
    }

    fn segment(id: &str, file_path: &str) -> SegmentInsert {
        SegmentInsert {
            id: id.to_string(),
            file_path: file_path.to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: format!("segment {id}"),
            line_start: 1,
            line_end: 3,
            content_key: None,
            embedding_vec: None,
            breadcrumb: None,
            complexity: 1,
            role: "IMPLEMENTATION".to_string(),
            defined_symbols: "[]".to_string(),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: format!("hash-{id}"),
        }
    }

    fn definition(id: &str, file_path: &str, block_type: &str, symbol: &str) -> SegmentInsert {
        let mut seg = segment(id, file_path);
        seg.block_type = block_type.to_string();
        seg.role = "DEFINITION".to_string();
        seg.defined_symbols = serde_json::to_string(&[symbol]).unwrap();
        seg
    }

    fn referencing(id: &str, file_path: &str, refs: Vec<ParsedRelation>) -> SegmentInsert {
        let mut seg = segment(id, file_path);
        seg.referenced_relations = serde_json::to_string(&refs).unwrap();
        seg
    }

    fn relation(symbol: &str, edge_identity_kind: &str) -> ParsedRelation {
        ParsedRelation {
            symbol: symbol.to_string(),
            edge_identity_kind: edge_identity_kind.to_string(),
            kind: None,
        }
    }

    fn bare(symbol: &str) -> ParsedRelation {
        relation(symbol, EDGE_IDENTITY_BARE_IDENTIFIER)
    }

    async fn insert_all(conn: &Connection, context_id: &str, segments: Vec<SegmentInsert>) {
        for seg in segments {
            upsert_segment_for_context(conn, context_id, &seg)
                .await
                .unwrap();
        }
    }

    async fn compute(conn: &Connection, context_id: &str) -> RepositoryOverview {
        OverviewEngine::new(conn).compute(context_id).await.unwrap()
    }

    /// Fixture with one genuine cross-module edge, one test-target edge, one
    /// low-signal-target edge, and one same-module edge. `src` holds 4 of 7
    /// segments (57%), so the module map stays at depth-1 granularity.
    fn dependency_fixture() -> Vec<SegmentInsert> {
        vec![
            definition("def_helper", "tests/support/helper.rs", "struct", "Helper"),
            definition("def_bench_util", "benches/util.rs", "struct", "BenchUtil"),
            definition(
                "def_registry",
                "src/daemon/registry.rs",
                "struct",
                "Registry",
            ),
            definition("def_local", "src/cli/types.rs", "struct", "Local"),
            referencing(
                "ref_cli",
                "src/cli/a.rs",
                vec![bare("Helper"), bare("BenchUtil")],
            ),
            referencing("ref_cmd", "src/cli/cmd.rs", vec![bare("Local")]),
            referencing("ref_tests", "tests/integration.rs", vec![bare("Registry")]),
        ]
    }

    #[tokio::test]
    async fn overview_excludes_test_vendor_and_chunk_definitions() {
        let (_db, conn) = setup().await;
        insert_all(
            &conn,
            "ctx-a",
            vec![
                definition("def_widget", "src/widgets.rs", "struct", "Widget"),
                definition("def_fixture", "tests/helpers.rs", "struct", "Fixture"),
                definition("def_vendored", "vendor/lib.rs", "struct", "Vendored"),
                definition("def_blob", "src/blob.rs", "chunk", "Blob"),
                referencing(
                    "ref_a",
                    "src/a.rs",
                    vec![
                        bare("Widget"),
                        bare("Fixture"),
                        bare("Vendored"),
                        bare("Blob"),
                    ],
                ),
                referencing(
                    "ref_b",
                    "src/b.rs",
                    vec![
                        bare("Widget"),
                        bare("Fixture"),
                        bare("Vendored"),
                        bare("Blob"),
                    ],
                ),
            ],
        )
        .await;

        let overview = compute(&conn, "ctx-a").await;

        assert_eq!(
            overview
                .top_symbols
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Widget"]
        );
        let widget = &overview.top_symbols[0];
        assert_eq!(widget.handle, "def_widget");
        assert_eq!(widget.path, "src/widgets.rs");
        assert_eq!(widget.referencing_files, 2);
        assert_eq!(widget.definition_count, 1);
    }

    #[tokio::test]
    async fn overview_skips_ambiguous_keys_post_exclusion() {
        let (_db, conn) = setup().await;
        insert_all(
            &conn,
            "ctx-a",
            vec![
                definition("def_dup_a", "src/a/dup.rs", "struct", "Dup"),
                definition("def_dup_b", "src/b/dup.rs", "struct", "Dup"),
                definition("def_dup_c", "src/c/dup.rs", "struct", "Dup"),
                definition("def_dup_d", "src/d/dup.rs", "struct", "Dup"),
                definition("def_half_a", "src/half_a.rs", "struct", "Half"),
                definition("def_half_b", "src/half_b.rs", "struct", "Half"),
                definition("def_half_t1", "tests/half_t1.rs", "struct", "Half"),
                definition("def_half_t2", "tests/half_t2.rs", "struct", "Half"),
                referencing("ref_a", "src/r1.rs", vec![bare("Dup"), bare("Half")]),
                referencing("ref_b", "src/r2.rs", vec![bare("Dup"), bare("Half")]),
            ],
        )
        .await;

        let overview = compute(&conn, "ctx-a").await;

        assert_eq!(
            overview
                .top_symbols
                .iter()
                .map(|entry| (entry.name.as_str(), entry.definition_count))
                .collect::<Vec<_>>(),
            vec![("Half", 2)]
        );
        assert_eq!(overview.top_symbols[0].handle, "def_half_a");
    }

    #[tokio::test]
    async fn overview_distinct_file_metric_prefers_breadth() {
        let (_db, conn) = setup().await;
        insert_all(
            &conn,
            "ctx-a",
            vec![
                definition("def_broad", "src/broad.rs", "struct", "Broad"),
                definition("def_deep", "src/deep.rs", "struct", "Deep"),
                referencing(
                    "ref_volume_1",
                    "src/volume.rs",
                    vec![bare("Deep"), bare("Deep"), bare("Deep")],
                ),
                referencing("ref_volume_2", "src/volume.rs", vec![bare("Deep")]),
                referencing("ref_volume_3", "src/volume.rs", vec![bare("Deep")]),
                referencing("ref_a", "src/a.rs", vec![bare("Broad")]),
                referencing("ref_b", "src/b.rs", vec![bare("Broad")]),
                referencing("ref_c", "src/c.rs", vec![bare("Broad")]),
            ],
        )
        .await;

        let overview = compute(&conn, "ctx-a").await;

        assert_eq!(
            overview
                .top_symbols
                .iter()
                .map(|entry| (entry.name.as_str(), entry.referencing_files))
                .collect::<Vec<_>>(),
            vec![("Broad", 3), ("Deep", 1)]
        );
    }

    #[tokio::test]
    async fn overview_attributes_mixed_key_to_type_definition() {
        let (_db, conn) = setup().await;
        // `BranchStatus` (enum) and `branch_status` (accessor fn) normalize
        // to the same key; only the type definition qualifies under Branch B,
        // so attribution must land on the enum even though the accessor path
        // sorts first.
        insert_all(
            &conn,
            "ctx-a",
            vec![
                definition("def_enum", "src/shared/types.rs", "enum", "BranchStatus"),
                definition(
                    "def_accessor",
                    "src/daemon/registry.rs",
                    "function",
                    "branch_status",
                ),
                referencing("ref_a", "src/cli/a.rs", vec![bare("BranchStatus")]),
                referencing("ref_b", "src/mcp/b.rs", vec![bare("branch_status")]),
            ],
        )
        .await;

        let overview = compute(&conn, "ctx-a").await;

        assert_eq!(overview.top_symbols.len(), 1);
        let top = &overview.top_symbols[0];
        assert_eq!(top.name, "BranchStatus");
        assert_eq!(top.handle, "def_enum");
        assert_eq!(top.path, "src/shared/types.rs");
        assert_eq!(top.referencing_files, 2);
        assert_eq!(top.definition_count, 1);
    }

    #[tokio::test]
    async fn overview_excludes_receiver_only_and_non_type_definition_noise() {
        let (_db, conn) = setup().await;
        insert_all(
            &conn,
            "ctx-a",
            vec![
                definition("def_conn_struct", "src/db/conn.rs", "struct", "Conn"),
                definition("def_conn_accessor", "src/db/util.rs", "function", "conn"),
                definition("def_tmp_var", "scripts/vars.sh", "variable", "tmp"),
                definition("def_err_alias", "src/shared/types.rs", "type", "Err"),
                definition("def_anchor", "src/anchor.rs", "struct", "Anchor"),
                referencing(
                    "ref_a",
                    "src/a.rs",
                    vec![
                        relation("Conn", EDGE_IDENTITY_METHOD_RECEIVER),
                        bare("tmp"),
                        bare("Err"),
                        bare("Anchor"),
                    ],
                ),
                referencing(
                    "ref_b",
                    "src/b.rs",
                    vec![
                        relation("Conn", EDGE_IDENTITY_METHOD_RECEIVER),
                        bare("tmp"),
                        bare("Err"),
                    ],
                ),
                referencing(
                    "ref_c",
                    "src/c.rs",
                    vec![relation("Conn", EDGE_IDENTITY_METHOD_RECEIVER)],
                ),
            ],
        )
        .await;

        let overview = compute(&conn, "ctx-a").await;

        assert_eq!(
            overview
                .top_symbols
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Anchor"]
        );
    }

    #[tokio::test]
    async fn overview_drops_test_and_low_signal_target_module_edges() {
        let (_db, conn) = setup().await;
        insert_all(&conn, "ctx-a", dependency_fixture()).await;

        let overview = compute(&conn, "ctx-a").await;

        // Largest module is below the dominance threshold: depth-1 map.
        assert_eq!(
            overview
                .modules
                .iter()
                .map(|entry| (entry.module.as_str(), entry.segments))
                .collect::<Vec<_>>(),
            vec![("src", 4), ("tests", 2), ("benches", 1)]
        );
        // The `-> tests` and `-> benches` target edges and the src self-edge
        // are dropped; the tests-side SOURCE edge survives.
        assert_eq!(
            overview.module_dependencies,
            vec![ModuleDependencyEntry {
                source: "tests".to_string(),
                target: "src".to_string(),
                count: 1,
            }]
        );
    }

    #[tokio::test]
    async fn overview_dominant_module_expands_once() {
        let (_db, conn) = setup().await;
        insert_all(
            &conn,
            "ctx-a",
            vec![
                segment("seg_cli_a", "src/cli/a.rs"),
                segment("seg_cli_b", "src/cli/b.rs"),
                segment("seg_cli_deep", "src/cli/sub/deep.rs"),
                segment("seg_storage_a", "src/storage/a.rs"),
                segment("seg_storage_b", "src/storage/b.rs"),
                segment("seg_main", "src/main.rs"),
                segment("seg_lib", "src/lib.rs"),
                segment("seg_scripts", "scripts/run.sh"),
                segment("seg_docs", "docs/guide.md"),
                segment("seg_root", "README.md"),
            ],
        )
        .await;

        let overview = compute(&conn, "ctx-a").await;

        assert_eq!(overview.stats.indexed_files, 10);
        assert_eq!(overview.stats.total_segments, 10);
        assert_eq!(
            overview.stats.languages,
            vec![LanguageBreakdown {
                language: "rust".to_string(),
                files: 10,
                segments: 10,
            }]
        );
        // `src` holds 7 of 10 segments (>= 60%): expanded exactly one level.
        // Deeper paths stay at depth-2 (`src/cli`, never `src/cli/sub`) and
        // files directly inside the expanded module keep its key.
        assert_eq!(
            overview
                .modules
                .iter()
                .map(|entry| (entry.module.as_str(), entry.segments))
                .collect::<Vec<_>>(),
            vec![
                ("src/cli", 3),
                ("src", 2),
                ("src/storage", 2),
                ("(root)", 1),
                ("docs", 1),
                ("scripts", 1),
            ]
        );
    }

    #[tokio::test]
    async fn overview_edge_rollup_uses_module_map_granularity() {
        let (_db, conn) = setup().await;
        insert_all(
            &conn,
            "ctx-a",
            vec![
                definition("def_store", "src/storage/db.rs", "struct", "Store"),
                referencing("ref_cli_a", "src/cli/a.rs", vec![bare("Store")]),
                referencing("ref_cli_b", "src/cli/b.rs", vec![bare("Store")]),
                segment("seg_cli_c", "src/cli/c.rs"),
                segment("seg_storage_b", "src/storage/b.rs"),
                segment("seg_direct", "src/main.rs"),
                referencing("ref_scripts", "scripts/gen.rs", vec![bare("Store")]),
            ],
        )
        .await;

        let overview = compute(&conn, "ctx-a").await;

        // `src` holds 6 of 7 segments: the module map expands it, so edges
        // between its children stay depth-2 while outside modules collapse
        // to depth-1 through the same mapping.
        assert_eq!(
            overview
                .modules
                .iter()
                .map(|entry| (entry.module.as_str(), entry.segments))
                .collect::<Vec<_>>(),
            vec![
                ("src/cli", 3),
                ("src/storage", 2),
                ("scripts", 1),
                ("src", 1),
            ]
        );
        assert_eq!(
            overview.module_dependencies,
            vec![
                ModuleDependencyEntry {
                    source: "src/cli".to_string(),
                    target: "src/storage".to_string(),
                    count: 2,
                },
                ModuleDependencyEntry {
                    source: "scripts".to_string(),
                    target: "src/storage".to_string(),
                    count: 1,
                },
            ]
        );
    }

    #[tokio::test]
    async fn overview_entry_points_prefer_shallow_paths_and_exclude_test_paths() {
        let (_db, conn) = setup().await;
        let mut orch_main = segment("seg_main", "main.rs");
        orch_main.role = "ORCHESTRATION".to_string();
        orch_main.defined_symbols = r#"["main"]"#.to_string();
        orch_main.breadcrumb = Some("main".to_string());
        let mut def_lib = segment("seg_lib", "src/lib.rs");
        def_lib.role = "DEFINITION".to_string();
        let mut orch_run = segment("seg_run", "src/run.rs");
        orch_run.role = "ORCHESTRATION".to_string();
        let mut orch_deep = segment("seg_deep", "src/cli/mod.rs");
        orch_deep.role = "ORCHESTRATION".to_string();
        let mut orch_test = segment("seg_test", "tests/runner.rs");
        orch_test.role = "ORCHESTRATION".to_string();
        let mut orch_bench = segment("seg_bench", "benches/bench.rs");
        orch_bench.role = "ORCHESTRATION".to_string();
        let mut chunk_root = segment("seg_chunk", "data.txt");
        chunk_root.role = "DEFINITION".to_string();
        chunk_root.block_type = "chunk".to_string();
        let impl_only = segment("seg_impl", "src/x.rs");
        insert_all(
            &conn,
            "ctx-a",
            vec![
                orch_main, def_lib, orch_run, orch_deep, orch_test, orch_bench, chunk_root,
                impl_only,
            ],
        )
        .await;

        let overview = compute(&conn, "ctx-a").await;

        assert_eq!(
            overview
                .entry_points
                .iter()
                .map(|entry| (entry.path.as_str(), entry.role.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("main.rs", "ORCHESTRATION"),
                ("src/run.rs", "ORCHESTRATION"),
                ("src/lib.rs", "DEFINITION"),
                ("src/cli/mod.rs", "ORCHESTRATION"),
            ]
        );
        let first = &overview.entry_points[0];
        assert_eq!(first.handle, "seg_main");
        assert_eq!(first.symbol.as_deref(), Some("main"));
        assert_eq!(first.breadcrumb.as_deref(), Some("main"));
        assert_eq!(overview.entry_points[2].symbol, None);
    }

    #[tokio::test]
    async fn overview_scopes_to_active_context() {
        let (_db, conn) = setup().await;
        insert_all(
            &conn,
            "ctx-a",
            vec![
                definition("a_def", "src/alpha.rs", "struct", "Alpha"),
                referencing("a_ref", "src/use_a.rs", vec![bare("Alpha")]),
            ],
        )
        .await;
        insert_all(
            &conn,
            "ctx-b",
            vec![
                definition("b_def", "lib/beta.rs", "struct", "Beta"),
                referencing("b_ref", "lib/use_b.rs", vec![bare("Beta")]),
                segment("b_extra", "lib/extra.rs"),
            ],
        )
        .await;

        let overview = compute(&conn, "ctx-a").await;

        assert_eq!(overview.stats.indexed_files, 2);
        assert_eq!(overview.stats.total_segments, 2);
        assert_eq!(
            overview
                .top_symbols
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Alpha"]
        );
        assert_eq!(
            overview
                .modules
                .iter()
                .map(|entry| entry.module.as_str())
                .collect::<Vec<_>>(),
            vec!["src"]
        );
        assert!(overview
            .entry_points
            .iter()
            .all(|entry| !entry.path.starts_with("lib/")));
    }

    #[tokio::test]
    async fn overview_skips_keys_truncated_by_the_definition_resolution_limit() {
        let (_db, conn) = setup().await;
        // Enough qualifying definitions to overflow DEFINITION_RESOLUTION_LIMIT:
        // the filler key consumes nearly the whole fetch (ordered by symbol key
        // ASC), so the alphabetically-late key with four real definitions has
        // all but one row cut. Without truncation detection that key passes
        // the ambiguity gate on its partial fetch and is misattributed.
        let mut segments = vec![definition(
            "def_anchor",
            "src/anchor.rs",
            "struct",
            "AaaAnchor",
        )];
        for idx in 0..(DEFINITION_RESOLUTION_LIMIT - 2) {
            segments.push(definition(
                &format!("def_filler_{idx:04}"),
                &format!("src/filler/f{idx:04}.rs"),
                "struct",
                "AabFiller",
            ));
        }
        for (idx, path) in [
            "src/late/a.rs",
            "src/late/b.rs",
            "src/late/c.rs",
            "src/late/d.rs",
        ]
        .into_iter()
        .enumerate()
        {
            segments.push(definition(
                &format!("def_late_{idx}"),
                path,
                "struct",
                "ZzzLate",
            ));
        }
        segments.extend([
            referencing(
                "ref_anchor_1",
                "src/r1.rs",
                vec![bare("AaaAnchor"), bare("ZzzLate")],
            ),
            referencing(
                "ref_anchor_2",
                "src/r2.rs",
                vec![bare("AaaAnchor"), bare("ZzzLate")],
            ),
            referencing(
                "ref_anchor_3",
                "src/r3.rs",
                vec![bare("AaaAnchor"), bare("AabFiller")],
            ),
        ]);
        insert_all(&conn, "ctx-a", segments).await;

        let overview = compute(&conn, "ctx-a").await;

        assert_eq!(
            overview
                .top_symbols
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["AaaAnchor"],
            "a key whose definitions may have been cut by the resolution limit \
             must be skipped as ambiguous, not misattributed"
        );
    }

    #[tokio::test]
    async fn overview_recompute_is_identical() {
        let (_db, conn) = setup().await;
        insert_all(&conn, "ctx-a", dependency_fixture()).await;

        let first = compute(&conn, "ctx-a").await;
        let second = compute(&conn, "ctx-a").await;

        assert!(!first.top_symbols.is_empty());
        assert!(!first.module_dependencies.is_empty());
        assert_eq!(first, second);
    }

    #[test]
    fn overview_policy_constants_match_storage_sql_fragments() {
        for kind in QUALIFYING_DEFINITION_KINDS {
            assert!(
                queries::OVERVIEW_QUALIFYING_TYPE_KINDS_SQL.contains(&format!("'{kind}'")),
                "qualifying kind {kind} missing from SQL fragment"
            );
        }
        assert_eq!(
            queries::OVERVIEW_QUALIFYING_TYPE_KINDS_SQL
                .matches('\'')
                .count(),
            QUALIFYING_DEFINITION_KINDS.len() * 2
        );
        for role in QUALIFYING_DEFINITION_ROLES {
            assert!(
                queries::OVERVIEW_QUALIFYING_ROLES_SQL.contains(&format!("'{role}'")),
                "qualifying role {role} missing from SQL fragment"
            );
        }
        assert_eq!(
            queries::OVERVIEW_QUALIFYING_ROLES_SQL.matches('\'').count(),
            QUALIFYING_DEFINITION_ROLES.len() * 2
        );
        for edge_kind in IDENTITY_BEARING_EDGE_KINDS {
            assert!(
                queries::OVERVIEW_IDENTITY_BEARING_EDGE_KINDS_SQL
                    .contains(&format!("'{edge_kind}'")),
                "edge kind {edge_kind} missing from SQL fragment"
            );
        }
        assert_eq!(
            queries::OVERVIEW_IDENTITY_BEARING_EDGE_KINDS_SQL
                .matches('\'')
                .count(),
            IDENTITY_BEARING_EDGE_KINDS.len() * 2
        );
    }
}
