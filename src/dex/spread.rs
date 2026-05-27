// src/dex/spread.rs
use ethers::types::U256;

pub struct SpreadCalculator;

impl SpreadCalculator {
    pub fn new() -> Self {
        Self
    }

    pub fn calculate_net_profit(&self, gross_profit: f64, gas_cost: f64) -> f64 {
        if gross_profit > gas_cost {
            gross_profit - gas_cost
        } else {
            0.0
        }
    }

    pub fn calculate_spread_percentage(&self, price_a: f64, price_b: f64) -> f64 {
        if price_a == 0.0 {
            return 0.0;
        }
        ((price_b - price_a) / price_a) * 100.0
    }
}

pub fn calculate_spread(price_a: f64, price_b: f64) -> f64 {
    if price_a == 0.0 {
        return 0.0;
    }
    ((price_b - price_a) / price_a) * 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_spread() {
        let calculator = SpreadCalculator::new();
        
        // Teste básico
        assert_eq!(calculate_spread(100.0, 110.0), 10.0);
        assert_eq!(calculator.calculate_spread_percentage(100.0, 110.0), 10.0);
        
        // Teste com preço zero
        assert_eq!(calculate_spread(0.0, 110.0), 0.0);
        assert_eq!(calculator.calculate_spread_percentage(0.0, 110.0), 0.0);
        
        // Teste de lucro líquido
        assert_eq!(calculator.calculate_net_profit(150.0, 50.0), 100.0);
        assert_eq!(calculator.calculate_net_profit(30.0, 50.0), 0.0);
    }
}