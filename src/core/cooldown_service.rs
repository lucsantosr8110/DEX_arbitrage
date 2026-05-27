// src/core/cooldown_service.rs
use crate::config::CooldownManager;
use std::sync::Arc;
use tracing::{debug, warn};

#[derive(Clone)]
pub struct CooldownService {
    manager: Option<Arc<CooldownManager>>,
}

impl CooldownService {
    pub fn new(manager: Option<Arc<CooldownManager>>) -> Self {
        Self { manager }
    }
    
    pub async fn check_execution(
        &self,
        pair: &str,
        strategy: &str,
        current_block: Option<u64>,
    ) -> Result<(), String> {
        if let Some(manager) = &self.manager {
            manager.can_execute(pair, strategy, current_block).await?;
            Ok(())
        } else {
            Ok(()) // Se não há manager, permite todas
        }
    }
    
    pub async fn record_execution(
        &self,
        pair: &str,
        strategy: &str,
        block_number: Option<u64>,
    ) {
        if let Some(manager) = &self.manager {
            manager.record_execution(pair, strategy, block_number).await;
        }
    }
    
    pub async fn record_revert(&self, pair: &str) {
        if let Some(manager) = &self.manager {
            manager.record_revert(pair).await;
        }
    }
    
    pub async fn get_stats(&self) -> Option<crate::config::CooldownStats> {
        if let Some(manager) = &self.manager {
            Some(manager.get_stats().await)
        } else {
            None
        }
    }
}