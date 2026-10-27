//! HTML has one escaping boundary. Persisted strings never become markup.
use crate::{
    Result,
    actions::{Command, MessageTemplate},
    journal::token,
    read::{Job, Queue},
};
use serde_json::Value;

pub const NAV: &[(&str, &str)] = &[
    ("/queue", "Help-desk queue"),
    ("/jobs", "Fix jobs"),
    ("/exclusions", "Quarantine"),
    ("/capabilities", "Client capabilities"),
    ("/deliveries", "Deliveries"),
    ("/shadow", "Shadow parity"),
    ("/accounting", "Decider accounting"),
    ("/health", "System health"),
    ("/requests", "Action audit"),
];
pub fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
pub fn page(title: &str, path: &str, content: &str, csrf: Option<&str>) -> String {
    let nav = NAV
        .iter()
        .map(|(url, label)| {
            format!(
                "<a href=\"{url}\"{}>{label}</a>",
                if *url == path {
                    " aria-current=\"page\""
                } else {
                    ""
                }
            )
        })
        .collect::<String>();
    let logout = csrf.map(|csrf| format!("<form action=\"/logout\" method=\"post\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><button class=\"quiet\">Sign out</button></form>",escape(csrf))).unwrap_or_default();
    // Content is inserted last so text resembling a template marker remains text.
    include_str!("../templates/base.html")
        .replace("{{TITLE}}", &escape(title))
        .replace("{{LOGOUT}}", &logout)
        .replace("{{NAV}}", &nav)
        .replace("{{CONTENT}}", content)
}
pub fn heading(title: &str, description: &str) -> String {
    format!(
        "<h1>{}</h1><p class=\"muted\">{}</p>",
        escape(title),
        escape(description)
    )
}
pub fn detail(label: &str, value: &Value) -> String {
    format!(
        "<details><summary>{}</summary><pre>{}</pre></details>",
        escape(label),
        escape(&serde_json::to_string_pretty(value).expect("JSON value serializes"))
    )
}
fn display(value: &Value) -> String {
    match value {
        Value::Null => "Not recorded".into(),
        Value::String(s) => escape(s),
        Value::Object(_) | Value::Array(_) => detail("View saved evidence", value),
        _ => escape(&value.to_string()),
    }
}
pub fn table(caption: &str, columns: &[(&str, &str)], rows: &[Value]) -> String {
    if rows.is_empty() {
        return format!("<p class=\"notice\">No {} recorded.</p>", escape(caption));
    }
    let mut html = format!(
        "<div class=\"table-scroll\" role=\"region\" aria-label=\"{}\" tabindex=\"0\"><table><caption>{} · up to 100 latest records</caption><thead><tr>",
        escape(caption),
        escape(caption)
    );
    for (_, label) in columns {
        html.push_str(&format!("<th scope=\"col\">{}</th>", escape(label)));
    }
    html.push_str("</tr></thead><tbody>");
    for row in rows {
        html.push_str("<tr>");
        for (key, _) in columns {
            html.push_str(&format!("<td>{}</td>", display(&row[key])));
        }
        html.push_str("</tr>");
    }
    html.push_str("</tbody></table></div>");
    html
}
pub fn form(command: &Command, label: &str, csrf: &str, api_enabled: bool) -> Result<String> {
    let key = token()?;
    let pending = command.api("", "").is_none();
    let explanation = if pending {
        "Saved for the control plane to validate. Check Action audit for the applied, rejected, deferred or uncertain result. A pending request has not performed the action."
    } else {
        "Applied through the configured admin API and recorded in the audit."
    };
    let disabled = if !pending && !api_enabled {
        " disabled"
    } else {
        ""
    };
    let extra = if !pending && !api_enabled {
        "<p class=\"warning\">Admin API is not configured.</p>"
    } else {
        ""
    };
    let preview = match command {
        Command::Message { template, .. } => {
            format!("<blockquote>{}</blockquote>", escape(template.text()))
        }
        _ => String::new(),
    };
    Ok(format!(
        "<form class=\"action\" action=\"/actions\" method=\"post\"><h3>{}</h3><p class=\"muted\">{explanation}</p>{preview}{extra}<input type=\"hidden\" name=\"csrf\" value=\"{}\"><input type=\"hidden\" name=\"request_key\" value=\"{key}\"><input type=\"hidden\" name=\"command\" value=\"{}\"><label for=\"note-{key}\">Reason for this action</label><textarea id=\"note-{key}\" name=\"note\" rows=\"2\" maxlength=\"1000\" required></textarea><button{disabled}>{}</button></form>",
        escape(label),
        escape(csrf),
        escape(&serde_json::to_string(command)?),
        escape(label)
    ))
}
pub fn queue(data: &Queue) -> String {
    let mut html = heading(
        "Help-desk queue",
        "Review reports, compare decisions, and follow the exact repair job. A recorded diagnosis is a recommendation.",
    );
    if data.diagnoses.is_none() {
        html.push_str("<p class=\"notice\">Diagnosis data is not connected yet.</p>");
    }
    if data.progress.is_none() {
        html.push_str("<p class=\"notice\">Job progress is not connected yet. Reports remain awaiting review.</p>");
    }
    if data.reports.is_empty() {
        html.push_str("<p>No reports yet. Reports from the viewer page and connected channels will appear here.</p>");
    }
    for report in &data.reports {
        let id = report["id"].as_u64().unwrap_or_default();
        html.push_str(&format!(
            "<article><h2><a href=\"/reports/{id}\">Report {id}</a></h2><p>{} · received {}</p>",
            display(&report["channel"]),
            display(&report["created"])
        ));
        if let Some(progress) = data
            .progress
            .as_ref()
            .and_then(|rows| rows.iter().find(|r| r["report_id"] == report["id"]))
        {
            html.push_str(&format!(
                "<p class=\"status\">{}</p>",
                display(&progress["state"])
            ));
            if let Some(job) = progress["job_id"].as_u64() {
                html.push_str(&format!("<a href=\"/jobs/{job}\">Open fix job {job}</a>"));
            }
        } else {
            html.push_str("<p class=\"status\">Awaiting review</p>");
        }
        if let Some(diagnosis) = data
            .diagnoses
            .as_ref()
            .and_then(|rows| rows.iter().find(|r| r["report_id"] == report["id"]))
        {
            html.push_str(&format!(
                "<p>Decider: {}</p>{}",
                display(&diagnosis["decider_kind"]),
                detail("Diagnosis", &diagnosis["decision"])
            ));
        }
        html.push_str(&detail("Viewer report", &report["response"]));
        html.push_str("</article>");
    }
    html
}
pub fn report(data: &Queue, csrf: &str, api_enabled: bool) -> Result<String> {
    let report = &data.reports[0];
    let id = report["id"].as_u64().unwrap_or_default();
    let mut html = queue(data);
    if let Some(rows) = &data.diagnoses {
        html.push_str("<h2>Evidence and decider comparison</h2>");
        html.push_str(&table(
            "diagnoses",
            &[
                ("resource", "Resource"),
                ("evidence_summary", "Evidence"),
                ("decision", "Selected decision"),
                ("comparison", "Decider comparison"),
            ],
            rows,
        ));
        if let Some(decided_at) = rows
            .first()
            .filter(|r| crate::read::replacement(&r["decision"]))
            .and_then(|r| r["decided_at"].as_u64())
            && !data
                .progress
                .as_ref()
                .is_some_and(|p| p.iter().any(|r| !r["job_id"].is_null()))
        {
            html.push_str(&form(
                &Command::Approve {
                    report: id,
                    decided_at,
                },
                "Request fix approval",
                csrf,
                api_enabled,
            )?);
            html.push_str(&form(
                &Command::Deny {
                    report: id,
                    decided_at,
                },
                "Request denial",
                csrf,
                api_enabled,
            )?);
        }
    }
    html.push_str("<h2>Report actions</h2><div class=\"actions\">");
    html.push_str(&form(
        &Command::Close { report: id },
        "Request report closure",
        csrf,
        api_enabled,
    )?);
    for (template, label) in [
        (MessageTemplate::Received, "Queue received message"),
        (MessageTemplate::NeedDetails, "Ask viewer for details"),
        (MessageTemplate::UnderReview, "Queue review update"),
    ] {
        html.push_str(&form(
            &Command::Message {
                report: id,
                template,
            },
            label,
            csrf,
            api_enabled,
        )?);
    }
    html.push_str("</div>");
    Ok(html)
}
pub fn job(data: &Job, csrf: &str, api_enabled: bool) -> Result<String> {
    let mut html = heading(
        &format!("Fix job {}", data.id),
        "The state is replayed from the event journal. Intent status describes execution, not proof that playback works.",
    );
    html.push_str(&format!(
        "<p class=\"status\">{} · revision {}</p>",
        escape(&data.state),
        data.revision
    ));
    html.push_str(&detail("Identity, constraints and budget", &data.summary));
    html.push_str("<h2>Timeline</h2>");
    html.push_str(&table(
        "journal events",
        &[
            ("revision", "Revision"),
            ("at", "Recorded at"),
            ("event", "Event / evidence"),
        ],
        &data.events,
    ));
    html.push_str("<h2>Effects and intents</h2>");
    html.push_str(&table(
        "effect intents",
        &[
            ("sequence", "Sequence"),
            ("status", "Status"),
            ("intent", "Intent"),
        ],
        &data.intents,
    ));
    if data.state == "AwaitingQualityApproval" {
        html.push_str(&form(
            &Command::LowerQuality {
                job: data.id,
                revision: data.revision,
            },
            "Request lower-quality approval",
            csrf,
            api_enabled,
        )?);
    }
    Ok(html)
}
