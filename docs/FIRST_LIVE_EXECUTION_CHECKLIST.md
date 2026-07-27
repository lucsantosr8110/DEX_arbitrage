# Checklist — primeira execução real (capital reduzido)

Contexto: `EXECUTOR_ADDRESS` aponta pro contrato novo
(`0x384b0c63A54F50D754f76A436Ce0302e82FF9068`, deployado em 2026-07-27, block
`90949362`, owner = deployer com fundos reais). `CONFIG_FILE` atual
(`config/config.dryrun.toml`) tem `[execution].dry_run = true` — esse é o
gate real que hoje impede o bot de mandar qualquer transação, apesar da
`PRIVATE_KEY` já ser a de produção. Nada abaixo assume que esse checklist
já foi seguido antes.

## 1. Antes de tocar em `dry_run`

- [ ] Rodar `node scripts/sanity_check_readonly.cjs` — 0 approvals pra
      spender desconhecido, 0 saldo parado no executor, `owner()` correto.
- [ ] Rodar `HARDHAT_FORK_POLYGON=1 npx hardhat test test/polygon_fork_v3.cjs`
      — 7/7 passando contra o bloco mais recente (não só o que já rodou).
- [ ] Confirmar `RPC_POLYGON_URL` funcional (`eth_chainId == 0x89`) e que
      `BOT_RPC_ENDPOINTS` tem pelo menos um provedor sem rate-limit no
      momento (Alchemy no limite mensal, QuickNode no limite diário —
      conferir status antes de ir ao vivo, não assumir que continua igual).
- [ ] Conferir saldo do deployer/owner é suficiente pro gas de N execuções
      de teste + margem (`eth_getBalance`, comparar com `gasPrice` atual).

## 2. Reduzir o capital exposto

- [ ] `[flashloan].capital_usd` em `config/config.dryrun.toml`: baixar de
      `100.0` pra um valor de teste (ex.: `5`–`20` USD) antes da primeira
      execução real. Não subir de novo até ter pelo menos uma execução
      real bem-sucedida e auditada.
- [ ] `[flashloan].min_profit_usd` e `[arbitrage].min_profit_threshold_usd`:
      hoje `0.0` — considerar um piso pequeno positivo (ex.: `0.05`–`0.10`
      USD) pra primeira rodada, pra não deixar passar trade cujo lucro
      nominal seja menor que a variância do próprio gas.
- [ ] Confirmar `[flashloan].simulate_before_execute = true` continua ativo
      (já está) — é a simulação local antes do broadcast real.
- [ ] Confirmar `[wallet].max_tx_value` e `max_gas_price_gwei` em
      `config.dryrun.toml` ainda fazem sentido pro capital reduzido (gas
      atual observado ~275–310 gwei — perto do teto configurado de 500).

## 3. Alertas e observabilidade

- [ ] `TELEGRAM_TOKEN` / `TELEGRAM_CHAT_ID` no `.env` estão **vazios** —
      sem isso não há alerta fora do log. Configurar antes de ir ao vivo,
      ou aceitar explicitamente que o monitoramento será manual (log
      tail) durante a janela de teste.
- [ ] Prometheus/metrics (`[prometheus].enabled`) — confirmar que está
      habilitado e acessível durante a janela de execução.
- [ ] Definir quem observa a janela de execução em tempo real (não deixar
      rodar sem supervisão na primeira vez).

## 4. Kill switch conhecido de antemão

- [ ] Confirmar que a wallet owner consegue chamar `emergencyStop()`
      (pausa) e `resume()` no `FlashloanExecutor` — testar a leitura de
      `paused` antes (não precisa chamar, só saber o caminho).
- [ ] Confirmar `updateExecutor(address, bool)` pra revogar um relayer
      comprometido, e `withdrawToken` / `withdrawMATIC` como rota de
      resgate caso fundos fiquem presos no contrato.
- [ ] Ter o comando de parada do processo do bot (systemd/pm2/docker,
      conforme o ambiente) já identificado e testado antes de começar.

## 5. Ligar o modo real

- [ ] Mudar `[execution].dry_run` de `true` pra `false` em
      `config/config.dryrun.toml` (ou apontar `CONFIG_FILE` pra um perfil
      dedicado de "primeira execução real" — recomendado, pra não deixar
      o arquivo chamado "dryrun" ser o que efetivamente manda transação
      real; renomear/clonar evita confusão futura).
- [ ] Rodar o bot só pra 1 ciclo/1 oportunidade primeiro, se houver flag
      pra isso, em vez de deixar rodando solto.
- [ ] Ter `scripts/sanity_check_readonly.cjs` pronto pra rodar de novo
      logo depois da primeira execução (ver seção 6).

## 6. Logo depois da primeira execução real

- [ ] Rodar `node scripts/sanity_check_readonly.cjs` de novo — confirmar
      saldo do executor voltou a zero (nenhum fundo ficou preso) e nenhum
      approval novo foi pra spender fora dos 3 routers conhecidos.
- [ ] Conferir on-chain (Polygonscan) o resultado real da tx: lucro
      líquido recebido pelo `owner`, gas gasto, se bateu o `minProfit`.
- [ ] Registrar o resultado (hash, lucro, gas, custo) em
      `logs/deployments.log` ou log equivalente de execução — não só
      deploy, também primeira operação real.
- [ ] Decidir com base em dado real (não intuição) se sobe `capital_usd`
      gradualmente ou mantém reduzido por mais um tempo.

## Não fazer nesta etapa

- Não subir capital de uma vez pro valor de produção alvo.
- Não desabilitar `simulate_before_execute` pra "ganhar velocidade".
- Não rodar sem alguém observando a janela inteira da primeira execução.
- Não deixar `min_profit` em `0.0` pra produção de fato — só é aceitável
  como valor de teste controlado, sob supervisão.
