use crate::core::types::{ArbitrageOpportunity, BundleResult};
use anyhow::{Error, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::Path,
};
use tracing::{error, info, warn};

// ================================================================
// LOGGING STRUCT — Exportação de Arbitragem
// ================================================================

/// Versão simplificada de ArbitrageOpportunity para fins de serialização (CSV/JSON).
#[derive(Debug, Serialize, Deserialize)]
pub struct SerializableArbitrageOpportunity {
    pub id: String,
    pub timestamp: u64,
    pub pair: String,
    pub buy_dex: String,
    pub sell_dex: String,
    pub buy_price: f64,
    pub sell_price: f64,
    pub spread_percent: f64,
    pub net_profit_usd: f64,
    pub path: String, // Representação em string do vetor path
}

impl From<&ArbitrageOpportunity> for SerializableArbitrageOpportunity {
    fn from(opp: &ArbitrageOpportunity) -> Self {
        SerializableArbitrageOpportunity {
            id: opp.id.clone(),
            timestamp: opp.timestamp,
            pair: opp.pair.clone(),
            buy_dex: opp.buy_dex.clone(),
            sell_dex: opp.sell_dex.clone(),
            buy_price: opp.buy_price,
            sell_price: opp.sell_price,
            spread_percent: opp.spread_percent,
            net_profit_usd: opp.net_profit_usd,
            path: opp.path.join("->"),
        }
    }
}

/// ================================================================
/// 📋 GERADOR DE RELATÓRIOS E AUDITORIAS
/// ================================================================
pub struct ReportGenerator;

impl ReportGenerator {
    /// 🆕 Gera relatório consolidado de flashloans
    pub fn generate_flashloan_report(&self, results: &[BundleResult]) -> String {
        let flashloan_results: Vec<&BundleResult> = results
            .iter()
            .filter(|r| {
                r.execution_mode
                    .as_ref()
                    .map_or(false, |m| m.contains("flashloan"))
            })
            .collect();

        let total_profit: f64 = flashloan_results.iter().map(|r| r.profit).sum();
        let success_rate = if !flashloan_results.is_empty() {
            flashloan_results.iter().filter(|r| r.success).count() as f64
                / flashloan_results.len() as f64
        } else {
            0.0
        };

        format!(
            "📊 RELATÓRIO FLASHLOAN\n\
             • Execuções: {}\n\
             • Lucro Total: ${:.2}\n\
             • Taxa de Sucesso: {:.1}%\n\
             • Modo Preferido: {}",
            flashloan_results.len(),
            total_profit,
            success_rate * 100.0,
            self.detect_preferred_mode(&flashloan_results)
        )
    }

    /// Detecta o modo de flashloan mais utilizado (wrapper / aave)
    fn detect_preferred_mode(&self, results: &[&BundleResult]) -> String {
        let wrapper_count = results
            .iter()
            .filter(|r| r.execution_mode.as_ref().map_or(false, |m| m == "wrapper"))
            .count();

        let aave_count = results
            .iter()
            .filter(|r| r.execution_mode.as_ref().map_or(false, |m| m == "aave"))
            .count();

        match (wrapper_count, aave_count) {
            (w, a) if w > a => "WRAPPER".to_string(),
            (w, a) if a > w => "AAVE".to_string(),
            _ => "HÍBRIDO".to_string(),
        }
    }
}

/// ================================================================
/// 🧾 FUNÇÕES AUXILIARES DE LOG E AUDITORIA
/// ================================================================

/// Loga erro detalhado com todas as causas
pub fn report_error(e: &Error) {
    error!("❌ Erro fatal encontrado: {}", e);
    let mut cause = e.source();
    let mut i = 1;
    while let Some(err) = cause {
        error!("  ➡️ Causa #{}: {}", i, err);
        cause = err.source();
        i += 1;
    }
}

/// Salva uma oportunidade em JSON (auditoria)
pub fn save_opportunity_json<P: AsRef<Path>>(
    opp: &ArbitrageOpportunity,
    path: P,
) -> Result<(), anyhow::Error> {
    let serializable = SerializableArbitrageOpportunity::from(opp);
    let json_str = serde_json::to_string_pretty(&serializable)?;

    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    writer.write_all(json_str.as_bytes())?;
    writer.flush()?;

    info!("📁 Oportunidade exportada para JSON.");
    Ok(())
}

/// Salva oportunidade em CSV (append se existir)
pub fn save_opportunity_csv<P: AsRef<Path>>(
    opp: &ArbitrageOpportunity,
    path: P,
) -> Result<(), anyhow::Error> {
    let serializable = SerializableArbitrageOpportunity::from(opp);
    let file_exists = path.as_ref().exists();
    let file = OpenOptions::new().create(true).append(true).open(path)?;

    let mut writer = csv::WriterBuilder::new()
        .has_headers(!file_exists)
        .from_writer(file);

    writer.serialize(serializable)?;
    writer.flush()?;
    info!("📊 Oportunidade exportada para CSV.");
    Ok(())
}

/// Loga resultado de execução de arbitragem
pub fn log_bundle_result(result: &BundleResult) {
    if result.success {
        info!(
            "✅ Execução bem-sucedida. Tx: {:?}, lucro: {:.4}, modo: {:?}",
            result.tx_hash, result.profit, result.execution_mode
        );
    } else {
        warn!(
            "⚠️ Execução falhou/revertida. Tx: {:?}, aceito={}, lucro: {:.4}, modo: {:?}",
            result.tx_hash, result.accepted, result.profit, result.execution_mode
        );
    }
}

/// Salva resultado de execução em JSON (append)
pub fn save_bundle_result_json<P: AsRef<Path>>(
    result: &BundleResult,
    path: P,
) -> Result<(), anyhow::Error> {
    let entry = json!({
        "timestamp": Utc::now().to_rfc3339(),
        "success": result.success,
        "tx_hash": result.tx_hash,
        "accepted": result.accepted,
        "profit": result.profit,
        "execution_mode": result.execution_mode,
    });

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{}", entry)?;
    Ok(())
}
