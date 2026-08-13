# Console do Operador em Docker

Esta stack sobe o bot Rust, o console React, a API de operação e a observabilidade
em containers separados. A API do operador é somente leitura e o compose inicia o
bot com `config/config.dryrun.toml` e uma chave descartável fixa para evitar envio
acidental de transações.

## Portas publicadas

| Serviço | Porta local | Uso |
| --- | ---: | --- |
| Console | `5174` | Interface web |
| API do operador | `8081` | Saúde, snapshot e eventos SSE |
| Métricas do bot | `9102` | Endpoint Prometheus |
| Prometheus | `9091` | Consultas e targets |
| Grafana | `3001` | Dashboards |

As portas foram escolhidas para coexistir com os bots já ativos neste host
(incluindo serviços que ocupam `5173`, `8080`, `9090` e `3000`). Dentro da rede
Docker, os serviços continuam usando as portas internas padrão.

## Subir e verificar

```bash
docker-compose build
docker-compose up -d
docker-compose ps
```

URLs principais:

```text
http://127.0.0.1:5174/
http://127.0.0.1:8081/api/v1/health
http://127.0.0.1:8081/api/v1/snapshot
http://127.0.0.1:9102/metrics
http://127.0.0.1:9091/-/ready
http://127.0.0.1:3001/api/health
```

O Prometheus coleta as métricas pela rede interna em
`http://flashloan-bot:9100/metrics`. O nginx do console encaminha `/api/` para
`flashloan-bot:8080`.

## Ambiente e segurança

- O arquivo `.env` e suas variantes são ignorados pelo Git; não commite credenciais.
- O compose lê `.env`, mas sobrescreve `CONFIG_FILE` para o perfil dry-run e
  `PRIVATE_KEY` para uma chave sem valor operacional.
- Para usar endpoints RPC locais, mantenha-os apenas no `.env` ou em um arquivo
  de ambiente fora do controle de versão.
- Antes de qualquer execução real, revise explicitamente a configuração de rede,
  executor, chave e controles de risco; esta stack foi preparada para validação
  local sem transações.

## Operação

```bash
# Logs dos serviços
docker-compose logs -f flashloan-bot
docker-compose logs -f operator-console

# Parar sem remover dados persistidos
docker-compose down

# Recriar após alterações de código
docker-compose build --no-cache
docker-compose up -d
```

Se o Docker usar um contexto/configuração de usuário quebrado neste host, a
execução validada pode ser repetida com uma configuração temporária:

```bash
DOCKER_CONFIG=/tmp/docker-config docker-compose build
DOCKER_CONFIG=/tmp/docker-config docker-compose up -d
```

Não use `docker-compose down -v` em uma máquina compartilhada: isso remove os
volumes nomeados de Prometheus e Grafana.

## API do operador

- `GET /api/v1/health`: estado do processo e sequência do snapshot.
- `GET /api/v1/snapshot`: estado agregado para o console.
- `GET /api/v1/events`: stream SSE para atualizações do console.

A API não expõe operações de execução, saque ou alteração de configuração.
