use metrics::{counter, histogram};

pub struct DexMetrics {
    dex_name: String,
}

impl DexMetrics {
    pub fn new(dex_name: String) -> Self {
        Self { dex_name }
    }

    pub fn record_success(&self, duration: f64) {
        counter!("dex_requests_total", 1, "dex" => self.dex_name.clone(), "status" => "success");
        histogram!("dex_request_duration_seconds", duration, "dex" => self.dex_name.clone());
    }

    pub fn record_failure(&self, error_type: &str) {
        counter!("dex_requests_total", 1, "dex" => self.dex_name.clone(), "status" => "failure", "error" => error_type.to_string());
    }

    pub fn record_timeout(&self) {
        counter!("dex_requests_total", 1, "dex" => self.dex_name.clone(), "status" => "timeout");
    }

    pub fn record_circuit_breaker_trip(&self) {
        counter!("circuit_breaker_trips", 1, "dex" => self.dex_name.clone());
    }

    pub fn record_init_failure(&self) {
        counter!("dex_initialization_failures", 1, "dex" => self.dex_name.clone());
    }
}

