mod common;
use common::*;
use pitcrew_admin::{
    accounting,
    actions::{Command, MessageTemplate},
    render,
    web::health_rows,
};
use pitcrew_config::admin::AdminRates;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[test]
fn accounting_keeps_unknown_usage_separate_and_uses_all_explicit_rate_classes() {
    let rates = BTreeMap::from([(
        "anthropic".into(),
        AdminRates {
            input_microusd_per_million: 1_000_000,
            output_microusd_per_million: 2_000_000,
            cache_creation_microusd_per_million: 3_000_000,
            cache_read_microusd_per_million: 4_000_000,
        },
    )]);
    let records = [
        json!({"provider":"anthropic","usage":usage(10,20)}),
        json!({"provider":"anthropic","usage":usage(5,7)}),
        json!({"provider":"anthropic","usage":null}),
        json!({"provider":"jev","usage":usage(1,2)}),
    ];
    let result = accounting::summarize(&records, &rates).unwrap();
    assert_eq!(
        result,
        vec![
            json!({"provider":"anthropic","calls":3,"calls_with_usage":2,"calls_without_usage":1,"input_tokens":15,"output_tokens":27,"cache_creation_tokens":6,"cache_read_tokens":8,"estimated_known_microusd":119}),
            json!({"provider":"jev","calls":1,"calls_with_usage":1,"calls_without_usage":0,"input_tokens":1,"output_tokens":2,"cache_creation_tokens":3,"cache_read_tokens":4,"estimated_known_microusd":null})
        ]
    );
    assert!(accounting::summarize(&[], &rates).unwrap().is_empty());
    assert!(accounting::summarize(&[json!({"usage":usage(1,2)})], &rates).is_err());
    assert!(
        accounting::summarize(
            &[json!({"provider":"a","usage":{"input_tokens":1}})],
            &rates
        )
        .is_err()
    );
    let overflow = [
        json!({"provider":"a","usage":usage(u64::MAX,1)}),
        json!({"provider":"a","usage":usage(1,1)}),
    ];
    assert!(accounting::summarize(&overflow, &rates).is_err());
}
#[test]
fn calls_without_any_usage_never_display_a_zero_cost_estimate() {
    let rates = BTreeMap::from([(
        "anthropic".into(),
        AdminRates {
            input_microusd_per_million: 1,
            output_microusd_per_million: 2,
            cache_creation_microusd_per_million: 3,
            cache_read_microusd_per_million: 4,
        },
    )]);
    let rows =
        accounting::summarize(&[json!({"provider":"anthropic","usage":null})], &rates).unwrap();
    assert_eq!(rows[0]["calls_without_usage"], 1);
    assert_eq!(rows[0]["estimated_known_microusd"], Value::Null);
}
#[test]
fn health_freshness_never_infers_live_health_and_handles_future_or_unknown_clocks() {
    let records = vec![
        json!({"source":"admin:health","at":100,"payload":{"component":"agent","status":"up"}}),
        json!({"source":"admin:health","at":101,"payload":{}}),
        json!({"source":"admin:health","at":401,"payload":{}}),
        json!({"source":"admin:health","at":null,"payload":{}}),
        json!({"source":"diff","at":400,"payload":{}}),
    ];
    let rows = health_rows(&records, 400);
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0]["freshness"], "Recent observation");
    assert_eq!(rows[1]["freshness"], "Recent observation");
    assert_eq!(rows[2]["freshness"], "Stale / unknown");
    assert_eq!(rows[3]["freshness"], "Stale / unknown");
    assert_eq!(rows[0]["payload"], records[0]["payload"]);
    assert_eq!(
        health_rows(&records, 401)[0]["freshness"],
        "Stale / unknown"
    );
    assert!(health_rows(&[], 0).is_empty());
}
#[test]
fn every_persisted_character_is_escaped_in_content_attributes_and_details() {
    let untrusted = "<script>\" '& {{CONTENT}}";
    assert_eq!(
        render::escape(untrusted),
        "&lt;script&gt;&quot; &#39;&amp; {{CONTENT}}"
    );
    let html = render::page(
        untrusted,
        "/queue",
        &render::detail(untrusted, &json!({"payload":untrusted})),
        Some(untrusted),
    );
    assert!(!html.contains("<script>"));
    assert!(html.contains("&lt;script&gt;"));
    assert!(html.contains("{{CONTENT}}"));
    assert!(html.contains("aria-current=\"page\""));
    assert!(html.contains("Skip to content"));
    assert!(html.contains("<html lang=\"en\""));
    assert!(html.contains("tabindex=\"-1\""));
    let table = render::table(
        untrusted,
        &[("a", untrusted), ("b", "b"), ("c", "c"), ("d", "d")],
        &[json!({"a":untrusted,"b":true,"c":7,"d":null})],
    );
    assert!(!table.contains("<script>"));
    assert!(table.contains("scope=\"col\""));
    assert!(table.contains("<td>true</td>"));
    assert!(table.contains("<td>7</td>"));
    assert!(table.contains("Not recorded"));
    assert_eq!(
        render::table("requests", &[], &[]),
        "<p class=\"notice\">No requests recorded.</p>"
    );
}
#[test]
fn action_forms_have_unique_labels_csrf_nonce_reason_and_honest_pending_or_disabled_status() {
    let mut keys = std::collections::BTreeSet::new();
    for command in [
        Command::Release { id: 1 },
        Command::Close { report: 1 },
        Command::Message {
            report: 1,
            template: MessageTemplate::NeedDetails,
        },
    ] {
        let html = render::form(&command, "Review action", "csrf-proof", false).unwrap();
        let key = get_hidden(&html, "request_key");
        assert_eq!(key.len(), 64);
        assert!(keys.insert(key.clone()));
        assert!(html.contains(&format!("for=\"note-{key}\"")));
        assert!(html.contains(&format!("id=\"note-{key}\"")));
        assert!(html.contains("maxlength=\"1000\" required"));
        assert_eq!(get_hidden(&html, "csrf"), "csrf-proof");
        assert!(html.contains("method=\"post\""));
        if command.api("", "").is_some() {
            assert!(html.contains("button disabled"));
            assert!(html.contains("Admin API is not configured"));
        } else {
            assert!(!html.contains("button disabled"));
            assert!(html.contains("Check Action audit"));
            assert!(html.contains("A pending request has not performed the action"));
        }
    }
    let html = render::form(&Command::Release { id: 1 }, "Release", "proof", true).unwrap();
    assert!(!html.contains("button disabled"));
    assert!(html.contains("Applied through the configured admin API"));
}
#[tokio::test]
async fn all_pages_render_empty_and_populated_data_without_javascript_or_untrusted_markup() {
    let s = Setup::new(true).await;
    let (cookie, _) = s.session().await;
    for (path, _) in render::NAV {
        let (status, _, body) = call(&s.app, "GET", path, &cookie, "").await;
        assert_eq!(status, 200, "{path}: {body}");
        assert!(!body.contains("<script"));
        assert!(body.contains("Owner navigation"));
    }
    s.report().await;
    s.seams().await;
    s.job(1).await;
    s.exclusion().await;
    s.capabilities().await;
    for path in [
        "/queue",
        "/reports/1",
        "/jobs",
        "/jobs/1",
        "/exclusions",
        "/capabilities",
    ] {
        let (status, _, body) = call(&s.app, "GET", path, &cookie, "").await;
        assert_eq!(status, 200, "{path}: {body}");
        assert!(!body.contains("<script>alert"));
        assert!(body.contains("<main id=\"main\""));
    }
    let report = call(&s.app, "GET", "/reports/1", &cookie, "").await.2;
    assert!(report.contains("&lt;script&gt;"));
    assert!(report.contains("Request fix approval"));
    assert!(report.contains("Request denial"));
    assert!(!report.contains("NeedDetails"));
    assert!(report.contains("Please reply with the episode or movie"));
    assert!(report.contains("Evidence and decider comparison"));
    for path in [
        "/reports/unknown",
        "/reports/99",
        "/jobs/no",
        "/jobs/99",
        "/unknown",
    ] {
        assert_eq!(call(&s.app, "GET", path, &cookie, "").await.0, 404);
    }
    assert_eq!(call(&s.app, "PATCH", "/queue", &cookie, "").await.0, 404);
}
#[tokio::test]
async fn exclusion_controls_follow_active_expiry_pin_and_release_state() {
    let s = Setup::new(true).await;
    s.exclusion().await;
    let (cookie, _) = s.session().await;
    for (expires, pinned, released, release_visible, pin_visible) in [
        (101, 0, None, true, true),
        (100, 0, None, false, false),
        (99, 0, None, false, false),
        (99, 1, None, true, false),
        (101, 1, Some(90), false, false),
        (101, 0, Some(90), false, false),
    ] {
        sqlx::query("UPDATE release_exclusions SET expires_at=?,pinned=?,released_at=?")
            .bind(expires)
            .bind(pinned)
            .bind(released)
            .execute(&s.engine_sql)
            .await
            .unwrap();
        let page = call(&s.app, "GET", "/exclusions", &cookie, "").await;
        assert_eq!(page.0, 200);
        assert_eq!(page.2.contains("Release exclusion 1"), release_visible);
        assert_eq!(page.2.contains("Request pin for exclusion 1"), pin_visible);
    }
}
#[tokio::test]
async fn queue_correlates_report_decision_progress_and_saved_observation_views() {
    let s = Setup::new(true).await;
    s.report().await;
    s.seams().await;
    sqlx::query("INSERT INTO report_progress(report_id,job_id,state,updated_at,viewer_copy_key) VALUES(1,7,'working',100,'working')")
        .execute(&s.intake_sql)
        .await
        .unwrap();
    let (cookie, _) = s.session().await;
    let page = call(&s.app, "GET", "/queue", &cookie, "").await.2;
    assert!(page.contains("Open fix job 7"));
    assert!(page.contains("class=\"status\">working"));
    assert!(page.contains("Decider: rules"));
    assert!(page.contains("replace"));
    assert!(!page.contains("Awaiting review"));
    for (source, payload) in [
        (
            "admin:health",
            json!({"component":"agent-a","status":"up","policy":"watchdog observation"}),
        ),
        ("diff", json!({"fix":"synthetic-fix","correlated":false})),
        (
            "decider:usage",
            json!({"provider":"anthropic","usage":usage(12,34)}),
        ),
    ] {
        s.engine
            .ingest_observation(source, source, &payload, 90)
            .await
            .unwrap();
    }
    let health = call(&s.app, "GET", "/health", &cookie, "").await.2;
    assert!(health.contains("agent-a"));
    assert!(health.contains("Recent observation"));
    assert!(!health.contains("synthetic-fix"));
    let shadow = call(&s.app, "GET", "/shadow", &cookie, "").await.2;
    assert!(shadow.contains("synthetic-fix"));
    assert!(!shadow.contains("watchdog observation"));
    let accounting = call(&s.app, "GET", "/accounting", &cookie, "").await.2;
    assert!(accounting.contains("anthropic"));
    assert!(accounting.contains("<td>12</td>"));
    assert!(accounting.contains("<td>34</td>"));
    assert!(!accounting.contains("not available yet"));
}
#[tokio::test]
async fn actual_detail_routes_keep_parent_navigation_and_specific_document_titles() {
    let s = Setup::new(false).await;
    s.report().await;
    s.job(1).await;
    let (cookie, _) = s.session().await;
    for (path, parent, title) in [
        ("/reports/1", "/queue", "Report 1"),
        ("/jobs/1", "/jobs", "Fix job 1"),
        ("/health", "/health", "System health"),
    ] {
        let (status, _, body) = call(&s.app, "GET", path, &cookie, "").await;
        assert_eq!(status, 200);
        assert!(
            body.contains(&format!("<a href=\"{parent}\" aria-current=\"page\"")),
            "{path}"
        );
        assert!(
            body.contains(&format!("<title>{title} · Pit Crew</title>")),
            "{path}"
        );
    }
}
#[test]
fn dark_palette_has_text_focus_and_control_contrast_and_tablet_layout_rules() {
    fn lum(hex: &str) -> f64 {
        let rgb = (0..3)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap() as f64 / 255.)
            .map(|v| {
                if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            })
            .collect::<Vec<_>>();
        rgb[0] * 0.2126 + rgb[1] * 0.7152 + rgb[2] * 0.0722
    }
    fn ratio(a: &str, b: &str) -> f64 {
        let a = lum(a);
        let b = lum(b);
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }
    let css = include_str!("../templates/style.css");
    for foreground in ["eef1eb", "bdc8b8", "b8dca9"] {
        for background in ["191e19", "242b24"] {
            assert!(ratio(foreground, background) >= 4.5);
        }
    }
    assert!(ratio("162411", "b8dca9") >= 4.5);
    assert!(ratio("f1bb70", "242b24") >= 3.0);
    assert!(css.contains("color-scheme:dark"));
    assert!(css.contains("max-width:60rem"));
    assert!(css.contains("min-height:44px"));
    assert!(css.contains("prefers-reduced-motion"));
    assert!(css.contains("overflow:auto"));
    assert!(css.contains(":focus-visible"));
    assert!(css.contains("resize:vertical"));
    // Textarea border is the only boundary against the page; it needs 3:1.
    let border = css
        .split("--control:")
        .nth(1)
        .unwrap_or("#4c5848;")
        .trim_start_matches('#')
        .split(';')
        .next()
        .unwrap();
    assert!(ratio(border, "191e19") >= 3.0);
}
fn normalized(mut html: String) -> String {
    while let Some((_, tail)) = html.split_once("name=\"request_key\" value=\"") {
        let key = tail.split('"').next().unwrap().to_owned();
        if key == "REQUEST-KEY" {
            break;
        }
        html = html.replace(&key, "REQUEST-KEY");
        // Only one action in the snapshot below.
    }
    html
}
#[test]
fn native_template_snapshot_includes_table_evidence_and_pending_action() {
    let content = format!(
        "{}{}{}",
        render::heading("Report 7", "Saved evidence from a synthetic report."),
        render::table(
            "diagnoses",
            &[("provider", "Decider"), ("decision", "Decision")],
            &[
                json!({"provider":"rules","decision":{"action":"replace","evidence":"decode failed"}})
            ]
        ),
        render::form(
            &Command::Close { report: 7 },
            "Request report closure",
            "CSRF",
            false
        )
        .unwrap()
    );
    let html = normalized(render::page("Report 7", "/queue", &content, Some("CSRF")));
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/report.html");
    if std::env::var_os("UPDATE_ADMIN_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &html).unwrap();
    }
    assert_eq!(html, std::fs::read_to_string(path).unwrap());
}
