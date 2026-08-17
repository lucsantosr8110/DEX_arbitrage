# Phase 2D-A anchor contract

`AnchorBlock` is selected once per scan from `head - confirmation_lag`.
Default lag is `2`; underflow, missing block, missing hash, and number mismatch
fail closed.

Every scanner quote uses `pinned_eth_call`, which applies:

```text
BlockId::Number(BlockNumber::Number(anchor.number.into()))
```

There is no retry using `latest`. Quote provenance carries block number, block
hash, and `pinned=true`. Before an edge is accepted, provenance must match the
single scan anchor. Before snapshot publication, the anchor hash is fetched a
second time; a changed or missing hash discards the scan.

Snapshot fields:

```text
temporal_mode=PINNED_ANCHOR_BLOCK
anchor_block={number,hash,selected_from_head,confirmation_lag}
head_at_scan_start
head_at_scan_end
head_advance_during_scan
anchor_hash_verified_before=true
anchor_hash_verified_after=true
reorg_detected=false
quote_state_block_span=0
```

Safety contract: scanner uses only `eth_chainId`, `eth_blockNumber`,
`eth_getBlockByNumber`, and read-only `eth_call`. No signer, private key,
broadcaster, Jito client, or transaction method is initialized.
