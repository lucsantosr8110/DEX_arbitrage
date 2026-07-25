// src/utils/telegram.rs

use crate::config::Config;
use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::Serialize;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

#[derive(Clone)]
pub struct TelegramNotifier {
    pub enabled: bool,
    pub bot_token: String,
    pub chat_id: String,
    pub cooldown: Duration,
    last_sent: Arc<Mutex<Instant>>,
    client: Client,
}

impl TelegramNotifier {
    pub async fn init_from_config(cfg: &Config) -> Result<Self> {
        // 🔍 Verifica se a seção telegram existe
        let telegram_config = if let Some(tg_cfg) = &cfg.telegram { // ✅ Remove .as_ref()
            tg_cfg
        } else {
            warn!("📵 Seção [telegram] não encontrada no config - desativando notificações");
            return Ok(Self::disabled());
        };

        if !telegram_config.enabled {
            warn!("📵 Telegram desativado via config.");
            return Ok(Self::disabled());
        }

        // 🔐 Busca credenciais (env vars têm prioridade)
        let token = std::env::var("TELEGRAM_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| telegram_config.bot_token.clone());

        let chat_id = std::env::var("TELEGRAM_CHAT_ID")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| telegram_config.chat_id.clone());

        // ✅ Validação rigorosa das credenciais
        if token.is_empty() {
            return Err(anyhow!("❌ TELEGRAM_TOKEN não encontrado (env: TELEGRAM_TOKEN ou config.telegram.bot_token)"));
        }

        if chat_id.is_empty() {
            return Err(anyhow!("❌ TELEGRAM_CHAT_ID não encontrado (env: TELEGRAM_CHAT_ID ou config.telegram.chat_id)"));
        }

        if token == "${TELEGRAM_TOKEN}" || token == "${TELEGRAM_BOT_TOKEN}"
            || chat_id == "${TELEGRAM_CHAT_ID}"
        {
            return Err(anyhow!("❌ Variáveis Telegram não substituídas - verifique .env (TELEGRAM_TOKEN e TELEGRAM_CHAT_ID)"));
        }

        // ⏱️ Configuração de cooldown com fallback seguro
        let cooldown_secs = if telegram_config.alert_cooldown > 0 {
            telegram_config.alert_cooldown as u64
        } else {
            0 // Cooldown desativado conforme config
        };

        info!(
            "✅ Telegram inicializado | Chat: {} | Cooldown: {}s",
            chat_id, cooldown_secs
        );

        Ok(Self {
            enabled: true,
            bot_token: token,
            chat_id,
            cooldown: Duration::from_secs(cooldown_secs),
            last_sent: Arc::new(Mutex::new(
                Instant::now().checked_sub(Duration::from_secs(cooldown_secs))
                    .unwrap_or(Instant::now())
            )),
            client: Client::new(),
        })
    }

    pub fn disabled() -> Self {
        Self {
            enabled: false,
            bot_token: String::new(),
            chat_id: String::new(),
            cooldown: Duration::from_secs(60),
            last_sent: Arc::new(Mutex::new(Instant::now())),
            client: Client::new(),
        }
    }

    pub async fn send_alert(&self, title: &str, message: &str) -> Result<()> {
        let formatted_msg = format!("🚨 *{}*\n\n{}", title, message);
        self.send(&formatted_msg).await
    }

    pub async fn send_profit_alert(&self, profit_usd: f64, details: &str) -> Result<()> {
        let msg = format!(
            "💰 *Lucro Detectado!*\n\n💵 *Valor:* `${:.6}`\n📊 *Detalhes:* {}",
            profit_usd, details
        );
        self.send(&msg).await
    }

    pub async fn send_error_alert(&self, context: &str, error: &str) -> Result<()> {
        let msg = format!("❌ *Erro em {}*\n\n`{}`", context, error);
        self.send(&msg).await
    }

    pub async fn send_startup_alert(&self, version: &str, network: &str) -> Result<()> {
        let msg = format!(
            "🤖 *Bot Iniciado!*\n\n⚙️ *Versão:* `{}`\n🌐 *Rede:* `{}`\n⏰ *Horário:* {}",
            version,
            network,
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
        );
        self.send(&msg).await
    }

    pub async fn send(&self, text: &str) -> Result<()> {
        if !self.enabled {
            debug!("📵 Telegram desativado - mensagem ignorada");
            return Ok(());
        }

        // ⏱️ Verificação de cooldown
        if self.cooldown.as_secs() > 0 {
            let now = Instant::now();
            let mut last_sent = self.last_sent.lock().await;
            
            if now.duration_since(*last_sent) < self.cooldown {
                debug!("⏳ Cooldown ativo ({:?} restante) - ignorando envio Telegram", 
                       self.cooldown - now.duration_since(*last_sent));
                return Ok(());
            }
            *last_sent = now;
        }

        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);
        
        let payload = TelegramMessage {
            chat_id: self.chat_id.clone(),
            text: text.to_string(),
            parse_mode: "Markdown".to_string(),
            disable_web_page_preview: true,
        };

        match self.client.post(&url).json(&payload).send().await {
            Ok(response) => {
                if response.status().is_success() {
                    debug!("✅ Mensagem Telegram enviada com sucesso");
                    Ok(())
                } else {
                    let status = response.status();
                    let error_text = response.text().await.unwrap_or_default();
                    error!("❌ Erro Telegram ({}): {}", status, error_text);
                    Err(anyhow!("Telegram API error: {} - {}", status, error_text))
                }
            }
            Err(e) => {
                error!("❌ Falha de rede ao enviar para Telegram: {}", e);
                Err(anyhow!("Network error sending to Telegram: {}", e))
            }
        }
    }

    // 🔄 Método para forçar envio (ignora cooldown)
    pub async fn send_force(&self, text: &str) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);
        
        let payload = TelegramMessage {
            chat_id: self.chat_id.clone(),
            text: text.to_string(),
            parse_mode: "Markdown".to_string(),
            disable_web_page_preview: true,
        };

        match self.client.post(&url).json(&payload).send().await {
            Ok(response) => {
                if response.status().is_success() {
                    debug!("✅ Mensagem Telegram (forçada) enviada com sucesso");
                    Ok(())
                } else {
                    let status = response.status();
                    error!("❌ Erro Telegram forçado: {}", status);
                    Err(anyhow!("Telegram forced send error: {}", status))
                }
            }
            Err(e) => {
                error!("❌ Falha de rede no envio forçado: {}", e);
                Err(anyhow!("Network error in forced send: {}", e))
            }
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

#[derive(Serialize)]
struct TelegramMessage {
    chat_id: String,
    text: String,
    parse_mode: String,
    disable_web_page_preview: bool,
}

// 📦 Implementações auxiliares para uso fácil
impl TelegramNotifier {
    pub async fn notify_opportunity(
        &self, 
        spread: f64, 
        profit_estimate: f64,
        pair: &str,
        dexes: &[&str]
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let msg = format!(
            "🎯 *Oportunidade Detectada!*\n\n\
             📈 *Par:* `{}`\n\
             📊 *Spread:* `{:.4}%`\n\
             💰 *Lucro Est.:* `${:.6}`\n\
             🏦 *DEXs:* `{}`\n\
             ⏰ *Horário:* {}",
            pair,
            spread,
            profit_estimate,
            dexes.join(" → "),
            chrono::Utc::now().format("%H:%M:%S")
        );

        self.send(&msg).await
    }

    pub async fn notify_execution(
        &self,
        success: bool,
        profit_usd: f64,
        tx_hash: Option<&str>,
        gas_cost: f64
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let emoji = if success { "✅" } else { "❌" };
        let status = if success { "Sucesso" } else { "Falha" };
        
        let tx_info = if let Some(hash) = tx_hash {
            format!("[Ver TX](https://polygonscan.com/tx/{})", hash)
        } else {
            "N/A".to_string()
        };

        let msg = format!(
            "{} *Execução Concluída*\n\n\
             📋 *Status:* `{}`\n\
             💵 *Lucro:* `${:.6}`\n\
             ⛽ *Gás:* `${:.4}`\n\
             🔗 *TX:* {}\n\
             🕒 *Horário:* {}",
            emoji,
            status,
            profit_usd,
            gas_cost,
            tx_info,
            chrono::Utc::now().format("%H:%M:%S")
        );

        self.send(&msg).await
    }
}