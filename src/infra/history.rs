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
    best_route_net        REAL
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
        for column in ["discovery_ms", "shadow_ms"] {
            if !existing.iter().any(|name| name == column) {
                conn.execute_batch(&format!("ALTER TABLE rounds ADD COLUMN {column} INTEGER"))
                    .with_context(|| format!("migrar coluna {column} no histórico"))?;
            }
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
                best_route_gross, best_route_net
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                ?8, ?9, ?10,
                ?11, ?12, ?13,
                ?14, ?15, ?16, ?17,
                ?18, ?19, ?20,
                ?21, ?22
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
                "SELECT duration_ms, discovery_ms, shadow_ms
                 FROM rounds
                 ORDER BY id DESC
                 LIMIT ?1",
            )
            .context("preparar latency_stats")?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?, row.get::<_, Option<i64>>(2)?))
            })
            .context("query latency_stats")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("ler latency_stats")?;

        let mut durations = Vec::new();
        let mut discoveries = Vec::new();
        let mut shadows = Vec::new();
        for (duration, discovery, shadow) in rows {
            if let Some(value) = duration {
                durations.push(value as u64);
            }
            if let Some(value) = discovery {
                discoveries.push(value as u64);
            }
            if let Some(value) = shadow {
                shadows.push(value as u64);
            }
        }
        durations.sort_unstable();
        discoveries.sort_unstable();
        shadows.sort_unstable();
        Ok(LatencyStats {
            sample_count: durations.len() as u64,
            duration_p50_ms: percentile(&durations, 0.50),
            duration_p95_ms: percentile(&durations, 0.95),
            discovery_p50_ms: percentile(&discoveries, 0.50),
            discovery_p95_ms: percentile(&discoveries, 0.95),
            shadow_p50_ms: percentile(&shadows, 0.50),
            shadow_p95_ms: percentile(&shadows, 0.95),
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
