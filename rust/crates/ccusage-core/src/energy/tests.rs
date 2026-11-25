use super::*;

// Synthetic rows, not CLEER data. Opus 4.5: 100 Wh/USD, SD 50%, 0.6 g/Wh. Opus 4.1: 10 Wh/USD, SD 10%, 0.5 g/Wh.
const FIXTURE: &str = concat!(
    "model_name,use_case,tier,energy_wh_mean,energy_wh_sd,carbon_gco2e_mean,energy_wh_per_usd,dashboard_description\n",
    "Claude Opus 4.5,Agentic,Typical,10,5,6,100,\"a, quoted \"\"description\"\"\"\n",
    "Claude Opus 4.5,Chat,Typical,10,5,6,999,x\n",
    "Claude Opus 4.5,Agentic,Light,10,5,6,999,x\n",
    "Claude Opus 4.1,Agentic,Typical,10,1,5,10,x\n",
    "Claude Sonnet 4,Agentic,Typical,10,20,5,10,x\n",
    "GPT-5.4 mini,Agentic,Typical,10,1,5,20,x\n",
    "Bad Row,Agentic,Typical,0,0,0,0,x\n",
);

fn config(carbon: bool, grid: Option<f64>) -> EnergyConfig {
    EnergyConfig {
        csv_path: String::new(),
        grid_g_per_kwh: grid,
        carbon,
    }
}

fn table() -> EnergyTable {
    EnergyTable::parse(FIXTURE).unwrap()
}

#[test]
fn exact_match_uses_agentic_typical_row_only() {
    let e = table().estimate("claude-opus-4-5-20251101", 1.0).unwrap();
    assert!(!e.proxy);
    assert_eq!(e.format(&config(true, None)), "50-150 Wh · 30-90 gCO₂e");
}

#[test]
fn bracket_suffix_ids_match_exactly() {
    let e = table()
        .estimate("claude-opus-4-5-20251101[1m]", 1.0)
        .unwrap();
    assert!(!e.proxy);
    assert_eq!(e.wh, 100.0);
}

#[test]
fn newer_claude_version_proxies_to_lower_version_with_tilde() {
    let e = table().estimate("claude-opus-5-5", 1.0).unwrap();
    assert!(e.proxy);
    assert_eq!(e.wh, 100.0);
    assert!(e.format(&config(false, None)).starts_with('~'));
}

#[test]
fn older_claude_version_falls_back_to_newest_of_family() {
    let e = table().estimate("claude-opus-3", 1.0).unwrap();
    assert!(e.proxy);
    assert_eq!(e.wh, 100.0);
}

#[test]
fn non_claude_exact_match_and_unknown_models() {
    let table = table();
    assert_eq!(table.estimate("GPT-5.4 mini", 1.0).unwrap().wh, 20.0);
    assert!(table.estimate("gpt-9-unknown", 1.0).is_none());
    assert!(table.estimate("claude-haiku-4-5", 1.0).is_none());
    assert!(table.estimate("claude-opus-4-5", 0.0).is_none());
}

#[test]
fn lower_bound_is_clamped_at_zero() {
    let e = table().estimate("claude-sonnet-4", 1.0).unwrap();
    assert_eq!(e.lo, 0.0);
    assert_eq!(e.format(&config(false, None)), "0-30 Wh");
}

#[test]
fn small_values_keep_one_decimal_and_tiny_ranges_collapse() {
    let t = table();
    // 0.05 USD * 100 Wh/USD = 5 Wh (SD 50%): 2.5-7.5 Wh, 1.5-4.5 g
    let e = t.estimate("claude-opus-4-5", 0.05).unwrap();
    assert_eq!(e.format(&config(true, None)), "2.5-7.5 Wh · 1.5-4.5 gCO₂e");
    // high end rounds to 0.0: whole range collapses
    let e = t.estimate("claude-opus-4-5", 0.0001).unwrap();
    assert_eq!(e.format(&config(true, None)), "<0.1 Wh · <0.1 gCO₂e");
    // low end rounding to 0.0 prints "0"; 0.075 Wh high end is 0.1
    let e = t.estimate("claude-opus-4-5", 0.0005).unwrap();
    assert_eq!(e.format(&config(false, None)), "0-0.1 Wh");
    assert_eq!(e.format(&config(true, None)), "0-0.1 Wh · <0.1 gCO₂e");
    // 9.96 must not render as "10.0"
    assert_eq!(super::num(9.96), "10");
    assert_eq!(super::num(0.0), "0");
    assert_eq!(super::num(0.04), "0");
    assert_eq!(super::num(12.4), "12");
}

#[test]
fn grid_override_replaces_row_carbon_ratio() {
    let e = table().estimate("claude-opus-4-5", 1.0).unwrap();
    assert_eq!(
        e.format(&config(true, Some(1000.0))),
        "50-150 Wh · 50-150 gCO₂e"
    );
}

#[test]
fn energy_only_drops_carbon() {
    let e = table().estimate("claude-opus-4-5", 1.0).unwrap();
    assert_eq!(e.format(&config(false, None)), "50-150 Wh");
}

#[test]
fn switches_to_kwh_when_high_end_reaches_1000() {
    let e = table().estimate("claude-opus-4-5", 10.0).unwrap();
    assert_eq!(e.format(&config(false, None)), "0.5-1.5 kWh");
}

#[test]
fn totals_sum_per_model_and_skip_unknown() {
    let total = table()
        .estimate_total([
            ("claude-opus-4-5", 1.0),
            ("claude-opus-4-1", 1.0),
            ("mystery", 5.0),
        ])
        .unwrap();
    assert_eq!(total.wh, 110.0);
    assert!(table().estimate_total([("mystery", 5.0)]).is_none());
}

#[test]
fn csv_without_required_columns_is_an_error() {
    assert!(EnergyTable::parse("model_name\nx\n").is_err());
}

#[test]
fn name_keys_normalize() {
    assert_eq!(name_key("claude-opus-4-5-20251101"), "opus 4.5");
    assert_eq!(name_key("Claude Sonnet 4.5"), "sonnet 4.5");
    assert_eq!(name_key("claude-opus-4-5[1m]"), "opus 4.5");
    assert_eq!(name_key("Gemini 3.7 Flash"), "gemini 3.7 flash");
}
