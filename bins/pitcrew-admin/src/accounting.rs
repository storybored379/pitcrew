//! Recorded calls only. Missing usage never becomes a zero-cost assertion.
use crate::{Error, Result};
use pitcrew_config::admin::AdminRates;
use pitcrew_deciders::{Usage, providers::CostRates};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub fn summarize(records: &[Value], rates: &BTreeMap<String, AdminRates>) -> Result<Vec<Value>> {
    let mut grouped: BTreeMap<String, (u64, u64, Usage)> = BTreeMap::new();
    for record in records {
        let provider = record["provider"].as_str().ok_or(Error::Storage)?;
        let entry = grouped.entry(provider.into()).or_default();
        entry.0 = entry.0.checked_add(1).ok_or(Error::Storage)?;
        if !record["usage"].is_null() {
            let usage: Usage = serde_json::from_value(record["usage"].clone())?;
            entry.1 = entry.1.checked_add(1).ok_or(Error::Storage)?;
            entry.2.input_tokens = entry
                .2
                .input_tokens
                .checked_add(usage.input_tokens)
                .ok_or(Error::Storage)?;
            entry.2.output_tokens = entry
                .2
                .output_tokens
                .checked_add(usage.output_tokens)
                .ok_or(Error::Storage)?;
            entry.2.cache_creation_input_tokens = entry
                .2
                .cache_creation_input_tokens
                .checked_add(usage.cache_creation_input_tokens)
                .ok_or(Error::Storage)?;
            entry.2.cache_read_input_tokens = entry
                .2
                .cache_read_input_tokens
                .checked_add(usage.cache_read_input_tokens)
                .ok_or(Error::Storage)?;
        }
    }
    Ok(grouped.into_iter().map(|(provider,(calls,known,usage))| {
        let cost = rates.get(&provider).filter(|_| known > 0).and_then(|rate| CostRates {
            input_microusd_per_million:rate.input_microusd_per_million,
            output_microusd_per_million:rate.output_microusd_per_million,
            cache_creation_microusd_per_million:rate.cache_creation_microusd_per_million,
            cache_read_microusd_per_million:rate.cache_read_microusd_per_million,
        }.estimate(usage));
        json!({"provider":provider,"calls":calls,"calls_with_usage":known,"calls_without_usage":calls-known,"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens,"cache_creation_tokens":usage.cache_creation_input_tokens,"cache_read_tokens":usage.cache_read_input_tokens,"estimated_known_microusd":cost})
    }).collect())
}
