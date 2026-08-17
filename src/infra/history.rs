//! ============================================================
//! src/infra/history.rs — Persistência SQLite do histórico de rodadas
//! ============================================================
//!
//! O console do operador mostrava só o último snapshot em memória
//! (`TuiState`); estatísticas de lucro e melhor rota se perdiam no
//! restart. Este módulo persiste um resumo por rodada (funnel canônico +
//! melhor rota + net USD agregado) em SQLite embutido.
//!
//! - `rusqlite` com `bundled` (sqlite compilado, sem dependência de sistema).
//! - Uma escrita por rodada (~5-15min), bloqueio trivial; `Mutex<Connection>`.
//! - WAL + synchronous=NORMAL: leituras concorrentes não bloqueiam escrita.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

/// Resumo de UMA rodada do funil canônico. Campos `Option` = sem medição
/// (ex.: perfil sem re-quote). Escrito ao completar cada round.
#[derive(Debug, Clone)]
pub struct RoundRecord {
    pub sequence: u64,
    /// RFC3339 UTC de quando a rodada completou.
    pub completed_at: String,
    pub duration_ms: Option<u64>,
    /// Tempo da descoberta canônica (quotes + ciclos) nesta rodada.
    pub discovery_ms: Option<u64>,
    /// Tempo do shadow round (re-quote + simulação) nesta rodada.
    pub shadow_ms: Option<u64>,
    pub quotes: u64,
    pub edges: Option<u64>,
    pub cycles_detected: u64,
    pub routes_ranked: u64,
    pub routes_evaluated: Option<u64>,
    pub gross_positive: u64,
    pub economically_positive: u64,
    pub stable: Option<u64>,
    pub risk_approved: Option<u64>,
    pub selected: Option<u64>,
    /// Soma USD dos ciclos com net projetado > 0 nesta rodada.
    pub net_usd_total: f64,
    pub anchor_block: Option<u64>,
    // Melhor rota da rodada (top ranked por gross).
    pub best_route_kind: Option<String>,
    pub best_route_path: Option<String>,
    pub best_route_venues: Option<String>,
    pub best_route_gross: Option<f64>,
    pub best_route_net: Option<f64>,
    /// TUI spread (forward dispersion) — mesma métrica que `best_route_gross`
    /// legado; mantido separado para não quebrar consumidores que já importam
    /// o nome antigo.
    pub tui_spread_pct: Option<f64>,
    /// Cycle rate real (economics) em %. Diferente de `tui_spread_pct` quando
    /// route é multi-leg e reverse leg piora o rate composto.
    pub cycle_rate_pct: Option<f64>,
    /// Net projetado em USD da rota com cycle rate real.
    pub cycle_net_usd: Option<f64>,
    /// Stage latency breakdown (ver `core::canonical_discovery::CanonicalRoundTiming`).
    /// `None` quando a rodada não chegou a rodar discovery real (ex.: shutdown
    /// no meio do ciclo) — nunca preenchido com 0 nesse caso.
    pub anchor_resolution_ms: Option<u64>,
    pub metadata_ms: Option<u64>,
    pub quote_ms: Option<u64>,
    pub ranking_ms: Option<u64>,
    pub requote_ms: Option<u64>,
    pub context_build_ms: Option<u64>,
    pub materialization_economics_ms: Option<u64>,
    pub unattributed_ms: Option<u64>,
    /// Economic waterfall (USD) da melhor rota (ver `tui::TopSpreadRow`).
    /// `gross_pnl_usd` já é pós-fee do AMM; `gas_cost_usd`/`flashloan_cost_usd`
    /// são os MESMOS componentes já deduzidos uma única vez dentro de
    /// `best_route_net`/`cycle_net_usd` — expostos aqui só para visibilidade,
    /// não para dedução adicional. `None` = não computado nesta rodada
    /// (nunca um 0 fabricado).
    pub best_route_gross_pnl_usd: Option<f64>,
    pub best_route_gas_cost_usd: Option<f64>,
    pub best_route_flashloan_cost_usd: Option<f64>,
    /// Ver `tui::NegativeCause` — "POSITIVE" | "NO_GROSS_SPREAD" |
    /// "GAS_DOMINATES" | "FLASHLOAN_FEE_DOMINATES" | "NET_NON_POSITIVE_OTHER".
    pub best_route_negative_cause: Option<String>,
}

/// Linha lida do DB para o console (serializável p/ /api/v1/rounds).
#[derive(Debug, Clone, Serialize)]
pub struct RoundRow {
    pub sequence: u64,
    pub completed_at: String,
    pub duration_ms: Option<u64>,
    pub discovery_ms: Option<u64>,
    pub shadow_ms: Option<u64>,
    pub quotes: u64,
    pub edges: Option<u64>,
    pub cycles_detected: u64,
    pub routes_ranked: u64,
    pub routes_evaluated: Option<u64>,
    pub gross_positive: u64,
    pub economically_positive: u64,
    pub stable: Option<u64>,
    pub risk_approved: Option<u64>,
    pub selected: Option<u64>,
    pub net_usd_total: f64,
    pub anchor_block: Option<u64>,
    pub best_route_kind: Option<String>,
    pub best_route_path: Option<String>,
    pub best_route_venues: Option<String>,
    pub best_route_gross: Option<f64>,
    pub best_route_net: Option<f64>,
    /// TUI spread (forward dispersion) — mesma métrica que `best_route_gross`
    /// legado; mantido separado para não quebrar consumidores que já importam
    /// o nome antigo.
    pub tui_spread_pct: Option<f64>,
    /// Cycle rate real (economics) em %. Diferente de `tui_spread_pct` quando
    /// route é multi-leg e reverse leg piora o rate composto.
    pub cycle_rate_pct: Option<f64>,
    /// Net projetado em USD da rota com cycle rate real.
    pub cycle_net_usd: Option<f64>,
    pub anchor_resolution_ms: Option<u64>,
    pub metadata_ms: Option<u64>,
    pub quote_ms: Option<u64>,
    pub ranking_ms: Option<u64>,
    pub requote_ms: Option<u64>,
    pub context_build_ms: Option<u64>,
    pub materialization_economics_ms: Option<u64>,
    pub unattributed_ms: Option<u64>,
    pub best_route_gross_pnl_usd: Option<f64>,
    pub best_route_gas_cost_usd: Option<f64>,
    pub best_route_flashloan_cost_usd: Option<f64>,
    pub best_route_negative_cause: Option<String>,
}

impl RoundRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(RoundRow {
            sequence: row.get("sequence")?,
            completed_at: row.get("completed_at")?,
            duration_ms: row.get("duration_ms")?,
            discovery_ms: row.get("discovery_ms")?,
            shadow_ms: row.get("shadow_ms")?,
            quotes: row.get("quotes")?,
            edges: row.get("edges")?,
            cycles_detected: row.get("cycles_detected")?,
            routes_ranked: row.get("routes_ranked")?,
            routes_evaluated: row.get("routes_evaluated")?,
            gross_positive: row.get("gross_positive")?,
            economically_positive: row.get("economically_positive")?,
            stable: row.get("stable")?,
            risk_approved: row.get("risk_approved")?,
            selected: row.get("selected")?,
            net_usd_total: row.get("net_usd_total")?,
            anchor_block: row.get("anchor_block")?,
            best_route_kind: row.get("best_route_kind")?,
            best_route_path: row.get("best_route_path")?,
            best_route_venues: row.get("best_route_venues")?,
            best_route_gross: row.get("best_route_gross")?,
            best_route_net: row.get("best_route_net")?,
            tui_spread_pct: row.get("tui_spread_pct")?,
            cycle_rate_pct: row.get("cycle_rate_pct")?,
            cycle_net_usd: row.get("cycle_net_usd")?,
            anchor_resolution_ms: row.get("anchor_resolution_ms")?,
            metadata_ms: row.get("metadata_ms")?,
            quote_ms: row.get("quote_ms")?,
            ranking_ms: row.get("ranking_ms")?,
            requote_ms: row.get("requote_ms")?,
            context_build_ms: row.get("context_build_ms")?,
            materialization_economics_ms: row.get("materialization_economics_ms")?,
            unattributed_ms: row.get("unattributed_ms")?,
            best_route_gross_pnl_usd: row.get("best_route_gross_pnl_usd")?,
            best_route_gas_cost_usd: row.get("best_route_gas_cost_usd")?,
            best_route_flashloan_cost_usd: row.get("best_route_flashloan_cost_usd")?,
            best_route_negative_cause: row.get("best_route_negative_cause")?,
        })
    }
}

/// Agregados cumulativos p/ o painel "Melhor net" (não só snapshot atual).
#[derive(Debug, Clone, Default, Serialize)]
pub struct OverallStats {
    pub total_rounds: u64,
    pub sum_net_usd: f64,
    pub sum_gross_positive: u64,
    /// Melhor net de rota única já observado (qualquer rodada).
    pub best_net_usd: Option<f64>,
    pub best_net_sequence: Option<u64>,
    pub best_net_completed_at: Option<String>,
    pub best_net_route_path: Option<String>,
    pub best_net_route_venues: Option<String>,
    pub best_net_route_gross: Option<f64>,
    pub best_net_route_net: Option<f64>,
    /// `cycle_rate_pct` da rota (economics real; para rotas triangulares
    /// canônicas coincide com `best_net_route_gross` — mesma fonte, mesmo
    /// `cycle_rate` — mas para o path de 2-leg diagnóstico pode divergir).
    /// Faltava no struct: o frontend já pedia esse campo por nome
    /// (`best_net_route_cycle_rate_pct`) mas ele nunca existiu aqui, então
    /// sempre chegava `undefined` e a UI mostrava "0.00%" mesmo com edge
    /// real (achado 2026-08-17, spec ARGUS economic-proof).
    pub best_net_route_cycle_rate_pct: Option<f64>,
    /// Economic waterfall (ver `tui::TopSpreadRow`/`tui::NegativeCause`).
    pub best_net_route_gross_pnl_usd: Option<f64>,
    pub best_net_route_gas_cost_usd: Option<f64>,
    pub best_net_route_flashloan_cost_usd: Option<f64>,
    pub best_net_route_negative_cause: Option<String>,
    /// Percentis de latência (duração do round + estágios) nas últimas rodadas.
    pub latency: LatencyStats,
}

/// Percentis p50/p95 de latência por estágio, calculados do histórico.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LatencyStats {
    pub sample_count: u64,
    pub duration_p50_ms: Option<u64>,
    pub duration_p95_ms: Option<u64>,
    pub discovery_p50_ms: Option<u64>,
    pub discovery_p95_ms: Option<u64>,
    pub shadow_p50_ms: Option<u64>,
    pub shadow_p95_ms: Option<u64>,
    /// ARGUS stage waterfall percentis (ver `CanonicalRoundTiming`).
    pub anchor_resolution_p50_ms: Option<u64>,
    pub anchor_resolution_p95_ms: Option<u64>,
    pub metadata_p50_ms: Option<u64>,
    pub metadata_p95_ms: Option<u64>,
    pub quote_p50_ms: Option<u64>,
    pub quote_p95_ms: Option<u64>,
    pub ranking_p50_ms: Option<u64>,
    pub ranking_p95_ms: Option<u64>,
    pub requote_p50_ms: Option<u64>,
    pub requote_p95_ms: Option<u64>,
    pub context_build_p50_ms: Option<u64>,
    pub context_build_p95_ms: Option<u64>,
    pub materialization_economics_p50_ms: Option<u64>,
    pub materialization_economics_p95_ms: Option<u64>,
    pub unattributed_p50_ms: Option<u64>,
    pub unattributed_p95_ms: Option<u64>,
}

/// Handle compartilhado do histórico. `Arc` para passar entre threads
/// (main loop, API axum); `Mutex` serializa acesso ao `Connection`.
pub struct RoundHistory {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS rounds (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    sequence              INTEGER NOT NULL,
    completed_at          TEXT    NOT NULL,
    duration_ms           INTEGER,
    discovery_ms          INTEGER,
    shadow_ms             INTEGER,
    quotes                INTEGER NOT NULL,
    edges                 INTEGER,
    cycles_detected       INTEGER NOT NULL,
    routes_ranked         INTEGER NOT NULL,
    routes_evaluated      INTEGER,
    gross_positive        INTEGER NOT NULL,
    economically_positive INTEGER NOT NULL,
    stable                INTEGER,
    risk_approved         INTEGER,
    selected              INTEGER,
    net_usd_total         REAL    NOT NULL DEFAULT 0,
    anchor_block          INTEGER,
    best_route_kind       TEXT,
    best_route_path       TEXT,
    best_route_venues     TEXT,
    best_route_gross      REAL,
    best_route_net        REAL,
    -- v1.1: distinguir spread forward (TUI) de cycle rate real (economics).
    -- `tui_spread_pct` = (max-min)/min*100 entre venues; `cycle_rate_pct`
    -- = (cycle_rate-1)*100 = gross cycle real; `cycle_net_usd` = net já com
    -- custos deduzidos em USD. Migração idempotente via ALTER TABLE.
    tui_spread_pct        REAL,
    cycle_rate_pct        REAL,
    cycle_net_usd         REAL,
    -- ARGUS stage latency waterfall (ver core::canonical_discovery::
    -- CanonicalRoundTiming). Migração idempotente via ALTER TABLE.
    anchor_resolution_ms          INTEGER,
    metadata_ms                   INTEGER,
    quote_ms                      INTEGER,
    ranking_ms                    INTEGER,
    requote_ms                    INTEGER,
    context_build_ms              INTEGER,
    materialization_economics_ms  INTEGER,
    unattributed_ms               INTEGER,
    -- ARGUS economic waterfall (ver tui::TopSpreadRow /
    -- tui::classify_negative_cause). Migração idempotente via ALTER TABLE.
    best_route_gross_pnl_usd      REAL,
    best_route_gas_cost_usd       REAL,
    best_route_flashloan_cost_usd REAL,
    best_route_negative_cause     TEXT
);
CREATE INDEX IF NOT EXISTS idx_rounds_sequence ON rounds(sequence);
"#;

impl RoundHistory {
    /// Abre (ou cria) o DB no path dado, aplica schema e pragmas.
    pub fn open(path: impl AsRef<Path>) -> Result<Arc<Self>> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("criar dir do histórico: {}", parent.display()))?;
            }
        }
        let conn = Connection::open(path)
            .with_context(|| format!("abrir sqlite do histórico: {}", path.display()))?;
        conn.execute_batch(&format!(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             {SCHEMA}"
        ))
        .context("aplicar schema do histórico")?;
        // Migração: DBs criados antes das colunas de latência ganham as
        // colunas via ALTER (CREATE TABLE IF NOT EXISTS não adiciona coluna).
        let existing: Vec<String> = {
            let mut stmt = conn
                .prepare("PRAGMA table_info(rounds)")
                .context("ler schema do histórico")?;
            let mapped = stmt
                .query_map([], |row| row.get::<_, String>(1))
                .context("listar colunas do histórico")?;
            mapped.filter_map(Result::ok).collect()
        };
        for column in [
            "discovery_ms",
            "shadow_ms",
            "tui_spread_pct",
            "cycle_rate_pct",
            "cycle_net_usd",
            "anchor_resolution_ms",
            "metadata_ms",
            "quote_ms",
            "ranking_ms",
            "requote_ms",
            "context_build_ms",
            "materialization_economics_ms",
            "unattributed_ms",
            "best_route_gross_pnl_usd",
            "best_route_gas_cost_usd",
            "best_route_flashloan_cost_usd",
        ] {
            if !existing.iter().any(|name| name == column) {
                conn.execute_batch(&format!("ALTER TABLE rounds ADD COLUMN {column} INTEGER"))
                    .with_context(|| format!("migrar coluna {column} no histórico"))?;
            }
        }
        // TEXT separado: coluna nova de causa negativa (string, não número).
        if !existing
            .iter()
            .any(|name| name == "best_route_negative_cause")
        {
            conn.execute_batch("ALTER TABLE rounds ADD COLUMN best_route_negative_cause TEXT")
                .context("migrar coluna best_route_negative_cause no histórico")?;
        }
        Ok(Arc::new(RoundHistory {
            conn: Mutex::new(conn),
        }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Persiste um resumo de rodada. Chamada uma vez por round completado.
    pub fn insert_round(&self, record: &RoundRecord) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO rounds (
                sequence, completed_at, duration_ms, discovery_ms, shadow_ms, quotes, edges,
                cycles_detected, routes_ranked, routes_evaluated,
                gross_positive, economically_positive, stable,
                risk_approved, selected, net_usd_total, anchor_block,
                best_route_kind, best_route_path, best_route_venues,
                best_route_gross, best_route_net,
                tui_spread_pct, cycle_rate_pct, cycle_net_usd,
                anchor_resolution_ms, metadata_ms, quote_ms, ranking_ms, requote_ms,
                context_build_ms, materialization_economics_ms, unattributed_ms,
                best_route_gross_pnl_usd, best_route_gas_cost_usd,
                best_route_flashloan_cost_usd, best_route_negative_cause
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                ?8, ?9, ?10,
                ?11, ?12, ?13,
                ?14, ?15, ?16, ?17,
                ?18, ?19, ?20,
                ?21, ?22,
                ?23, ?24, ?25,
                ?26, ?27, ?28, ?29, ?30,
                ?31, ?32, ?33,
                ?34, ?35, ?36, ?37
            )",
            params![
                record.sequence as i64,
                record.completed_at,
                record.duration_ms.map(|v| v as i64),
                record.discovery_ms.map(|v| v as i64),
                record.shadow_ms.map(|v| v as i64),
                record.quotes as i64,
                record.edges.map(|v| v as i64),
                record.cycles_detected as i64,
                record.routes_ranked as i64,
                record.routes_evaluated.map(|v| v as i64),
                record.gross_positive as i64,
                record.economically_positive as i64,
                record.stable.map(|v| v as i64),
                record.risk_approved.map(|v| v as i64),
                record.selected.map(|v| v as i64),
                record.net_usd_total,
                record.anchor_block.map(|v| v as i64),
                record.best_route_kind,
                record.best_route_path,
                record.best_route_venues,
                record.best_route_gross,
                record.best_route_net,
                record.tui_spread_pct,
                record.cycle_rate_pct,
                record.cycle_net_usd,
                record.anchor_resolution_ms.map(|v| v as i64),
                record.metadata_ms.map(|v| v as i64),
                record.quote_ms.map(|v| v as i64),
                record.ranking_ms.map(|v| v as i64),
                record.requote_ms.map(|v| v as i64),
                record.context_build_ms.map(|v| v as i64),
                record.materialization_economics_ms.map(|v| v as i64),
                record.unattributed_ms.map(|v| v as i64),
                record.best_route_gross_pnl_usd,
                record.best_route_gas_cost_usd,
                record.best_route_flashloan_cost_usd,
                record.best_route_negative_cause,
            ],
        )
        .context("insert round no histórico")?;
        Ok(())
    }

    /// Últimos `limit` rounds, mais recente primeiro.
    pub fn recent_rounds(&self, limit: u64) -> Result<Vec<RoundRow>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT * FROM rounds
                 ORDER BY id DESC
                 LIMIT ?1",
            )
            .context("preparar recent_rounds")?;
        let rows = stmt
            .query_map(params![limit as i64], RoundRow::from_row)
            .context("query recent_rounds")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("ler recent_rounds")?;
        Ok(rows)
    }

    /// Agregados cumulativos + a rodada com melhor net de rota.
    pub fn overall_stats(&self) -> Result<OverallStats> {
        let conn = self.lock();
        let (total_rounds, sum_net_usd, sum_gross_positive): (i64, f64, i64) = conn
            .query_row(
                "SELECT COUNT(*),
                        COALESCE(SUM(net_usd_total), 0),
                        COALESCE(SUM(gross_positive), 0)
                 FROM rounds",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .context("query overall totals")?;

        // Rodada com melhor net de rota única; fallback p/ net agregado.
        let best: Option<RoundRow> = conn
            .query_row(
                "SELECT * FROM rounds
                 ORDER BY COALESCE(best_route_net, net_usd_total) DESC, id ASC
                 LIMIT 1",
                [],
                RoundRow::from_row,
            )
            .optional()
            .context("query melhor rodada")?;

        let best_net = best
            .as_ref()
            .and_then(|row| row.best_route_net.or(Some(row.net_usd_total)));
        let latency = Self::latency_stats_locked(&conn, 50)?;
        Ok(OverallStats {
            total_rounds: total_rounds as u64,
            sum_net_usd,
            sum_gross_positive: sum_gross_positive as u64,
            best_net_usd: best_net,
            best_net_sequence: best.as_ref().map(|row| row.sequence),
            best_net_completed_at: best.as_ref().map(|row| row.completed_at.clone()),
            best_net_route_path: best.as_ref().and_then(|row| row.best_route_path.clone()),
            best_net_route_venues: best.as_ref().and_then(|row| row.best_route_venues.clone()),
            best_net_route_gross: best.as_ref().and_then(|row| row.best_route_gross),
            best_net_route_net: best.as_ref().and_then(|row| row.best_route_net),
            best_net_route_cycle_rate_pct: best.as_ref().and_then(|row| row.cycle_rate_pct),
            best_net_route_gross_pnl_usd: best
                .as_ref()
                .and_then(|row| row.best_route_gross_pnl_usd),
            best_net_route_gas_cost_usd: best.as_ref().and_then(|row| row.best_route_gas_cost_usd),
            best_net_route_flashloan_cost_usd: best
                .as_ref()
                .and_then(|row| row.best_route_flashloan_cost_usd),
            best_net_route_negative_cause: best
                .as_ref()
                .and_then(|row| row.best_route_negative_cause.clone()),
            latency,
        })
    }

    /// Percentis p50/p95 de latência nas últimas `limit` rodadas.
    pub fn latency_stats(&self, limit: u64) -> Result<LatencyStats> {
        let conn = self.lock();
        Self::latency_stats_locked(&conn, limit)
    }

    fn latency_stats_locked(conn: &Connection, limit: u64) -> Result<LatencyStats> {
        let mut stmt = conn
            .prepare(
                "SELECT duration_ms, discovery_ms, shadow_ms,
                        anchor_resolution_ms, metadata_ms, quote_ms, ranking_ms,
                        requote_ms, context_build_ms, materialization_economics_ms,
                        unattributed_ms
                 FROM rounds
                 ORDER BY id DESC
                 LIMIT ?1",
            )
            .context("preparar latency_stats")?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                    row.get::<_, Option<i64>>(9)?,
                    row.get::<_, Option<i64>>(10)?,
                ))
            })
            .context("query latency_stats")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("ler latency_stats")?;

        let mut durations = Vec::new();
        let mut discoveries = Vec::new();
        let mut shadows = Vec::new();
        let mut anchor_res = Vec::new();
        let mut metadata = Vec::new();
        let mut quote = Vec::new();
        let mut ranking = Vec::new();
        let mut requote = Vec::new();
        let mut context_build = Vec::new();
        let mut materialization_economics = Vec::new();
        let mut unattributed = Vec::new();
        for (
            duration,
            discovery,
            shadow,
            anchor_resolution,
            metadata_v,
            quote_v,
            ranking_v,
            requote_v,
            context_build_v,
            materialization_economics_v,
            unattributed_v,
        ) in rows
        {
            let push = |bucket: &mut Vec<u64>, value: Option<i64>| {
                if let Some(v) = value {
                    bucket.push(v as u64);
                }
            };
            push(&mut durations, duration);
            push(&mut discoveries, discovery);
            push(&mut shadows, shadow);
            push(&mut anchor_res, anchor_resolution);
            push(&mut metadata, metadata_v);
            push(&mut quote, quote_v);
            push(&mut ranking, ranking_v);
            push(&mut requote, requote_v);
            push(&mut context_build, context_build_v);
            push(&mut materialization_economics, materialization_economics_v);
            push(&mut unattributed, unattributed_v);
        }
        for bucket in [
            &mut durations,
            &mut discoveries,
            &mut shadows,
            &mut anchor_res,
            &mut metadata,
            &mut quote,
            &mut ranking,
            &mut requote,
            &mut context_build,
            &mut materialization_economics,
            &mut unattributed,
        ] {
            bucket.sort_unstable();
        }
        Ok(LatencyStats {
            sample_count: durations.len() as u64,
            duration_p50_ms: percentile(&durations, 0.50),
            duration_p95_ms: percentile(&durations, 0.95),
            discovery_p50_ms: percentile(&discoveries, 0.50),
            discovery_p95_ms: percentile(&discoveries, 0.95),
            shadow_p50_ms: percentile(&shadows, 0.50),
            shadow_p95_ms: percentile(&shadows, 0.95),
            anchor_resolution_p50_ms: percentile(&anchor_res, 0.50),
            anchor_resolution_p95_ms: percentile(&anchor_res, 0.95),
            metadata_p50_ms: percentile(&metadata, 0.50),
            metadata_p95_ms: percentile(&metadata, 0.95),
            quote_p50_ms: percentile(&quote, 0.50),
            quote_p95_ms: percentile(&quote, 0.95),
            ranking_p50_ms: percentile(&ranking, 0.50),
            ranking_p95_ms: percentile(&ranking, 0.95),
            requote_p50_ms: percentile(&requote, 0.50),
            requote_p95_ms: percentile(&requote, 0.95),
            context_build_p50_ms: percentile(&context_build, 0.50),
            context_build_p95_ms: percentile(&context_build, 0.95),
            materialization_economics_p50_ms: percentile(&materialization_economics, 0.50),
            materialization_economics_p95_ms: percentile(&materialization_economics, 0.95),
            unattributed_p50_ms: percentile(&unattributed, 0.50),
            unattributed_p95_ms: percentile(&unattributed, 0.95),
        })
    }
}

/// Percentil nearest-rank: índice `ceil(n * p) - 1`, clampado.
fn percentile(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let index = ((sorted.len() as f64) * p).ceil() as usize;
    Some(sorted[index.saturating_sub(1).min(sorted.len() - 1)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(sequence: u64, net: f64, path: &str) -> RoundRecord {
        RoundRecord {
            sequence,
            completed_at: format!("2026-08-14T12:00:{:02}Z", sequence % 60),
            duration_ms: Some(1_000 + sequence),
            discovery_ms: Some(500 + sequence),
            shadow_ms: Some(200 + sequence),
            quotes: 8,
            edges: None,
            cycles_detected: 121,
            routes_ranked: 6,
            routes_evaluated: None,
            gross_positive: 1,
            economically_positive: 0,
            stable: None,
            risk_approved: Some(0),
            selected: None,
            net_usd_total: net,
            anchor_block: Some(100 + sequence),
            best_route_kind: Some("two_leg".into()),
            best_route_path: Some(path.into()),
            best_route_venues: Some("QuickSwap / UniswapV3".into()),
            best_route_gross: Some(1.5),
            best_route_net: Some(net),
            tui_spread_pct: Some(1.5),
            cycle_rate_pct: Some(0.5),
            cycle_net_usd: Some(net),
            anchor_resolution_ms: Some(10),
            metadata_ms: Some(20),
            quote_ms: Some(300),
            ranking_ms: Some(15),
            requote_ms: Some(400),
            context_build_ms: Some(25),
            materialization_economics_ms: Some(50),
            unattributed_ms: Some(5),
            best_route_gross_pnl_usd: Some(0.5),
            best_route_gas_cost_usd: Some(0.3),
            best_route_flashloan_cost_usd: Some(0.1),
            best_route_negative_cause: Some("GAS_DOMINATES".into()),
        }
    }

    fn temp_db(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "flashloan-history-test-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("test.db")
    }

    #[test]
    fn insert_and_read_roundtrip() {
        let db = RoundHistory::open(temp_db("roundtrip")).unwrap();
        db.insert_round(&record(1, 0.42, "WMATIC→USDC")).unwrap();

        let rows = db.recent_rounds(10).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.sequence, 1);
        assert_eq!(row.net_usd_total, 0.42);
        assert_eq!(row.best_route_path.as_deref(), Some("WMATIC→USDC"));
        assert_eq!(row.best_route_gross, Some(1.5));
        assert_eq!(row.risk_approved, Some(0));
        assert_eq!(row.anchor_block, Some(101));
        assert_eq!(row.best_route_gross_pnl_usd, Some(0.5));
        assert_eq!(row.best_route_gas_cost_usd, Some(0.3));
        assert_eq!(row.best_route_flashloan_cost_usd, Some(0.1));
        assert_eq!(
            row.best_route_negative_cause.as_deref(),
            Some("GAS_DOMINATES")
        );
    }

    #[test]
    fn recent_rounds_are_newest_first() {
        let db = RoundHistory::open(temp_db("ordering")).unwrap();
        db.insert_round(&record(1, 0.1, "A")).unwrap();
        db.insert_round(&record(2, 0.2, "B")).unwrap();
        db.insert_round(&record(3, 0.3, "C")).unwrap();

        let rows = db.recent_rounds(10).unwrap();
        let seqs: Vec<u64> = rows.iter().map(|row| row.sequence).collect();
        assert_eq!(seqs, vec![3, 2, 1]);
    }

    #[test]
    fn recent_rounds_respect_limit() {
        let db = RoundHistory::open(temp_db("limit")).unwrap();
        for seq in 1..=5 {
            db.insert_round(&record(seq, seq as f64 * 0.1, "X")).unwrap();
        }
        let rows = db.recent_rounds(2).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].sequence, 5);
        assert_eq!(rows[1].sequence, 4);
    }

    #[test]
    fn overall_stats_picks_best_net_round() {
        let db = RoundHistory::open(temp_db("stats")).unwrap();
        db.insert_round(&record(1, 0.05, "WMATIC→USDC")).unwrap();
        db.insert_round(&record(2, 1.20, "WMATIC→WETH→USDC")).unwrap();
        db.insert_round(&record(3, 0.70, "USDC→USDT")).unwrap();

        let stats = db.overall_stats().unwrap();
        assert_eq!(stats.total_rounds, 3);
        assert_eq!(stats.best_net_usd, Some(1.20));
        assert_eq!(stats.best_net_sequence, Some(2));
        assert_eq!(
            stats.best_net_route_path.as_deref(),
            Some("WMATIC→WETH→USDC")
        );
        assert!((stats.sum_net_usd - 1.95).abs() < 1e-9);
        assert_eq!(stats.sum_gross_positive, 3);
    }

    #[test]
    fn overall_stats_empty_db_returns_defaults() {
        let db = RoundHistory::open(temp_db("empty")).unwrap();
        let stats = db.overall_stats().unwrap();
        assert_eq!(stats.total_rounds, 0);
        assert_eq!(stats.best_net_usd, None);
        assert_eq!(stats.sum_net_usd, 0.0);
        let rows = db.recent_rounds(10).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn history_survives_reopen() {
        let path = temp_db("reopen");
        {
            let db = RoundHistory::open(&path).unwrap();
            db.insert_round(&record(7, 2.5, "WMATIC→WETH→USDC")).unwrap();
        }
        let db = RoundHistory::open(&path).unwrap();
        let rows = db.recent_rounds(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sequence, 7);
        assert_eq!(rows[0].net_usd_total, 2.5);
    }

    #[test]
    fn best_net_falls_back_to_aggregate_when_no_route_net() {
        let db = RoundHistory::open(temp_db("fallback")).unwrap();
        let mut r = record(1, 0.99, "WMATIC→USDC");
        r.best_route_net = None;
        db.insert_round(&r).unwrap();

        let stats = db.overall_stats().unwrap();
        assert_eq!(stats.best_net_usd, Some(0.99));
    }

    #[test]
    fn latency_stats_computes_p50_p95_per_stage() {
        let db = RoundHistory::open(temp_db("latency")).unwrap();
        // durations: 1001..1005 → p50=1003, p95=1005
        for seq in 1..=5u64 {
            let mut r = record(seq, 0.1, "X");
            r.duration_ms = Some(1_000 + seq);
            r.discovery_ms = Some(500 + seq);
            r.shadow_ms = Some(200 + seq);
            r.requote_ms = Some(400 + seq);
            db.insert_round(&r).unwrap();
        }

        let stats = db.latency_stats(10).unwrap();
        assert_eq!(stats.sample_count, 5);
        assert_eq!(stats.duration_p50_ms, Some(1_003));
        assert_eq!(stats.duration_p95_ms, Some(1_005));
        assert_eq!(stats.discovery_p50_ms, Some(503));
        assert_eq!(stats.discovery_p95_ms, Some(505));
        assert_eq!(stats.shadow_p50_ms, Some(203));
        assert_eq!(stats.shadow_p95_ms, Some(205));
        // ARGUS stage waterfall percentiles reconcile the same way.
        assert_eq!(stats.requote_p50_ms, Some(403));
        assert_eq!(stats.requote_p95_ms, Some(405));
    }

    #[test]
    fn latency_stats_empty_db_returns_none() {
        let db = RoundHistory::open(temp_db("latency-empty")).unwrap();
        let stats = db.latency_stats(10).unwrap();
        assert_eq!(stats.sample_count, 0);
        assert_eq!(stats.duration_p50_ms, None);
        assert_eq!(stats.duration_p95_ms, None);
    }

    #[test]
    fn latency_stats_ignores_null_stage_values() {
        let db = RoundHistory::open(temp_db("latency-null")).unwrap();
        let mut r = record(1, 0.1, "X");
        r.discovery_ms = None;
        r.shadow_ms = None;
        db.insert_round(&r).unwrap();

        let stats = db.latency_stats(10).unwrap();
        assert_eq!(stats.duration_p50_ms, Some(1_001));
        assert_eq!(stats.discovery_p50_ms, None);
        assert_eq!(stats.shadow_p50_ms, None);
    }
}
