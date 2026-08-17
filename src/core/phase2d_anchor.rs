//! Phase 2D pinned-anchor contract.
//! Pure validation lives here so it can be exercised without RPC access.

use anyhow::{anyhow, Result};
use ethers::types::{Block, H256};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorBlock {
    pub number: u64,
    pub hash: H256,
    pub selected_from_head: u64,
    pub confirmation_lag: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanBlockContext {
    pub anchor: AnchorBlock,
    pub strict_pinning: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteBlockProvenance {
    pub requested_block_number: u64,
    pub requested_block_hash: H256,
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PinnedCallFailureKind {
    Timeout,
    RpcError,
    HistoricalStateUnavailable,
    BlockNotFound,
    Revert,
    DecodeError,
    PinningUnsupported,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedCallFailure {
    pub requested_block_number: u64,
    pub requested_block_hash: H256,
    pub classification: PinnedCallFailureKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorSnapshotMetadata {
    pub temporal_mode: String,
    pub anchor_block: AnchorBlock,
    pub head_at_scan_start: u64,
    pub head_at_scan_end: u64,
    pub head_advance_during_scan: u64,
    pub anchor_hash_verified_before: bool,
    pub anchor_hash_verified_after: bool,
    pub reorg_detected: bool,
    pub quote_state_block_span: u64,
}

pub fn select_anchor(
    head: u64,
    confirmation_lag: u64,
    block: Option<Block<H256>>,
) -> Result<AnchorBlock> {
    let number = head
        .checked_sub(confirmation_lag)
        .ok_or_else(|| anyhow!("ANCHOR_HEAD_UNDERFLOW head={head} lag={confirmation_lag}"))?;
    let block = block.ok_or_else(|| anyhow!("ANCHOR_BLOCK_NOT_FOUND number={number}"))?;
    let hash = block
        .hash
        .ok_or_else(|| anyhow!("ANCHOR_BLOCK_HASH_MISSING number={number}"))?;
    if block.number.map(|value| value.as_u64()) != Some(number) {
        return Err(anyhow!("ANCHOR_BLOCK_NUMBER_MISMATCH expected={number}"));
    }
    Ok(AnchorBlock {
        number,
        hash,
        selected_from_head: head,
        confirmation_lag,
    })
}

pub fn provenance(anchor: &AnchorBlock) -> QuoteBlockProvenance {
    QuoteBlockProvenance {
        requested_block_number: anchor.number,
        requested_block_hash: anchor.hash,
        pinned: true,
    }
}

pub fn validate_quote_anchor(quote: &QuoteBlockProvenance, anchor: &AnchorBlock) -> Result<()> {
    if !quote.pinned {
        return Err(anyhow!("QUOTE_UNPINNED"));
    }
    if quote.requested_block_number != anchor.number {
        return Err(anyhow!(
            "QUOTE_ANCHOR_BLOCK_MISMATCH expected={} actual={}",
            anchor.number,
            quote.requested_block_number
        ));
    }
    if quote.requested_block_hash != anchor.hash {
        return Err(anyhow!("QUOTE_ANCHOR_HASH_MISMATCH"));
    }
    Ok(())
}

pub fn validate_edge_anchors<'a, I>(edges: I, anchor: &AnchorBlock) -> Result<()>
where
    I: IntoIterator<Item = (&'a Option<u64>, &'a Option<H256>, &'a bool)>,
{
    for (number, hash, pinned) in edges {
        if !*pinned || *number != Some(anchor.number) || *hash != Some(anchor.hash) {
            return Err(anyhow!("GRAPH_ANCHOR_INVALID"));
        }
    }
    Ok(())
}

pub fn reorg_detected(anchor: &AnchorBlock, observed: Option<Block<H256>>) -> bool {
    observed.and_then(|block| block.hash) != Some(anchor.hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::Address;

    fn block(number: u64, hash: H256) -> Block<H256> {
        Block {
            number: Some(number.into()),
            hash: Some(hash),
            author: Some(Address::zero()),
            ..Default::default()
        }
    }

    fn anchor() -> AnchorBlock {
        select_anchor(1000, 2, Some(block(998, H256::repeat_byte(7)))).unwrap()
    }

    #[test]
    fn selects_lag_two_anchor() {
        assert_eq!(anchor().number, 998);
    }
    #[test]
    fn preserves_head_and_lag() {
        let a = anchor();
        assert_eq!((a.selected_from_head, a.confirmation_lag), (1000, 2));
    }
    #[test]
    fn rejects_underflow() {
        assert!(select_anchor(1, 2, None).is_err());
    }
    #[test]
    fn rejects_missing_block() {
        assert!(select_anchor(1000, 2, None).is_err());
    }
    #[test]
    fn rejects_missing_hash() {
        assert!(select_anchor(
            1000,
            2,
            Some(Block {
                number: Some(998.into()),
                ..Default::default()
            })
        )
        .is_err());
    }
    #[test]
    fn rejects_number_mismatch() {
        assert!(select_anchor(1000, 2, Some(block(997, H256::repeat_byte(7)))).is_err());
    }
    #[test]
    fn provenance_is_pinned() {
        assert!(provenance(&anchor()).pinned);
    }
    #[test]
    fn provenance_number_matches() {
        let a = anchor();
        assert_eq!(provenance(&a).requested_block_number, 998);
    }
    #[test]
    fn provenance_hash_matches() {
        let a = anchor();
        assert_eq!(provenance(&a).requested_block_hash, a.hash);
    }
    #[test]
    fn unpinned_quote_rejected() {
        let mut p = provenance(&anchor());
        p.pinned = false;
        assert!(validate_quote_anchor(&p, &anchor()).is_err());
    }
    #[test]
    fn divergent_number_rejected() {
        let mut p = provenance(&anchor());
        p.requested_block_number += 1;
        assert!(validate_quote_anchor(&p, &anchor()).is_err());
    }
    #[test]
    fn divergent_hash_rejected() {
        let mut p = provenance(&anchor());
        p.requested_block_hash = H256::zero();
        assert!(validate_quote_anchor(&p, &anchor()).is_err());
    }
    #[test]
    fn matching_quote_accepted() {
        let a = anchor();
        assert!(validate_quote_anchor(&provenance(&a), &a).is_ok());
    }
    #[test]
    fn empty_graph_anchor_is_valid() {
        assert!(validate_edge_anchors(std::iter::empty(), &anchor()).is_ok());
    }
    #[test]
    fn graph_anchor_accepts_matching_edge() {
        let a = anchor();
        let n = Some(a.number);
        let h = Some(a.hash);
        let p = true;
        assert!(validate_edge_anchors([(&n, &h, &p)], &a).is_ok());
    }
    #[test]
    fn graph_anchor_rejects_unpinned_edge() {
        let a = anchor();
        let n = Some(a.number);
        let h = Some(a.hash);
        let p = false;
        assert!(validate_edge_anchors([(&n, &h, &p)], &a).is_err());
    }
    #[test]
    fn graph_anchor_rejects_missing_number() {
        let a = anchor();
        let n = None;
        let h = Some(a.hash);
        let p = true;
        assert!(validate_edge_anchors([(&n, &h, &p)], &a).is_err());
    }
    #[test]
    fn graph_anchor_rejects_missing_hash() {
        let a = anchor();
        let n = Some(a.number);
        let h = None;
        let p = true;
        assert!(validate_edge_anchors([(&n, &h, &p)], &a).is_err());
    }
    #[test]
    fn reorg_false_for_same_hash() {
        let a = anchor();
        assert!(!reorg_detected(&a, Some(block(998, a.hash))));
    }
    #[test]
    fn reorg_true_for_changed_hash() {
        assert!(reorg_detected(
            &anchor(),
            Some(block(998, H256::repeat_byte(8)))
        ));
    }
    #[test]
    fn reorg_true_for_missing_block() {
        assert!(reorg_detected(&anchor(), None));
    }
    #[test]
    fn metadata_has_zero_quote_span() {
        let a = anchor();
        let m = AnchorSnapshotMetadata {
            temporal_mode: "PINNED_ANCHOR_BLOCK".into(),
            anchor_block: a,
            head_at_scan_start: 1000,
            head_at_scan_end: 1045,
            head_advance_during_scan: 45,
            anchor_hash_verified_before: true,
            anchor_hash_verified_after: true,
            reorg_detected: false,
            quote_state_block_span: 0,
        };
        assert_eq!(m.quote_state_block_span, 0);
    }
    #[test]
    fn metadata_records_head_advance() {
        let a = anchor();
        let m = AnchorSnapshotMetadata {
            temporal_mode: "PINNED_ANCHOR_BLOCK".into(),
            anchor_block: a,
            head_at_scan_start: 1000,
            head_at_scan_end: 1045,
            head_advance_during_scan: 45,
            anchor_hash_verified_before: true,
            anchor_hash_verified_after: true,
            reorg_detected: false,
            quote_state_block_span: 0,
        };
        assert_eq!(m.head_advance_during_scan, 45);
    }
    #[test]
    fn strict_context_contains_anchor() {
        let a = anchor();
        let c = ScanBlockContext {
            anchor: a.clone(),
            strict_pinning: true,
        };
        assert_eq!(c.anchor, a);
    }
    #[test]
    fn failure_keeps_anchor_number() {
        let a = anchor();
        let f = PinnedCallFailure {
            requested_block_number: a.number,
            requested_block_hash: a.hash,
            classification: PinnedCallFailureKind::Timeout,
        };
        assert_eq!(f.requested_block_number, 998);
    }
    #[test]
    fn failure_kind_is_serializable() {
        let f = PinnedCallFailureKind::PinningUnsupported;
        assert_eq!(serde_json::to_string(&f).unwrap(), "\"PinningUnsupported\"");
    }
}
