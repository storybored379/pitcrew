use pitcrew_admin::{Error, accounting::summarize};
use pitcrew_config::admin::AdminRates;
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn usage(tokens: [u64; 4]) -> Value {
    json!({
        "input_tokens": tokens[0],
        "output_tokens": tokens[1],
        "cache_creation_input_tokens": tokens[2],
        "cache_read_input_tokens": tokens[3],
        // Historical producer estimates must not override local admin rates.
        "estimated_cost_microusd": 987_654,
    })
}

fn rate(values: [u64; 4]) -> AdminRates {
    AdminRates {
        input_microusd_per_million: values[0],
        output_microusd_per_million: values[1],
        cache_creation_microusd_per_million: values[2],
        cache_read_microusd_per_million: values[3],
    }
}

#[test]
fn absent_and_null_usage_remain_unknown_but_explicit_zero_usage_is_known() {
    let rates = BTreeMap::from([("recorded".into(), rate([1_000_000; 4]))]);
    let rows = summarize(
        &[
            json!({"provider": "recorded"}),
            json!({"provider": "recorded", "usage": null}),
        ],
        &rates,
    )
    .unwrap();
    assert_eq!(rows[0]["calls"], 2);
    assert_eq!(rows[0]["calls_with_usage"], 0);
    assert_eq!(rows[0]["calls_without_usage"], 2);
    assert_eq!(rows[0]["estimated_known_microusd"], Value::Null);

    let rows = summarize(
        &[
            json!({"provider": "recorded"}),
            json!({"provider": "recorded", "usage": usage([0; 4])}),
        ],
        &rates,
    )
    .unwrap();
    assert_eq!(rows[0]["calls_with_usage"], 1);
    assert_eq!(rows[0]["calls_without_usage"], 1);
    assert_eq!(rows[0]["estimated_known_microusd"], 0);
}

#[test]
fn absent_rates_do_not_reuse_a_producer_estimate_and_explicit_zero_rates_are_valid() {
    let records = [json!({"provider": "recorded", "usage": usage([2, 3, 5, 7])})];
    let rows = summarize(&records, &BTreeMap::new()).unwrap();
    assert_eq!(rows[0]["calls_with_usage"], 1);
    assert_eq!(rows[0]["estimated_known_microusd"], Value::Null);
    let rates = BTreeMap::from([("recorded".into(), rate([0; 4]))]);
    let rows = summarize(&records, &rates).unwrap();
    assert_eq!(rows[0]["estimated_known_microusd"], 0);
}

#[test]
fn fractional_costs_round_up_once_after_combining_calls_and_token_classes() {
    let rates = BTreeMap::from([("recorded".into(), rate([1; 4]))]);
    for (tokens, expected) in [(0, 0), (999_999, 1), (1_000_000, 1), (1_000_001, 2)] {
        let rows = summarize(
            &[json!({"provider": "recorded", "usage": usage([tokens, 0, 0, 0])})],
            &rates,
        )
        .unwrap();
        assert_eq!(rows[0]["estimated_known_microusd"], expected);
    }
    let rows = summarize(
        &[
            json!({"provider": "recorded", "usage": usage([100_000; 4])}),
            json!({"provider": "recorded", "usage": usage([300_000; 4])}),
        ],
        &rates,
    )
    .unwrap();
    // The complete known usage costs 1.6 micro USD: neither 3 per-call units
    // nor 4 independently rounded token-class units.
    assert_eq!(rows[0]["estimated_known_microusd"], 2);
}

#[test]
fn output_and_cache_totals_reject_overflow_but_preserve_each_providers_exact_maximum() {
    for (index, column) in [
        (1, "output_tokens"),
        (2, "cache_creation_tokens"),
        (3, "cache_read_tokens"),
    ] {
        let mut maximum = [0; 4];
        maximum[index] = u64::MAX;
        let mut one = [0; 4];
        one[index] = 1;
        let records = [
            json!({"provider": "alpha", "usage": usage(maximum)}),
            json!({"provider": "alpha", "usage": usage(one)}),
        ];
        assert!(matches!(
            summarize(&records, &BTreeMap::new()),
            Err(Error::Storage)
        ));
        let rows = summarize(
            &[
                records[0].clone(),
                json!({"provider": "beta", "usage": usage(maximum)}),
            ],
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row["calls_with_usage"] == 1));
        assert!(rows.iter().all(|row| row[column] == u64::MAX));
    }
}

#[test]
fn cost_overflow_keeps_known_usage_but_withholds_an_unrepresentable_estimate() {
    for (tokens, prices, expected) in [
        ([u64::MAX, 0, 0, 0], [1_000_000, 0, 0, 0], json!(u64::MAX)),
        ([u64::MAX, 0, 0, 0], [1_000_001, 0, 0, 0], Value::Null),
        ([u64::MAX; 4], [u64::MAX; 4], Value::Null),
    ] {
        let rates = BTreeMap::from([("recorded".into(), rate(prices))]);
        let rows = summarize(
            &[json!({"provider": "recorded", "usage": usage(tokens)})],
            &rates,
        )
        .unwrap();
        assert_eq!(rows[0]["calls_with_usage"], 1);
        assert_eq!(rows[0]["calls_without_usage"], 0);
        assert_eq!(rows[0]["input_tokens"], u64::MAX);
        assert_eq!(rows[0]["estimated_known_microusd"], expected);
    }
}
