
use std::collections::HashSet;
#[cfg(test)]
use std::fmt::Write;

use libsql::Connection;

#[cfg(test)]
use crate::shared::constants::DEFAULT_INDEX_CONTEXT_ID;
use crate::shared::errors::{OneupError, StorageError};
use crate::shared::symbols::{
    normalize_edge_identity_kind, normalize_symbolish, owner_fingerprint_from_components,
    split_symbol_components,
};
use crate::shared::types::{ParsedRelation, ParsedRelationKind};
use crate::storage::queries;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelationKind {
    Call,
    Reference,
    Conformance,
}

impl RelationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Reference => "reference",
            Self::Conformance => "conformance",
        }
    }

    fn from_db(value: &str) -> Result<Self, OneupError> {
        match value {
            "call" => Ok(Self::Call),
            "reference" => Ok(Self::Reference),
            "conformance" => Ok(Self::Conformance),
            _ => Err(StorageError::Query(format!("unknown relation kind '{value}'")).into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationInsert {
    pub source_segment_id: String,
    pub relation_kind: RelationKind,
    pub raw_target_symbol: String,
    pub canonical_target_symbol: String,
    pub lookup_canonical_symbol: String,
    pub qualifier_fingerprint: String,
    pub edge_identity_kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRelation {
    pub source_segment_id: String,
    pub relation_kind: RelationKind,
    pub raw_target_symbol: String,
    pub canonical_target_symbol: String,
    pub lookup_canonical_symbol: String,
    pub qualifier_fingerprint: String,
    pub edge_identity_kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RelationTargetDescriptor {
    canonical_target_symbol: String,
    lookup_canonical_symbol: String,
    qualifier_fingerprint: String,
}

pub fn build_relation_inserts(
    source_segment_id: &str,
    called_relations: &[ParsedRelation],
    referenced_relations: &[ParsedRelation],
) -> Vec<RelationInsert> {
    let mut relations = Vec::new();
    let mut seen = HashSet::new();

    for (relation_kind, relation_group) in [
        (RelationKind::Call, called_relations),
        (RelationKind::Reference, referenced_relations),
    ] {
        for relation in relation_group {
            let relation_kind = relation_kind_for(relation, relation_kind);
            let Some(descriptor) = relation_target_descriptor(&relation.symbol) else {
                continue;
            };
            let edge_identity_kind = normalize_edge_identity_kind(&relation.edge_identity_kind);

            let dedupe_key = (
                relation_kind,
                descriptor.lookup_canonical_symbol.clone(),
                descriptor.qualifier_fingerprint.clone(),
                edge_identity_kind.clone(),
            );
            if seen.insert(dedupe_key) {
                relations.push(RelationInsert {
                    source_segment_id: source_segment_id.to_string(),
                    relation_kind,
                    raw_target_symbol: relation.symbol.clone(),
                    canonical_target_symbol: descriptor.canonical_target_symbol,
                    lookup_canonical_symbol: descriptor.lookup_canonical_symbol,
                    qualifier_fingerprint: descriptor.qualifier_fingerprint,
                    edge_identity_kind,
                });
            }
        }
    }

    relations
}

fn relation_kind_for(relation: &ParsedRelation, default_kind: RelationKind) -> RelationKind {
    match relation.kind {
        Some(ParsedRelationKind::Call) => RelationKind::Call,
        Some(ParsedRelationKind::Reference) => RelationKind::Reference,
        Some(ParsedRelationKind::Conformance) => RelationKind::Conformance,
        None => default_kind,
    }
}

fn relation_target_descriptor(raw_target_symbol: &str) -> Option<RelationTargetDescriptor> {
    let canonical_target_symbol = normalize_symbolish(raw_target_symbol);
    if canonical_target_symbol.is_empty() {
        return None;
    }

    let components = split_symbol_components(raw_target_symbol);

    let lookup_canonical_symbol = components
        .last()
        .cloned()
        .unwrap_or_else(|| canonical_target_symbol.clone());
    let qualifier_fingerprint = owner_fingerprint_from_components(&components);

    Some(RelationTargetDescriptor {
        canonical_target_symbol,
        lookup_canonical_symbol,
        qualifier_fingerprint,
    })
}

#[cfg(test)]
pub async fn get_outbound_relations(
    conn: &Connection,
    source_segment_id: &str,
    relation_kind: Option<RelationKind>,
    limit: usize,
) -> Result<Vec<StoredRelation>, OneupError> {
    get_outbound_relations_for_context(
        conn,
        DEFAULT_INDEX_CONTEXT_ID,
        source_segment_id,
        relation_kind,
        limit,
    )
    .await
}

/// Outbound relation lookup for one source segment. Doc-mention rows are
/// excluded in SQL so documentation evidence never consumes the bounded
/// fetch window (`LIMIT`) that impact budgets rely on.
pub async fn get_outbound_relations_for_context(
    conn: &Connection,
    context_id: &str,
    source_segment_id: &str,
    relation_kind: Option<RelationKind>,
    limit: usize,
) -> Result<Vec<StoredRelation>, OneupError> {
    let Some(limit) = relation_limit(limit)? else {
        return Ok(Vec::new());
    };

    let mut rows = match relation_kind {
        Some(relation_kind) => conn
            .query(
                queries::SELECT_OUTBOUND_RELATIONS_BY_KIND_FOR_CONTEXT,
                libsql::params![context_id, source_segment_id, relation_kind.as_str(), limit],
            )
            .await
            .map_err(|e| StorageError::Query(format!("outbound relation lookup failed: {e}")))?,
        None => conn
            .query(
                queries::SELECT_OUTBOUND_RELATIONS_FOR_CONTEXT,
                libsql::params![context_id, source_segment_id, limit],
            )
            .await
            .map_err(|e| StorageError::Query(format!("outbound relation lookup failed: {e}")))?,
    };

    let mut results = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("outbound relation row iteration failed: {e}")))?
    {
        results.push(row_to_stored_relation(&row)?);
    }

    Ok(results)
}

#[cfg(test)]
pub async fn get_inbound_relations(
    conn: &Connection,
    canonical_target_symbol: &str,
    relation_kind: Option<RelationKind>,
    limit: usize,
) -> Result<Vec<StoredRelation>, OneupError> {
    get_inbound_relations_for_context(
        conn,
        DEFAULT_INDEX_CONTEXT_ID,
        canonical_target_symbol,
        relation_kind,
        limit,
    )
    .await
}

#[cfg(test)]
pub async fn get_inbound_relations_for_context(
    conn: &Connection,
    context_id: &str,
    canonical_target_symbol: &str,
    relation_kind: Option<RelationKind>,
    limit: usize,
) -> Result<Vec<StoredRelation>, OneupError> {
    let Some(limit) = relation_limit(limit)? else {
        return Ok(Vec::new());
    };

    let mut rows = match relation_kind {
        Some(relation_kind) => conn
            .query(
                queries::SELECT_INBOUND_RELATIONS_BY_KIND_FOR_CONTEXT,
                libsql::params![
                    context_id,
                    canonical_target_symbol,
                    relation_kind.as_str(),
                    limit
                ],
            )
            .await
            .map_err(|e| StorageError::Query(format!("inbound relation lookup failed: {e}")))?,
        None => conn
            .query(
                queries::SELECT_INBOUND_RELATIONS_FOR_CONTEXT,
                libsql::params![context_id, canonical_target_symbol, limit],
            )
            .await
            .map_err(|e| StorageError::Query(format!("inbound relation lookup failed: {e}")))?,
    };

    let mut results = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("inbound relation row iteration failed: {e}")))?
    {
        results.push(row_to_stored_relation(&row)?);
    }

    Ok(results)
}

#[cfg(test)]
pub async fn get_inbound_relations_by_lookup_symbol(
    conn: &Connection,
    lookup_canonical_symbol: &str,
    relation_kind: Option<RelationKind>,
    limit: usize,
) -> Result<Vec<StoredRelation>, OneupError> {
    get_inbound_relations_by_lookup_symbol_for_context(
        conn,
        DEFAULT_INDEX_CONTEXT_ID,
        lookup_canonical_symbol,
        relation_kind,
        limit,
    )
    .await
}

/// Inbound relation lookup by canonical symbol tail. Doc-mention rows are
/// excluded in SQL so a heavily documented symbol cannot evict real code
/// references from the bounded fetch window (`LIMIT`).
pub async fn get_inbound_relations_by_lookup_symbol_for_context(
    conn: &Connection,
    context_id: &str,
    lookup_canonical_symbol: &str,
    relation_kind: Option<RelationKind>,
    limit: usize,
) -> Result<Vec<StoredRelation>, OneupError> {
    let Some(limit) = relation_limit(limit)? else {
        return Ok(Vec::new());
    };

    let mut rows = match relation_kind {
        Some(relation_kind) => conn
            .query(
                queries::SELECT_INBOUND_RELATIONS_BY_LOOKUP_SYMBOL_AND_KIND_FOR_CONTEXT,
                libsql::params![
                    context_id,
                    lookup_canonical_symbol,
                    relation_kind.as_str(),
                    limit
                ],
            )
            .await
            .map_err(|e| {
                StorageError::Query(format!("inbound lookup relation lookup failed: {e}"))
            })?,
        None => conn
            .query(
                queries::SELECT_INBOUND_RELATIONS_BY_LOOKUP_SYMBOL_FOR_CONTEXT,
                libsql::params![context_id, lookup_canonical_symbol, limit],
            )
            .await
            .map_err(|e| {
                StorageError::Query(format!("inbound lookup relation lookup failed: {e}"))
            })?,
    };

    let mut results = Vec::new();
    while let Some(row) = rows.next().await.map_err(|e| {
        StorageError::Query(format!("inbound lookup relation row iteration failed: {e}"))
    })? {
        results.push(row_to_stored_relation(&row)?);
    }

    Ok(results)
}

/// One ranked overview symbol key with the breadth of incoming references
/// (distinct referencing source files) and its qualifying definition count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolReferenceCount {
    pub symbol_key: String,
    pub referencing_files: u64,
    pub definition_count: u64,
}

/// One directed depth-2 module dependency aggregated from relation rows,
/// counted as distinct (referencing file, symbol key) pairs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleDependencyPair {
    pub source_module: String,
    pub target_module: String,
    pub pair_count: u64,
}

/// Rank overview symbol keys by distinct referencing source files inside one
/// index context, restricted to identity-bearing relation rows joined to
/// qualifying type definitions (Branch B kind policy).
pub async fn get_top_type_symbol_references_for_context(
    conn: &Connection,
    context_id: &str,
    limit: usize,
) -> Result<Vec<SymbolReferenceCount>, OneupError> {
    let Some(limit) = relation_limit(limit)? else {
        return Ok(Vec::new());
    };

    let mut rows = conn
        .query(
            queries::SELECT_TOP_TYPE_SYMBOL_REFERENCES_FOR_CONTEXT.as_str(),
            libsql::params![context_id, limit],
        )
        .await
        .map_err(|e| StorageError::Query(format!("top type symbol ranking failed: {e}")))?;

    let mut results = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("top type symbol row iteration failed: {e}")))?
    {
        let referencing_files: i64 = row
            .get(1)
            .map_err(|e| StorageError::Query(format!("read referencing_files failed: {e}")))?;
        let definition_count: i64 = row
            .get(2)
            .map_err(|e| StorageError::Query(format!("read definition_count failed: {e}")))?;
        results.push(SymbolReferenceCount {
            symbol_key: row
                .get(0)
                .map_err(|e| StorageError::Query(format!("read symbol_key failed: {e}")))?,
            referencing_files: referencing_files as u64,
            definition_count: definition_count as u64,
        });
    }

    Ok(results)
}

/// Aggregate directed depth-2 module dependency pairs inside one index
/// context, sharing the top-symbol filter stack plus the SQL-side per-key
/// qualifying-definition-count cap of 1..=3.
pub async fn get_module_dependency_pairs_for_context(
    conn: &Connection,
    context_id: &str,
    limit: usize,
) -> Result<Vec<ModuleDependencyPair>, OneupError> {
    let Some(limit) = relation_limit(limit)? else {
        return Ok(Vec::new());
    };

    let mut rows = conn
        .query(
            queries::SELECT_MODULE_DEPENDENCY_PAIRS_FOR_CONTEXT.as_str(),
            libsql::params![context_id, limit],
        )
        .await
        .map_err(|e| StorageError::Query(format!("module dependency aggregate failed: {e}")))?;

    let mut results = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|e| StorageError::Query(format!("module dependency row iteration failed: {e}")))?
    {
        let pair_count: i64 = row
            .get(2)
            .map_err(|e| StorageError::Query(format!("read pair_count failed: {e}")))?;
        results.push(ModuleDependencyPair {
            source_module: row
                .get(0)
                .map_err(|e| StorageError::Query(format!("read source_module failed: {e}")))?,
            target_module: row
                .get(1)
                .map_err(|e| StorageError::Query(format!("read target_module failed: {e}")))?,
            pair_count: pair_count as u64,
        });
    }

    Ok(results)
}

#[cfg(test)]
pub(crate) async fn insert_relations(
    conn: &Connection,
    relations: &[RelationInsert],
) -> Result<(), OneupError> {
    if relations.is_empty() {
        return Ok(());
    }

    for chunk in relations.chunks(queries::RELATION_CHUNK_SIZE) {
        let mut sql = String::from(
            "INSERT OR REPLACE INTO segment_relations (\
             source_segment_id, relation_kind, raw_target_symbol, \
             canonical_target_symbol, lookup_canonical_symbol, \
             qualifier_fingerprint, edge_identity_kind, created_at\
             ) VALUES ",
        );
        let mut params: Vec<libsql::Value> =
            Vec::with_capacity(chunk.len() * queries::RELATION_INSERT_COLS);

        for (i, relation) in chunk.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            let base = i * queries::RELATION_INSERT_COLS;
            write!(
                sql,
                "(?{}, ?{}, ?{}, ?{}, ?{}, ?{}, ?{}, datetime('now'))",
                base + 1,
                base + 2,
                base + 3,
                base + 4,
                base + 5,
                base + 6,
                base + 7,
            )
            .expect("write to String cannot fail");

            params.push(relation.source_segment_id.clone().into());
            params.push(relation.relation_kind.as_str().to_string().into());
            params.push(relation.raw_target_symbol.clone().into());
            params.push(relation.canonical_target_symbol.clone().into());
            params.push(relation.lookup_canonical_symbol.clone().into());
            params.push(relation.qualifier_fingerprint.clone().into());
            params.push(relation.edge_identity_kind.clone().into());
        }

        conn.execute(&sql, params).await.map_err(|e| {
            StorageError::Query(format!("batch insert segment relations failed: {e}"))
        })?;
    }

    Ok(())
}

fn relation_limit(limit: usize) -> Result<Option<i64>, OneupError> {
    if limit == 0 {
        return Ok(None);
    }

    i64::try_from(limit).map(Some).map_err(|_| {
        StorageError::Query(format!("relation limit {limit} exceeds i64 range")).into()
    })
}

fn row_to_stored_relation(row: &libsql::Row) -> Result<StoredRelation, OneupError> {
    let relation_kind: String = row
        .get(1)
        .map_err(|e| StorageError::Query(format!("read relation_kind failed: {e}")))?;

    Ok(StoredRelation {
        source_segment_id: row
            .get(0)
            .map_err(|e| StorageError::Query(format!("read source_segment_id failed: {e}")))?,
        relation_kind: RelationKind::from_db(&relation_kind)?,
        raw_target_symbol: row
            .get(2)
            .map_err(|e| StorageError::Query(format!("read raw_target_symbol failed: {e}")))?,
        canonical_target_symbol: row.get(3).map_err(|e| {
            StorageError::Query(format!("read canonical_target_symbol failed: {e}"))
        })?,
        lookup_canonical_symbol: row.get(4).map_err(|e| {
            StorageError::Query(format!("read lookup_canonical_symbol failed: {e}"))
        })?,
        qualifier_fingerprint: row
            .get(5)
            .map_err(|e| StorageError::Query(format!("read qualifier_fingerprint failed: {e}")))?,
        edge_identity_kind: row
            .get(6)
            .map_err(|e| StorageError::Query(format!("read edge_identity_kind failed: {e}")))?,
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::shared::symbols::{
        EDGE_IDENTITY_BARE_IDENTIFIER, EDGE_IDENTITY_CONSTRUCTOR_LIKE, EDGE_IDENTITY_MACRO_LIKE,
        EDGE_IDENTITY_METHOD_RECEIVER, EDGE_IDENTITY_QUALIFIED_PATH,
    };
    use crate::storage::{
        db::Db,
        schema,
        segments::{self, SegmentInsert},
    };

    async fn setup() -> (Db, Connection) {
        let db = Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
        (db, conn)
    }

    fn test_segment(id: &str, file_path: &str) -> SegmentInsert {
        SegmentInsert {
            id: id.to_string(),
            file_path: file_path.to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: format!("fn {id}() {{ }}"),
            line_start: 1,
            line_end: 3,
            content_key: None,
            embedding_vec: None,
            breadcrumb: None,
            complexity: 1,
            role: "IMPLEMENTATION".to_string(),
            defined_symbols: format!("[\"{id}\"]"),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: format!("hash-{id}"),
        }
    }

    fn relation(symbol: &str, edge_identity_kind: &str) -> ParsedRelation {
        ParsedRelation {
            symbol: symbol.to_string(),
            edge_identity_kind: edge_identity_kind.to_string(),
            kind: None,
        }
    }

    fn conformance_relation(symbol: &str, edge_identity_kind: &str) -> ParsedRelation {
        ParsedRelation {
            symbol: symbol.to_string(),
            edge_identity_kind: edge_identity_kind.to_string(),
            kind: Some(ParsedRelationKind::Conformance),
        }
    }

    #[test]
    fn build_relation_inserts_preserves_distinct_edge_identity_and_conformance_kind() {
        let called_relations = vec![
            relation(
                "crate::auth::config::load_config",
                EDGE_IDENTITY_QUALIFIED_PATH,
            ),
            relation("auth.config.load_config", EDGE_IDENTITY_QUALIFIED_PATH),
            relation("service.client.fetch", EDGE_IDENTITY_METHOD_RECEIVER),
            relation("service.client.fetch", EDGE_IDENTITY_BARE_IDENTIFIER),
            relation(" ", EDGE_IDENTITY_BARE_IDENTIFIER),
        ];
        let referenced_relations = vec![
            relation("ConfigLoader", EDGE_IDENTITY_BARE_IDENTIFIER),
            relation("config_loader", EDGE_IDENTITY_BARE_IDENTIFIER),
            conformance_relation("crate::auth::Validator", EDGE_IDENTITY_QUALIFIED_PATH),
            relation("", EDGE_IDENTITY_BARE_IDENTIFIER),
        ];

        let relations = build_relation_inserts("seg-1", &called_relations, &referenced_relations);

        assert_eq!(
            relations,
            vec![
                RelationInsert {
                    source_segment_id: "seg-1".to_string(),
                    relation_kind: RelationKind::Call,
                    raw_target_symbol: "crate::auth::config::load_config".to_string(),
                    canonical_target_symbol: "crateauthconfigloadconfig".to_string(),
                    lookup_canonical_symbol: "loadconfig".to_string(),
                    qualifier_fingerprint: "auth/config".to_string(),
                    edge_identity_kind: EDGE_IDENTITY_QUALIFIED_PATH.to_string(),
                },
                RelationInsert {
                    source_segment_id: "seg-1".to_string(),
                    relation_kind: RelationKind::Call,
                    raw_target_symbol: "service.client.fetch".to_string(),
                    canonical_target_symbol: "serviceclientfetch".to_string(),
                    lookup_canonical_symbol: "fetch".to_string(),
                    qualifier_fingerprint: "service/client".to_string(),
                    edge_identity_kind: EDGE_IDENTITY_METHOD_RECEIVER.to_string(),
                },
                RelationInsert {
                    source_segment_id: "seg-1".to_string(),
                    relation_kind: RelationKind::Call,
                    raw_target_symbol: "service.client.fetch".to_string(),
                    canonical_target_symbol: "serviceclientfetch".to_string(),
                    lookup_canonical_symbol: "fetch".to_string(),
                    qualifier_fingerprint: "service/client".to_string(),
                    edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
                },
                RelationInsert {
                    source_segment_id: "seg-1".to_string(),
                    relation_kind: RelationKind::Reference,
                    raw_target_symbol: "ConfigLoader".to_string(),
                    canonical_target_symbol: "configloader".to_string(),
                    lookup_canonical_symbol: "configloader".to_string(),
                    qualifier_fingerprint: String::new(),
                    edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
                },
                RelationInsert {
                    source_segment_id: "seg-1".to_string(),
                    relation_kind: RelationKind::Conformance,
                    raw_target_symbol: "crate::auth::Validator".to_string(),
                    canonical_target_symbol: "crateauthvalidator".to_string(),
                    lookup_canonical_symbol: "validator".to_string(),
                    qualifier_fingerprint: "auth".to_string(),
                    edge_identity_kind: EDGE_IDENTITY_QUALIFIED_PATH.to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn relation_lookup_helpers_filter_and_bound_results() {
        let (_db, conn) = setup().await;

        let mut source_a = test_segment("source_a", "src/a.rs");
        source_a.called_symbols =
            r#"["crate::auth::config::load_config","write_config"]"#.to_string();
        source_a.called_relations = serde_json::to_string(&vec![
            relation(
                "crate::auth::config::load_config",
                EDGE_IDENTITY_QUALIFIED_PATH,
            ),
            relation("write_config", EDGE_IDENTITY_BARE_IDENTIFIER),
        ])
        .unwrap();
        source_a.referenced_symbols = r#"["ConfigLoader"]"#.to_string();
        source_a.referenced_relations = serde_json::to_string(&vec![relation(
            "ConfigLoader",
            EDGE_IDENTITY_BARE_IDENTIFIER,
        )])
        .unwrap();
        segments::upsert_segment(&conn, &source_a).await.unwrap();

        let mut source_b = test_segment("source_b", "src/b.rs");
        source_b.called_symbols = r#"["auth.config.load_config"]"#.to_string();
        source_b.called_relations = serde_json::to_string(&vec![relation(
            "auth.config.load_config",
            EDGE_IDENTITY_QUALIFIED_PATH,
        )])
        .unwrap();
        source_b.referenced_symbols = r#"["ConfigLoader","Settings"]"#.to_string();
        source_b.referenced_relations = serde_json::to_string(&vec![
            relation("ConfigLoader", EDGE_IDENTITY_BARE_IDENTIFIER),
            relation("Settings", EDGE_IDENTITY_BARE_IDENTIFIER),
        ])
        .unwrap();
        segments::upsert_segment(&conn, &source_b).await.unwrap();

        let mut source_c = test_segment("source_c", "src/c.rs");
        source_c.defined_symbols = r#"["AuthStore"]"#.to_string();
        source_c.referenced_symbols = r#"["SessionStore"]"#.to_string();
        source_c.referenced_relations = serde_json::to_string(&vec![conformance_relation(
            "contracts.SessionStore",
            EDGE_IDENTITY_QUALIFIED_PATH,
        )])
        .unwrap();
        segments::upsert_segment(&conn, &source_c).await.unwrap();

        let outbound = get_outbound_relations(&conn, "source_a", None, 2)
            .await
            .unwrap();
        assert_eq!(outbound.len(), 2);
        assert_eq!(outbound[0].relation_kind, RelationKind::Call);
        assert_eq!(
            outbound[0].canonical_target_symbol,
            "crateauthconfigloadconfig"
        );
        assert_eq!(outbound[0].lookup_canonical_symbol, "loadconfig");
        assert_eq!(outbound[0].qualifier_fingerprint, "auth/config");
        assert_eq!(outbound[0].edge_identity_kind, EDGE_IDENTITY_QUALIFIED_PATH);
        assert_eq!(outbound[1].relation_kind, RelationKind::Call);
        assert_eq!(outbound[1].canonical_target_symbol, "writeconfig");
        assert_eq!(outbound[1].lookup_canonical_symbol, "writeconfig");
        assert!(outbound[1].qualifier_fingerprint.is_empty());
        assert_eq!(
            outbound[1].edge_identity_kind,
            EDGE_IDENTITY_BARE_IDENTIFIER
        );

        let inbound =
            get_inbound_relations(&conn, "configloader", Some(RelationKind::Reference), 8)
                .await
                .unwrap();
        assert_eq!(
            inbound
                .iter()
                .map(|relation| relation.source_segment_id.as_str())
                .collect::<Vec<_>>(),
            vec!["source_a", "source_b"]
        );

        let lookup_inbound = get_inbound_relations_by_lookup_symbol(
            &conn,
            "loadconfig",
            Some(RelationKind::Call),
            8,
        )
        .await
        .unwrap();
        assert_eq!(
            lookup_inbound
                .iter()
                .map(|relation| relation.source_segment_id.as_str())
                .collect::<Vec<_>>(),
            vec!["source_a", "source_b"]
        );

        let conformance_inbound = get_inbound_relations_by_lookup_symbol(
            &conn,
            "sessionstore",
            Some(RelationKind::Conformance),
            8,
        )
        .await
        .unwrap();
        assert_eq!(
            conformance_inbound
                .iter()
                .map(|relation| relation.source_segment_id.as_str())
                .collect::<Vec<_>>(),
            vec!["source_c"]
        );

        let empty = get_outbound_relations(&conn, "source_a", None, 0)
            .await
            .unwrap();
        assert!(empty.is_empty());
    }

    fn overview_definition(
        id: &str,
        file_path: &str,
        block_type: &str,
        symbol: &str,
    ) -> SegmentInsert {
        let mut seg = test_segment(id, file_path);
        seg.block_type = block_type.to_string();
        seg.role = "DEFINITION".to_string();
        seg.defined_symbols = serde_json::to_string(&[symbol]).unwrap();
        seg
    }

    fn overview_referencing(id: &str, file_path: &str, refs: Vec<ParsedRelation>) -> SegmentInsert {
        let mut seg = test_segment(id, file_path);
        seg.defined_symbols = "[]".to_string();
        seg.referenced_relations = serde_json::to_string(&refs).unwrap();
        seg
    }

    #[tokio::test]
    async fn top_type_symbols_ranked_by_distinct_referencing_files() {
        let (_db, conn) = setup().await;
        let ctx = "ctx-a";

        for seg in [
            overview_definition("def_db", "src/storage/db.rs", "struct", "Db"),
            overview_definition("def_err", "src/shared/errors.rs", "enum", "OneupError"),
            overview_definition("def_alpha", "src/cli/alpha.rs", "struct", "Alpha"),
            overview_definition("def_beta", "src/cli/beta.rs", "struct", "Beta"),
            overview_definition("def_helper", "src/mcp/tools.rs", "function", "helper"),
            overview_definition("def_alias", "src/shared/types.rs", "type", "Err"),
            overview_referencing(
                "ref_a",
                "src/cli/a.rs",
                vec![
                    relation("Db", EDGE_IDENTITY_QUALIFIED_PATH),
                    relation("Db", EDGE_IDENTITY_BARE_IDENTIFIER),
                    relation("OneupError", EDGE_IDENTITY_BARE_IDENTIFIER),
                    relation("helper", EDGE_IDENTITY_BARE_IDENTIFIER),
                    relation("Err", EDGE_IDENTITY_BARE_IDENTIFIER),
                ],
            ),
            overview_referencing(
                "ref_b",
                "src/cli/b.rs",
                vec![
                    relation("Db", EDGE_IDENTITY_BARE_IDENTIFIER),
                    relation("Alpha", EDGE_IDENTITY_BARE_IDENTIFIER),
                    relation("Beta", EDGE_IDENTITY_BARE_IDENTIFIER),
                    relation("OneupError", EDGE_IDENTITY_MACRO_LIKE),
                    relation("helper", EDGE_IDENTITY_BARE_IDENTIFIER),
                ],
            ),
            overview_referencing(
                "ref_c",
                "src/search/c.rs",
                vec![
                    relation("Db", EDGE_IDENTITY_CONSTRUCTOR_LIKE),
                    relation("helper", EDGE_IDENTITY_BARE_IDENTIFIER),
                ],
            ),
            overview_referencing(
                "ref_d",
                "src/daemon/d.rs",
                vec![
                    relation("Db", EDGE_IDENTITY_METHOD_RECEIVER),
                    relation("helper", EDGE_IDENTITY_BARE_IDENTIFIER),
                ],
            ),
        ] {
            segments::upsert_segment_for_context(&conn, ctx, &seg)
                .await
                .unwrap();
        }

        let ranked = get_top_type_symbol_references_for_context(&conn, ctx, 10)
            .await
            .unwrap();
        assert_eq!(
            ranked,
            vec![
                SymbolReferenceCount {
                    symbol_key: "db".to_string(),
                    referencing_files: 3,
                    definition_count: 1,
                },
                SymbolReferenceCount {
                    symbol_key: "alpha".to_string(),
                    referencing_files: 1,
                    definition_count: 1,
                },
                SymbolReferenceCount {
                    symbol_key: "beta".to_string(),
                    referencing_files: 1,
                    definition_count: 1,
                },
                SymbolReferenceCount {
                    symbol_key: "oneuperror".to_string(),
                    referencing_files: 1,
                    definition_count: 1,
                },
            ]
        );

        let capped = get_top_type_symbol_references_for_context(&conn, ctx, 2)
            .await
            .unwrap();
        assert_eq!(capped.len(), 2);
        assert_eq!(capped[0].symbol_key, "db");

        segments::upsert_segment_for_context(
            &conn,
            "ctx-b",
            &overview_definition("b_def_db", "src/storage/db.rs", "struct", "Db"),
        )
        .await
        .unwrap();
        segments::upsert_segment_for_context(
            &conn,
            "ctx-b",
            &overview_referencing(
                "b_ref",
                "src/x.rs",
                vec![relation("Db", EDGE_IDENTITY_BARE_IDENTIFIER)],
            ),
        )
        .await
        .unwrap();

        let scoped = get_top_type_symbol_references_for_context(&conn, "ctx-b", 10)
            .await
            .unwrap();
        assert_eq!(
            scoped,
            vec![SymbolReferenceCount {
                symbol_key: "db".to_string(),
                referencing_files: 1,
                definition_count: 1,
            }]
        );
        assert!(get_top_type_symbol_references_for_context(&conn, ctx, 0)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn module_dependency_pairs_capped_and_filtered() {
        let (_db, conn) = setup().await;
        let ctx = "ctx-a";

        for seg in [
            overview_definition(
                "def_registry",
                "src/daemon/registry.rs",
                "struct",
                "Registry",
            ),
            overview_definition("def_pair_storage", "src/storage/p1.rs", "struct", "Pair"),
            overview_definition("def_pair_shared", "src/shared/p2.rs", "struct", "Pair"),
            overview_definition("def_dup_1", "src/a/d1.rs", "struct", "Dup"),
            overview_definition("def_dup_2", "src/b/d2.rs", "struct", "Dup"),
            overview_definition("def_dup_3", "src/c/d3.rs", "struct", "Dup"),
            overview_definition("def_dup_4", "src/d/d4.rs", "struct", "Dup"),
            overview_referencing(
                "ref_one_a",
                "src/cli/one.rs",
                vec![
                    relation("Registry", EDGE_IDENTITY_BARE_IDENTIFIER),
                    relation("Dup", EDGE_IDENTITY_BARE_IDENTIFIER),
                    relation("Pair", EDGE_IDENTITY_BARE_IDENTIFIER),
                ],
            ),
            overview_referencing(
                "ref_one_b",
                "src/cli/one.rs",
                vec![relation("Registry", EDGE_IDENTITY_QUALIFIED_PATH)],
            ),
            overview_referencing(
                "ref_two",
                "src/cli/two.rs",
                vec![relation("Registry", EDGE_IDENTITY_QUALIFIED_PATH)],
            ),
            overview_referencing(
                "ref_tests",
                "tests/integration.rs",
                vec![relation("Registry", EDGE_IDENTITY_BARE_IDENTIFIER)],
            ),
            overview_referencing(
                "ref_root",
                "main.rs",
                vec![relation("Registry", EDGE_IDENTITY_BARE_IDENTIFIER)],
            ),
            overview_referencing(
                "ref_macro_only",
                "src/search/s.rs",
                vec![relation("Registry", EDGE_IDENTITY_MACRO_LIKE)],
            ),
            overview_referencing(
                "ref_worker",
                "src/daemon/worker.rs",
                vec![relation("Registry", EDGE_IDENTITY_BARE_IDENTIFIER)],
            ),
        ] {
            segments::upsert_segment_for_context(&conn, ctx, &seg)
                .await
                .unwrap();
        }

        let pairs = get_module_dependency_pairs_for_context(&conn, ctx, 20)
            .await
            .unwrap();
        assert_eq!(
            pairs,
            vec![
                ModuleDependencyPair {
                    source_module: "src/cli".to_string(),
                    target_module: "src/daemon".to_string(),
                    pair_count: 2,
                },
                ModuleDependencyPair {
                    source_module: "(root)".to_string(),
                    target_module: "src/daemon".to_string(),
                    pair_count: 1,
                },
                ModuleDependencyPair {
                    source_module: "src/cli".to_string(),
                    target_module: "src/shared".to_string(),
                    pair_count: 1,
                },
                ModuleDependencyPair {
                    source_module: "src/cli".to_string(),
                    target_module: "src/storage".to_string(),
                    pair_count: 1,
                },
                ModuleDependencyPair {
                    source_module: "src/daemon".to_string(),
                    target_module: "src/daemon".to_string(),
                    pair_count: 1,
                },
                ModuleDependencyPair {
                    source_module: "tests".to_string(),
                    target_module: "src/daemon".to_string(),
                    pair_count: 1,
                },
            ]
        );

        let capped = get_module_dependency_pairs_for_context(&conn, ctx, 1)
            .await
            .unwrap();
        assert_eq!(capped.len(), 1);
        assert!(get_module_dependency_pairs_for_context(&conn, ctx, 0)
            .await
            .unwrap()
            .is_empty());
    }

    /// Latency gate for the overview aggregates: the symbol and
    /// module-dependency queries must stay within the ~1s budget on an index
    /// of representative scale (measurement showed 0.186-0.232s on 81k
    /// relations; the prohibited correlated form measured 183.8s).
    #[tokio::test]
    async fn overview_aggregates_meet_latency_budget_on_representative_index() {
        let (_db, conn) = setup().await;

        const REFERENCING_SEGMENTS: usize = 400;
        const TYPE_KEYS: usize = 600;
        const DEFINED_TARGET_ROWS: usize = 48_000;
        const RECEIVER_NOISE_ROWS: usize = 16_000;
        const UNDEFINED_TAIL_ROWS: usize = 16_000;

        for index in 0..REFERENCING_SEGMENTS {
            let id = format!("latency_ref_{index}");
            let file_path = format!("app/m{}/file_{index}.rs", index % 8);
            let mut seg = test_segment(&id, &file_path);
            seg.defined_symbols = "[]".to_string();
            segments::upsert_segment(&conn, &seg).await.unwrap();
        }
        for key in 0..TYPE_KEYS {
            let id = format!("latency_def_{key}");
            let file_path = format!("src/m{}/types_{key}.rs", key % 12);
            let mut seg = test_segment(&id, &file_path);
            seg.block_type = "struct".to_string();
            seg.role = "DEFINITION".to_string();
            seg.defined_symbols = format!("[\"Type{key}\"]");
            segments::upsert_segment(&conn, &seg).await.unwrap();
        }

        let mut rows =
            Vec::with_capacity(DEFINED_TARGET_ROWS + RECEIVER_NOISE_ROWS + UNDEFINED_TAIL_ROWS);
        for index in 0..DEFINED_TARGET_ROWS {
            let key = index % TYPE_KEYS;
            rows.push(RelationInsert {
                source_segment_id: format!(
                    "latency_ref_{}",
                    (index / TYPE_KEYS) % REFERENCING_SEGMENTS
                ),
                relation_kind: RelationKind::Reference,
                raw_target_symbol: format!("v{index}::Type{key}"),
                canonical_target_symbol: format!("v{index}type{key}"),
                lookup_canonical_symbol: format!("type{key}"),
                qualifier_fingerprint: String::new(),
                edge_identity_kind: if index % 2 == 0 {
                    EDGE_IDENTITY_BARE_IDENTIFIER.to_string()
                } else {
                    EDGE_IDENTITY_QUALIFIED_PATH.to_string()
                },
            });
        }
        for index in 0..RECEIVER_NOISE_ROWS {
            let key = index % TYPE_KEYS;
            rows.push(RelationInsert {
                source_segment_id: format!("latency_ref_{}", index % REFERENCING_SEGMENTS),
                relation_kind: RelationKind::Call,
                raw_target_symbol: format!("recv{index}.type_{key}"),
                canonical_target_symbol: format!("recv{index}type{key}"),
                lookup_canonical_symbol: format!("type{key}"),
                qualifier_fingerprint: String::new(),
                edge_identity_kind: EDGE_IDENTITY_METHOD_RECEIVER.to_string(),
            });
        }
        for index in 0..UNDEFINED_TAIL_ROWS {
            let key = index % 2_000;
            rows.push(RelationInsert {
                source_segment_id: format!("latency_ref_{}", index % REFERENCING_SEGMENTS),
                relation_kind: RelationKind::Reference,
                raw_target_symbol: format!("u{index}::tail_{key}"),
                canonical_target_symbol: format!("u{index}tail{key}"),
                lookup_canonical_symbol: format!("tail{key}"),
                qualifier_fingerprint: String::new(),
                edge_identity_kind: EDGE_IDENTITY_BARE_IDENTIFIER.to_string(),
            });
        }
        insert_relations(&conn, &rows).await.unwrap();

        let budget = Duration::from_secs(1);

        let started = Instant::now();
        let ranked =
            get_top_type_symbol_references_for_context(&conn, DEFAULT_INDEX_CONTEXT_ID, 120)
                .await
                .unwrap();
        let ranking_elapsed = started.elapsed();

        let started = Instant::now();
        let pairs = get_module_dependency_pairs_for_context(&conn, DEFAULT_INDEX_CONTEXT_ID, 64)
            .await
            .unwrap();
        let pairs_elapsed = started.elapsed();

        assert_eq!(ranked.len(), 120);
        assert!(ranked.iter().all(|entry| entry.referencing_files > 0));
        assert!(!pairs.is_empty());
        assert!(
            ranking_elapsed < budget,
            "symbol ranking took {ranking_elapsed:?}, budget {budget:?}"
        );
        assert!(
            pairs_elapsed < budget,
            "module dependency aggregate took {pairs_elapsed:?}, budget {budget:?}"
        );
    }
}
