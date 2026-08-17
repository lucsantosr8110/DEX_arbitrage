//! Persistence for Phase 2D-C2B round artifacts: `executable_edges.jsonl`,
//! `structural_routes.jsonl`, `pinned_leg_quotes.jsonl`,
//! `route_artifact.jsonl`, `canonical_execution_context.json`. Generic
//! JSONL helpers are reused across all typed artifacts; `verify_reloaded`
//! cross-checks that every reference a reloaded route/edge carries still
//! resolves inside the reloaded context — fail closed on any dangling
//! reference, never silently reconstructed.

use crate::core::{
    canonical_execution_context::CanonicalExecutionContext,
    executable_price_edge::ExecutablePriceEdge, route_artifact::StructuralRoute,
};
use ethers::types::{Address, H256};
use serde::{de::DeserializeOwned, Serialize};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ArtifactError {
    #[error("ARTIFACT_IO: {0}")]
    Io(String),
    #[error("ARTIFACT_SERDE: {0}")]
    Serde(String),
    #[error("ARTIFACT_CONTEXT_HASH_MISMATCH")]
    ContextHashMismatch,
    #[error("ARTIFACT_DANGLING_REFERENCE: {0}")]
    DanglingReference(String),
}

pub fn write_jsonl<T: Serialize>(path: &Path, items: &[T]) -> Result<(), ArtifactError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ArtifactError::Io(e.to_string()))?;
    }
    let mut f = std::fs::File::create(path).map_err(|e| ArtifactError::Io(e.to_string()))?;
    for item in items {
        let line = serde_json::to_string(item).map_err(|e| ArtifactError::Serde(e.to_string()))?;
        writeln!(f, "{line}").map_err(|e| ArtifactError::Io(e.to_string()))?;
    }
    Ok(())
}

pub fn read_jsonl<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>, ArtifactError> {
    let content = std::fs::read_to_string(path).map_err(|e| ArtifactError::Io(e.to_string()))?;
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).map_err(|e| ArtifactError::Serde(e.to_string())))
        .collect()
}

pub fn write_context(
    path: &Path,
    context: &CanonicalExecutionContext,
) -> Result<(), ArtifactError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ArtifactError::Io(e.to_string()))?;
    }
    let json =
        serde_json::to_string_pretty(context).map_err(|e| ArtifactError::Serde(e.to_string()))?;
    std::fs::write(path, json).map_err(|e| ArtifactError::Io(e.to_string()))
}

pub fn read_context(path: &Path) -> Result<CanonicalExecutionContext, ArtifactError> {
    let content = std::fs::read_to_string(path).map_err(|e| ArtifactError::Io(e.to_string()))?;
    serde_json::from_str(&content).map_err(|e| ArtifactError::Serde(e.to_string()))
}

#[derive(Debug, Clone)]
pub struct RoundArtifactPaths {
    pub executable_edges: PathBuf,
    pub structural_routes: PathBuf,
    pub pinned_leg_quotes: PathBuf,
    pub route_artifact: PathBuf,
    pub canonical_execution_context: PathBuf,
}

pub fn round_artifact_paths(dir: &Path, round_id: usize) -> RoundArtifactPaths {
    let round_dir = dir.join("phase2d_c2b").join(format!("round_{round_id}"));
    RoundArtifactPaths {
        executable_edges: round_dir.join("executable_edges.jsonl"),
        structural_routes: round_dir.join("structural_routes.jsonl"),
        pinned_leg_quotes: round_dir.join("pinned_leg_quotes.jsonl"),
        route_artifact: round_dir.join("route_artifact.jsonl"),
        canonical_execution_context: round_dir.join("canonical_execution_context.json"),
    }
}

/// Cross-checks a reloaded `(context, edges, routes)` triple: the context
/// hash is authentic (`CanonicalExecutionContext::verify_reload`), every
/// structural route carries `executable_legs`, every leg's pool resolves to
/// a reloaded edge, and every edge's `pool_state_id`/`execution_metadata_id`
/// resolves against the reloaded context's pool/pool-state entries.
pub fn verify_reloaded(
    context: &CanonicalExecutionContext,
    edges: &[ExecutablePriceEdge],
    routes: &[StructuralRoute],
) -> Result<(), ArtifactError> {
    context
        .verify_reload()
        .map_err(|_| ArtifactError::ContextHashMismatch)?;

    let edge_pools: HashSet<Address> = edges.iter().map(|e| e.pool).collect();
    for route in routes {
        let legs = route
            .executable_legs
            .as_ref()
            .ok_or_else(|| ArtifactError::DanglingReference(route.route_id.clone()))?;
        for leg in legs {
            if !edge_pools.contains(&leg.pool) {
                return Err(ArtifactError::DanglingReference(format!(
                    "route={} pool={:?} not found among reloaded edges",
                    route.route_id, leg.pool
                )));
            }
        }
    }

    let pool_state_ids: HashSet<H256> = context
        .pool_states
        .values()
        .map(|s| H256::from(ethers::utils::keccak256(s.state_id.as_bytes())))
        .collect();
    for edge in edges {
        if !pool_state_ids.contains(&edge.pool_state_id) {
            return Err(ArtifactError::DanglingReference(format!(
                "edge_id={:?} pool_state_id={:?} not found in reloaded context",
                edge.edge_id, edge.pool_state_id
            )));
        }
        let pool_key = format!("{:?}", edge.pool);
        let pool_meta = context.pools.get(&pool_key).ok_or_else(|| {
            ArtifactError::DanglingReference(format!(
                "edge_id={:?} pool={pool_key} not found in reloaded context",
                edge.edge_id
            ))
        })?;
        let recomputed_meta_id = H256::from(ethers::utils::keccak256(
            format!("{pool_meta:?}").as_bytes(),
        ));
        if recomputed_meta_id != edge.execution_metadata_id {
            return Err(ArtifactError::DanglingReference(format!(
                "edge_id={:?} execution_metadata_id mismatch after reload",
                edge.edge_id
            )));
        }
    }
    Ok(())
}
