//! OpenAI Pricing for GPT-6 Luna

#[derive(Debug, Clone, Copy)]
pub struct Usage {
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

/// Calculate cost in microUSD.
pub fn calculate_cost(usage: &Usage) -> u64 {
    // Pricing per 1M tokens (in USD):
    // Ordinary: $0.10
    // Cache Read: $0.01
    // Cache Write: $0.125
    // Output: $0.50
    
    // Convert to microUSD per token:
    // Ordinary: 0.10 * 1e6 / 1e6 = 0.1 microUSD
    // Cache Read: 0.01 * 1e6 / 1e6 = 0.01 microUSD
    // Cache Write: 0.125 * 1e6 / 1e6 = 0.125 microUSD
    // Output: 0.50 * 1e6 / 1e6 = 0.5 microUSD

    let input_tokens_net = usage.input_tokens
        .saturating_sub(usage.cache_read_tokens)
        .saturating_sub(usage.cache_write_tokens);

    // Using fixed point arithmetic (scaling by 1000 to maintain precision for 0.125)
    // 0.1 USD/M = 0.1 * 1,000,000 / 1,000,000 = 0.1 microUSD/token.
    // Let's use 1000 microUSD as the base unit to avoid floats, 
    // or just calculate carefully.
    
    // microUSD = tokens * price_per_M / 1,000,000
    // Using 128-bit intermediate to prevent overflow.
    
    let ordinary_cost = (input_tokens_net as u128 * 100_000) / 1_000_000;
    let cache_read_cost = (usage.cache_read_tokens as u128 * 10_000) / 1_000_000;
    let cache_write_cost = (usage.cache_write_tokens as u128 * 125_000) / 1_000_000;
    let output_cost = (usage.output_tokens as u128 * 500_000) / 1_000_000;

    (ordinary_cost + cache_read_cost + cache_write_cost + output_cost) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pricing() {
        let usage = Usage {
            input_tokens: 1_000_000,
            cache_read_tokens: 100_000,
            cache_write_tokens: 100_000,
            output_tokens: 100_000,
            reasoning_tokens: 50_000,
        };
        // Net input = 1,000,000 - 100,000 - 100,000 = 800,000
        // Ordinary: 800,000 * 0.1 = 80,000 microUSD
        // Cache Read: 100,000 * 0.01 = 1,000 microUSD
        // Cache Write: 100,000 * 0.125 = 12,500 microUSD
        // Output: 100,000 * 0.5 = 50,000 microUSD
        // Total = 143,500 microUSD
        
        let cost = calculate_cost(&usage);
        assert_eq!(cost, 143_500);
    }
}
