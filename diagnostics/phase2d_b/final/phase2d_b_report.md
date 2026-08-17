# Fase 2D-B — campanha pinada

Veredito: **PASS**. Oito scans (3 base, 5 líquida), 16 snapshots, quotes fixadas em âncoras independentes.

Head avançou: base `[11, 11, 11]`, líquida `[46, 46, 48, 46, 47]`; spans de estado quote: zero em 8/8. Reorgs, chamadas não pinadas e mismatches: zero.

Chamadas pinadas: 3060 tentadas, 1664 sucesso, 1396 falhas; accounting válido. Todas as quatro venues foram inicializadas e suportaram pinning.

BF offline: 62 ciclos negativos exatos, 16 regiões, 16 witnesses primários, 46 variantes; misses, reconstruções, violações contratuais e divergências não classificadas: zero. Duas execuções determinísticas, RPC=false.

Persistência estrutural: 11 chaves (11 pós-reciprocidade); sobrevivência base 1.000, líquida 1.000. Persistência não prova executabilidade.

Limitações: sem execução sequencial, latência, gas integral, slippage de tamanho, transições de estado, concorrência/revert e atomicidade. Fase 2C permanece temporalmente não confiável; comparação limitada a contagens/distribuições.

Próxima fase: `PHASE_2D_C_ANCHOR_REQUOTE_AND_SEQUENTIAL_STATE_SIMULATION`; continuar read-only, sem transação/broadcast.
