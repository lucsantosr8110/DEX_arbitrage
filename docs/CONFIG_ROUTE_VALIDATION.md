# Configuração de rota executável

## Regra

TOML só expõe controle com consumidor real. Chave desconhecida gera `WARN` no
boot e em reload. Os três perfis versionados são testados para conter zero
chaves ignoradas.

## `arbitrage.route_validation`

```toml
[arbitrage.route_validation]
enabled = true
max_hops = 3
min_liquidity_per_hop = "50.0"
reject_high_slippage_routes = true
max_cumulative_slippage = 1.2
block_same_dex_consecutive = false
```

- `max_hops`: teto efetivo junto a `arbitrage.max_path_length`.
- `min_liquidity_per_hop`: entra no gate on-chain. Threshold final é máximo
  entre global, DEX e rota.
- `max_cumulative_slippage`: limite da rota inteira, em percentual. O executor
  calcula pior caso a partir de `max_slippage_bps` e
  `hop_slippage_increase_bps`; rejeita antes de simular/enviar se passar teto.
- `block_same_dex_consecutive`: rejeita duas pernas seguidas no mesmo DEX.

Rotas de flashloan sempre precisam fechar ciclo no token inicial. Não há flag
para desligar essa invariável de contrato.

## Premium

`flashloan.max_premium_bps` é teto de sanidade para `flashloan.fee_pct`.
Execução retorna `premium_cap` sem simular ou enviar quando premium configurado
ultrapassa teto.

## Verificação

```bash
cargo test -q config_parser_tests
cargo test -q route_validation_enforces_hop_and_slippage_caps
```
