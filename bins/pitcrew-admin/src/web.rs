use crate::{
    Error, Result,
    actions::{AdminApi, Command},
    auth::Auth,
    config::{Config, validate},
    journal::Journal,
    read::Sources,
    render::{self, escape, form, heading, table},
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{
        Request, StatusCode,
        header::{self, HeaderMap, HeaderValue},
    },
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use subtle::ConstantTimeEq;

pub struct App {
    pub journal: Journal,
    sources: Sources,
    auth: Arc<dyn Auth>,
    api: Option<Arc<dyn AdminApi>>,
    config: Config,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
}
impl App {
    pub fn new(
        journal: Journal,
        sources: Sources,
        auth: Arc<dyn Auth>,
        api: Option<Arc<dyn AdminApi>>,
        config: Config,
        now: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Result<Self> {
        validate(&config)?;
        Ok(Self {
            journal,
            sources,
            auth,
            api,
            config,
            now,
        })
    }
    pub fn router(self: Arc<Self>) -> Router {
        Router::new().fallback(handler).with_state(self)
    }
    fn cookie(&self, name: &str, value: &str, max_age: u64) -> String {
        let secure = if self.config.public_origin.starts_with("https:") {
            "; Secure"
        } else {
            ""
        };
        format!("{name}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}")
    }
    async fn dispatch(&self, request: Request<Body>) -> Result<Response> {
        let (parts, body) = request.into_parts();
        let path = parts.uri.path();
        if parts.method == "GET" && path == "/style.css" {
            return Ok((
                [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                include_str!("../templates/style.css"),
            )
                .into_response());
        }
        let post = parts.method == "POST";
        if post
            && (parts.headers.get_all(header::ORIGIN).iter().count() != 1
                || parts
                    .headers
                    .get(header::ORIGIN)
                    .and_then(|v| v.to_str().ok())
                    != Some(&self.config.public_origin))
        {
            return Err(Error::Forbidden);
        }
        let bytes = to_bytes(body, 8192).await.map_err(|_| Error::Input)?;
        let fields = fields(&bytes)?;
        let now = (self.now)();
        match (parts.method.as_str(), path) {
            ("GET", "/") => Ok(html(
                "Sign in",
                path,
                &format!(
                    "{}<p>Use an administrator account allowed by this installation.</p><form method=\"post\" action=\"/auth/start\"><button>Sign in with Plex</button></form>",
                    heading("Owner sign-in", "Pit Crew help-desk operations")
                ),
                None,
            )),
            ("POST", "/auth/start") => {
                self.journal.reserve_login(now).await?;
                let pin = self.auth.start().await?;
                let state = self.journal.begin(pin.id, now).await?;
                let params = url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("clientID", &self.config.client_id)
                    .append_pair("code", &pin.code)
                    .append_pair("context[device][product]", "Pit Crew Admin")
                    .finish();
                let content = format!(
                    "{}<p><a class=\"text-action\" href=\"https://app.plex.tv/auth#?{}\" target=\"_blank\" rel=\"noopener noreferrer\">Open Plex sign-in in a new tab</a></p><form action=\"/auth/poll\" method=\"post\"><button>Finish sign-in</button></form>",
                    heading(
                        "Finish signing in",
                        "Authorize the PIN in Plex, then return here."
                    ),
                    escape(&params)
                );
                let mut response = html("Finish sign-in", path, &content, None);
                set_cookie(
                    &mut response,
                    &self.cookie("pitcrew_admin_login", &state, 600),
                )?;
                Ok(response)
            }
            ("POST", "/auth/poll") => {
                self.journal.reserve_login(now).await?;
                let state =
                    cookie(&parts.headers, "pitcrew_admin_login").ok_or(Error::Unauthorized)?;
                let pin = self.journal.pin(state, now).await?;
                let Some(account) = self.auth.poll(pin).await? else {
                    return Ok(html(
                        "Waiting for Plex",
                        path,
                        "<h1>Waiting for Plex</h1><p>Finish signing in on Plex, then check again.</p><form action=\"/auth/poll\" method=\"post\"><button>Check sign-in</button></form>",
                        None,
                    ));
                };
                if !self.config.admin_account_ids.contains(&account) {
                    return Err(Error::Forbidden);
                }
                let credentials = self.journal.complete(state, &account, now).await?;
                let mut response = redirect("/queue");
                set_cookie(
                    &mut response,
                    &self.cookie("pitcrew_admin_session", &credentials.token, 28_800),
                )?;
                set_cookie(&mut response, &self.cookie("pitcrew_admin_login", "", 0))?;
                Ok(response)
            }
            _ => {
                let token =
                    cookie(&parts.headers, "pitcrew_admin_session").ok_or(Error::Unauthorized)?;
                let session = self.journal.session(token, now).await?;
                if !self.config.admin_account_ids.contains(&session.account) {
                    return Err(Error::Forbidden);
                }
                if post
                    && !bool::from(
                        session
                            .csrf
                            .as_bytes()
                            .ct_eq(fields.get("csrf").map(String::as_bytes).unwrap_or_default()),
                    )
                {
                    return Err(Error::Forbidden);
                }
                if post {
                    match path {
                        "/logout" => {
                            self.journal.logout(token).await?;
                            let mut response = redirect("/");
                            set_cookie(
                                &mut response,
                                &self.cookie("pitcrew_admin_session", "", 0),
                            )?;
                            return Ok(response);
                        }
                        "/actions" => {
                            #[derive(Deserialize)]
                            #[serde(deny_unknown_fields)]
                            struct Input {
                                csrf: String,
                                request_key: String,
                                command: String,
                                note: String,
                            }
                            let input: Input =
                                serde_json::from_value(json!(fields)).map_err(|_| Error::Input)?;
                            // CSRF already checked before deserializing any command.
                            drop(input.csrf);
                            let command: Command =
                                serde_json::from_str(&input.command).map_err(|_| Error::Input)?;
                            if !self
                                .journal
                                .existing_request(
                                    &session.account,
                                    &input.request_key,
                                    &command,
                                    &input.note,
                                )
                                .await?
                            {
                                self.sources.validate_command(&command, now).await?;
                                if command.api("", "").is_some() && self.api.is_none() {
                                    return Err(Error::Input);
                                }
                                if self
                                    .journal
                                    .request(
                                        &session.account,
                                        &input.request_key,
                                        &command,
                                        &input.note,
                                        now,
                                    )
                                    .await?
                                    && command.api("", "").is_some()
                                {
                                    let outcome = self
                                        .api
                                        .as_ref()
                                        .ok_or(Error::Input)?
                                        .submit(&command, &input.request_key, &input.note)
                                        .await;
                                    self.journal
                                        .finish(&input.request_key, outcome, (self.now)())
                                        .await?;
                                }
                            }
                            return Ok(redirect("/requests"));
                        }
                        _ => return Err(Error::Missing),
                    }
                }
                if parts.method != "GET" {
                    return Err(Error::Missing);
                }
                let content = self.content(path, &session.csrf, now).await?;
                let (title, navigation) = if let Some(id) = path.strip_prefix("/reports/") {
                    (format!("Report {id}"), "/queue")
                } else if let Some(id) = path.strip_prefix("/jobs/") {
                    (format!("Fix job {id}"), "/jobs")
                } else {
                    let (_, title) = render::NAV
                        .iter()
                        .find(|(url, _)| *url == path)
                        .ok_or(Error::Missing)?;
                    (title.to_string(), path)
                };
                Ok(html(&title, navigation, &content, Some(&session.csrf)))
            }
        }
    }
    async fn content(&self, path: &str, csrf: &str, now: u64) -> Result<String> {
        let enabled = self.api.is_some();
        if let Some(id) = path.strip_prefix("/reports/") {
            return render::report(
                &self
                    .sources
                    .report(id.parse().map_err(|_| Error::Missing)?)
                    .await?,
                csrf,
                enabled,
            );
        }
        if let Some(id) = path.strip_prefix("/jobs/") {
            return render::job(
                &self
                    .sources
                    .job(id.parse().map_err(|_| Error::Missing)?)
                    .await?,
                csrf,
                enabled,
            );
        }
        let html = match path {
            "/queue" => render::queue(&self.sources.queue().await?),
            "/jobs" => {
                let mut html = heading(
                    "Fix jobs",
                    "Authoritative states replayed from the journal. Open a job for exact identity, evidence, effects and reserved budget.",
                );
                let jobs = self.sources.jobs().await?;
                if jobs.is_empty() {
                    html.push_str("<p>No fix jobs recorded. Reports can remain advice-only or wait for diagnosis.</p>");
                }
                for job in jobs {
                    html.push_str(&format!("<article><h2><a href=\"/jobs/{}\">Fix job {}</a></h2><p class=\"status\">{} · revision {}</p>{}</article>",job.id,job.id,escape(&job.state),job.revision,render::detail("Identity and budget",&job.summary)));
                }
                html.push_str("<h2>Budget reservations</h2><p>Full job envelopes remain reserved while ownership is active, including older jobs. This view is bounded to the latest 100 reservations; it is not a remaining-budget total.</p>");
                html.push_str(&table(
                    "budget reservations",
                    &[
                        ("job", "Job"),
                        ("viewer", "Viewer reference"),
                        ("units", "Reserved grabs"),
                        ("at", "Reserved at"),
                        ("active", "Active ownership"),
                    ],
                    &self.sources.budgets(now).await?,
                ));
                html
            }
            "/exclusions" => {
                let rows = self.sources.exclusions().await?;
                let mut html = heading(
                    "Quarantine and exclusions",
                    "Review evidence before releasing a candidate. Pinned exclusions remain active until explicitly released.",
                );
                html.push_str(&table(
                    "exclusions",
                    &[
                        ("id", "Exclusion"),
                        ("client", "Client"),
                        ("spec", "Reason and evidence"),
                        ("expires_at", "Expires at"),
                        ("pinned", "Pinned"),
                        ("released_at", "Released at"),
                    ],
                    &rows,
                ));
                for row in rows {
                    if row["released_at"].is_null()
                        && (row["pinned"] == 1
                            || row["expires_at"].as_u64().is_some_and(|at| at > now))
                    {
                        let id = row["id"].as_u64().ok_or(Error::Storage)?;
                        html.push_str(&form(
                            &Command::Release { id },
                            &format!("Release exclusion {id}"),
                            csrf,
                            enabled,
                        )?);
                        if row["pinned"] != 1 {
                            html.push_str(&form(
                                &Command::Pin { id },
                                &format!("Request pin for exclusion {id}"),
                                csrf,
                                enabled,
                            )?);
                        }
                    }
                }
                html
            }
            "/capabilities" => {
                let rows = self.sources.capabilities().await?;
                let mut html = heading(
                    "Download-client capabilities",
                    "Version-bound evidence selects import policy. A reset returns capabilities to unknown and requires recertification.",
                );
                html.push_str(&table(
                    "capability ledgers",
                    &[
                        ("client", "Client"),
                        ("ledger", "Version, capabilities and evidence"),
                    ],
                    &rows,
                ));
                for row in rows {
                    let client = row["client"].as_str().ok_or(Error::Storage)?;
                    html.push_str(&form(
                        &Command::Reset {
                            client: client.into(),
                        },
                        &format!("Reset capabilities: {client}"),
                        csrf,
                        enabled,
                    )?);
                }
                html
            }
            "/deliveries" => {
                let rows = self.sources.deliveries().await?;
                let mut html = heading(
                    "Deliveries",
                    "Uncertain means acceptance could not be confirmed. An explicit resend reuses the immutable recipient and message and can cause a duplicate if the first send arrived.",
                );
                html.push_str(&table(
                    "notification deliveries",
                    &[
                        ("job", "Job"),
                        ("sequence", "Sequence"),
                        ("status", "Delivery status"),
                        ("payload", "Saved message"),
                    ],
                    &rows,
                ));
                for row in rows {
                    if row["status"] == "uncertain" {
                        let job = row["job"].as_u64().ok_or(Error::Storage)?;
                        let sequence = row["sequence"].as_u64().ok_or(Error::Storage)?;
                        html.push_str(&form(
                            &Command::Resend { job, sequence },
                            &format!("Resend job {job} message {sequence}"),
                            csrf,
                            enabled,
                        )?);
                    }
                }
                if let Some(lifecycle) = self.sources.lifecycle_deliveries().await? {
                    html.push_str(&table(
                        "report follow-ups",
                        &[
                            ("report_id", "Report"),
                            ("job_id", "Job"),
                            ("state", "Delivery status"),
                            ("payload", "Saved message"),
                        ],
                        &lifecycle,
                    ));
                    html.push_str("<p class=\"muted\">Shadowed messages were recorded without sending. Uncertain follow-ups are held for review and are never automatically resent.</p>");
                }
                html
            }
            "/shadow" => {
                let records = self.sources.observations().await?;
                let mut html = heading(
                    "Shadow parity",
                    "Saved comparisons only. An absent or uncorrelated observation cannot establish agreement or authorize a live change.",
                );
                for (source, title) in [
                    ("agent:shadow", "Agent shadow"),
                    ("diff", "External tool versus engine"),
                    ("arbiter:recorded", "Import arbiter shadow"),
                    ("decider:comparison", "Decider comparison"),
                ] {
                    html.push_str(&format!("<h2>{title}</h2>"));
                    let rows: Vec<_> = records
                        .iter()
                        .filter(|r| r["source"] == source)
                        .cloned()
                        .collect();
                    html.push_str(&table(
                        title,
                        &[("at", "Observed at"), ("payload", "Saved comparison")],
                        &rows,
                    ));
                }
                html
            }
            "/accounting" => {
                let records = self.sources.observations().await?;
                let usage: Vec<_> = records
                    .iter()
                    .filter(|r| r["source"] == "decider:usage")
                    .map(|r| r["payload"].clone())
                    .collect();
                let totals = crate::accounting::summarize(&usage, &self.config.rates)?;
                let mut html = heading(
                    "Decider accounting",
                    "Latest recorded calls only. Estimates use rates from the local admin configuration, in millionths of USD. They are not invoices. Missing usage and unrecorded requests are not free.",
                );
                if totals.is_empty() {
                    html.push_str("<p class=\"notice\">Durable call accounting is not available yet. Process counters alone do not establish a complete history.</p>");
                }
                html.push_str(&table(
                    "recorded decider usage",
                    &[
                        ("provider", "Provider"),
                        ("calls", "Calls"),
                        ("calls_with_usage", "Usage recorded"),
                        ("calls_without_usage", "Usage unknown"),
                        ("input_tokens", "Input tokens"),
                        ("output_tokens", "Output tokens"),
                        ("cache_creation_tokens", "Cache write tokens"),
                        ("cache_read_tokens", "Cache read tokens"),
                        (
                            "estimated_known_microusd",
                            "Known-usage estimate (micro USD)",
                        ),
                    ],
                    &totals,
                ));
                html
            }
            "/health" => {
                let records = self.sources.observations().await?;
                let mut html = heading(
                    "System health",
                    "Last observed reachability and mount/watchdog policy. This page makes no upstream probes. Observations older than five minutes, or dated in the future, are stale.",
                );
                let rows = health_rows(&records, now);
                if rows.is_empty() {
                    html.push_str("<p class=\"notice\">Agent, arr and Plex reachability are unknown. No health observations have been recorded.</p>");
                }
                html.push_str(&table(
                    "health observations",
                    &[
                        ("at", "Observed at"),
                        ("freshness", "Freshness"),
                        ("payload", "Reachability and policy"),
                    ],
                    &rows,
                ));
                html
            }
            "/requests" => {
                let mut html = heading(
                    "Action audit",
                    "Accepted means the admin API acknowledged the request. Pending controller requests have not been applied. Uncertain requests require reconciliation; refreshing never retries them.",
                );
                html.push_str(&table(
                    "admin requests",
                    &[
                        ("request_key", "Request"),
                        ("actor", "Administrator"),
                        ("command", "Action"),
                        ("note", "Reason"),
                        ("state", "Status"),
                        ("updated_at", "Updated at"),
                    ],
                    &self.journal.requests().await?,
                ));
                html.push_str("<h2>Append-only audit</h2>");
                html.push_str(&table(
                    "audit events",
                    &[
                        ("id", "Event"),
                        ("request_key", "Request"),
                        ("actor", "Administrator"),
                        ("audit_event", "State"),
                        ("at", "Recorded at"),
                    ],
                    &self.journal.audit().await?,
                ));
                html
            }
            _ => return Err(Error::Missing),
        };
        Ok(html)
    }
}
pub fn health_rows(records: &[Value], now: u64) -> Vec<Value> {
    records
        .iter()
        .filter(|r| r["source"] == "admin:health")
        .map(|r| {
            let mut row = r.clone();
            let fresh = r["at"]
                .as_u64()
                .is_some_and(|at| at <= now && now - at <= 300);
            row["freshness"] = json!(if fresh {
                "Recent observation"
            } else {
                "Stale / unknown"
            });
            row
        })
        .collect()
}
fn fields(body: &[u8]) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for (key, value) in url::form_urlencoded::parse(body) {
        if result
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return Err(Error::Input);
        }
    }
    Ok(result)
}
fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut found = None;
    for value in headers.get_all(header::COOKIE) {
        for part in value.to_str().ok()?.split(';').map(str::trim) {
            if let Some((key, value)) = part.split_once('=')
                && key == name
            {
                if found.is_some() {
                    return None;
                }
                found = Some(value);
            }
        }
    }
    found
}
fn set_cookie(response: &mut Response, value: &str) -> Result<()> {
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(value).map_err(|_| Error::Input)?,
    );
    Ok(())
}
fn redirect(path: &str) -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, path)]).into_response()
}
fn html(title: &str, path: &str, content: &str, csrf: Option<&str>) -> Response {
    Html(render::page(title, path, content, csrf)).into_response()
}
async fn handler(State(app): State<Arc<App>>, request: Request<Body>) -> Response {
    let mut response = match app.dispatch(request).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    };
    for (name, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        (
            "content-security-policy",
            "default-src 'none'; style-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
    ] {
        response
            .headers_mut()
            .insert(name, HeaderValue::from_static(value));
    }
    response
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::Input => StatusCode::BAD_REQUEST,
            Self::Conflict => StatusCode::CONFLICT,
            Self::Missing => StatusCode::NOT_FOUND,
            Self::Rate => StatusCode::TOO_MANY_REQUESTS,
            Self::Storage => StatusCode::SERVICE_UNAVAILABLE,
            Self::Upstream => StatusCode::BAD_GATEWAY,
        };
        (
            status,
            Html(render::page(
                "Unable to continue",
                "",
                &format!(
                    "<h1>Unable to continue</h1><p>{}</p><a class=\"text-action\" href=\"/\">Return to sign-in</a>",
                    escape(&self.to_string())
                ),
                None,
            )),
        )
            .into_response()
    }
}
