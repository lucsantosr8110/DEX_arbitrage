//! Observation-only comparison between the legacy and canonical shadow paths.

use crate::core::executable_opportunity::ExecutableOpportunity;
use ethers::types::H256;
use serde::Serialize;
use std::{collections::BTreeSet, fs::OpenOptions, io::Write, path::Path};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum DivergenceKind {
    LegacyOnly,
    CanonicalOnly,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DualRunComparison {
    pub anchor_block: u64,
    pub opportunity_id: Option<H256>,
    pub kind: DivergenceKind,
}

#[derive(Debug, Default)]
pub struct DualRunComparator {
    legacy_keys: BTreeSet<String>,
}

impl DualRunComparator {
    pub fn record_legacy(&mut self, structural_cycle_key: impl Into<String>) {
        self.legacy_keys.insert(structural_cycle_key.into());
    }

    pub fn compare_and_clear(
        &mut self,
        anchor_block: u64,
        canonical: &[ExecutableOpportunity],
    ) -> Vec<DualRunComparison> {
        let canonical_keys: BTreeSet<_> = canonical
            .iter()
            .map(|opportunity| opportunity.structural_cycle_key.clone())
            .collect();
        let mut results = Vec::new();
        for key in self.legacy_keys.union(&canonical_keys) {
            let legacy = self.legacy_keys.contains(key);
            let canonical_opportunity = canonical
                .iter()
                .find(|opportunity| opportunity.structural_cycle_key == *key);
            results.push(DualRunComparison {
                anchor_block,
                opportunity_id: canonical_opportunity.map(|opportunity| opportunity.opportunity_id),
                kind: match (legacy, canonical_opportunity.is_some()) {
                    (true, true) => DivergenceKind::Both,
                    (true, false) => DivergenceKind::LegacyOnly,
                    (false, true) => DivergenceKind::CanonicalOnly,
                    (false, false) => unreachable!("union element is present on one side"),
                },
            });
        }
        self.legacy_keys.clear();
        results
    }

    /// Best-effort diagnostics only. A write failure has no decision effect.
    pub fn append_jsonl(path: &Path, comparisons: &[DualRunComparison]) -> std::io::Result<()> {
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        for comparison in comparisons {
            writeln!(file, "{}", serde_json::to_string(comparison)?)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparison_is_observation_only_and_clears_legacy_window() {
        let mut comparator = DualRunComparator::default();
        comparator.record_legacy("legacy-only");
        let comparisons = comparator.compare_and_clear(100, &[]);
        assert_eq!(comparisons.len(), 1);
        assert_eq!(comparisons[0].kind, DivergenceKind::LegacyOnly);
        assert!(comparator.compare_and_clear(101, &[]).is_empty());
    }
}
