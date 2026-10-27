//! Server-rendered intake. Accepted reports are observations; this process never
//! starts repair effects or announces readiness without engine verification.
pub mod auth;
mod catalog;
mod wizard;
use auth::{Auth, AuthError};
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
use catalog::ViewerCatalog;
use pitcrew_domain::channels::{ChannelIdentity, IncomingMessage};
use pitcrew_intake::{AuthorizedViewer, CatalogItem, IntakeOutcome, IntakeService, IssueKind};
use pitcrew_intake_store::{Session, Store};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    sync::Arc,
};
use subtle::ConstantTimeEq;
use url::Url;

const PICKER_PAGE_SIZE: usize = 100;
use wizard::{WizardIssue, question};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub database: String,
    #[serde(default = "bind_default")]
    pub bind: SocketAddr,
    pub public_origin: String,
    #[serde(default = "auth_default")]
    pub plex_auth_url: String,
    pub plex_server_url: String,
    pub plex_server_id: String,
    pub client_id: String,
    #[serde(default)]
    pub catalog: Vec<CatalogItem>,
    #[serde(default)]
    pub catalog_snapshot_file: Option<String>,
}
fn bind_default() -> SocketAddr {
    ([127, 0, 0, 1], 7401).into()
}
fn auth_default() -> String {
    "https://plex.tv/".into()
}
impl Config {
    pub fn parse(source: &str) -> Result<Self, &'static str> {
        let config: Self = serde_json::from_str(source).map_err(|_| "invalid web configuration")?;
        let url = Url::parse(&config.public_origin).map_err(|_| "invalid public origin")?;
        if url.origin().ascii_serialization() != config.public_origin
            || !(url.scheme() == "https"
                || url.scheme() == "http"
                    && matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "localhost")))
        {
            return Err("public origin requires HTTPS (HTTP is allowed only on loopback)");
        }
        let unique: BTreeSet<_> = config.catalog.iter().map(|c| &c.id).collect();
        if unique.len() != config.catalog.len()
            || config
                .catalog
                .iter()
                .any(|c| c.id.is_empty() || c.library_id.is_empty())
        {
            return Err("catalog requires unique item IDs and library IDs");
        }
        Ok(config)
    }
}
pub struct App {
    pub store: Store,
    auth: Arc<dyn Auth>,
    catalog: Vec<CatalogItem>,
    catalog_snapshot_file: Option<String>,
    origin: String,
    client_id: String,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
}
impl App {
    pub fn new(
        store: Store,
        auth: Arc<dyn Auth>,
        config: Config,
        now: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        Self {
            store,
            auth,
            catalog: config.catalog,
            catalog_snapshot_file: config.catalog_snapshot_file,
            origin: config.public_origin,
            client_id: config.client_id,
            now,
        }
    }
    pub fn router(self: Arc<Self>) -> Router {
        Router::new().fallback(handler).with_state(self)
    }
    async fn session(&self, headers: &HeaderMap) -> Result<Session, WebError> {
        let token = cookie(headers, "pitcrew_session").ok_or(WebError::Login)?;
        self.store
            .session(token, (self.now)())
            .await?
            .ok_or(WebError::Login)
    }
    async fn viewer(&self, session: &Session) -> Result<AuthorizedViewer, WebError> {
        let token = self
            .store
            .account_token(&session.account_id)
            .await?
            .ok_or(WebError::Login)?;
        Ok(AuthorizedViewer {
            account_id: session.account_id.clone(),
            library_ids: self.auth.access(&token).await?,
        })
    }
    fn cookie(&self, name: &str, value: &str, max_age: u64) -> String {
        let secure = if self.origin.starts_with("https:") {
            "; Secure"
        } else {
            ""
        };
        format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}")
    }
    async fn dispatch(&self, request: Request<Body>) -> Result<Response, WebError> {
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
            && parts
                .headers
                .get(header::ORIGIN)
                .and_then(|v| v.to_str().ok())
                != Some(&self.origin)
        {
            return Err(WebError::Forbidden);
        }
        let body = to_bytes(body, 8192).await.map_err(|_| WebError::Input)?;
        let form: BTreeMap<String, String> =
            url::form_urlencoded::parse(&body).into_owned().collect();
        let query: BTreeMap<String, String> =
            url::form_urlencoded::parse(parts.uri.query().unwrap_or_default().as_bytes())
                .into_owned()
                .collect();
        match (parts.method.as_str(), path) {
            ("GET", "/") => Ok(page("Sign in", include_str!("../templates/login.html"))),
            ("POST", "/auth/start") => {
                if !self
                    .store
                    .reserve_rate("login", "global", 10, 60, (self.now)())
                    .await?
                {
                    return Err(WebError::Rate);
                }
                let pin = self.auth.create_pin().await?;
                let state = self.store.begin_login(pin.id, (self.now)()).await?;
                let params = url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("clientID", &self.client_id)
                    .append_pair("code", &pin.code)
                    .append_pair("context[device][product]", "Pit Crew")
                    .finish();
                let url = format!("https://app.plex.tv/auth#?{params}");
                let html = include_str!("../templates/pending.html")
                    .replace("{{AUTH_URL}}", &escape(&url));
                let mut result = page("Finish signing in", &html);
                set_cookie(&mut result, &self.cookie("pitcrew_login", &state, 600))?;
                Ok(result)
            }
            ("POST", "/auth/poll") => {
                if !self
                    .store
                    .reserve_rate("poll", "global", 100, 60, (self.now)())
                    .await?
                {
                    return Err(WebError::Rate);
                }
                let state = cookie(&parts.headers, "pitcrew_login").ok_or(WebError::Login)?;
                let pin = self
                    .store
                    .login_pin(state, (self.now)())
                    .await?
                    .ok_or(WebError::Login)?;
                let Some(account) = self.auth.poll(pin).await? else {
                    return Ok(page(
                        "Waiting for Plex",
                        "<h1>Waiting for Plex</h1><p>Finish signing in on Plex, then continue.</p><form action=\"/auth/poll\" method=\"post\"><button>Check sign-in</button></form>",
                    ));
                };
                self.auth.access(&account.token).await?;
                let session = self
                    .store
                    .complete_login(state, &account.id, &account.token, (self.now)())
                    .await?;
                let mut result = redirect("/report");
                set_cookie(
                    &mut result,
                    &self.cookie("pitcrew_session", &session.token, 604800),
                )?;
                set_cookie(&mut result, &self.cookie("pitcrew_login", "", 0))?;
                Ok(result)
            }
            ("GET", "/report" | "/my-reports" | "/link")
            | ("POST", "/report" | "/link" | "/logout") => {
                let session = self.session(&parts.headers).await?;
                if post {
                    let csrf = form.get("csrf").ok_or(WebError::Forbidden)?;
                    if !bool::from(csrf.as_bytes().ct_eq(session.csrf.as_bytes())) {
                        return Err(WebError::Forbidden);
                    }
                }
                if path == "/logout" {
                    self.store
                        .logout(cookie(&parts.headers, "pitcrew_session").ok_or(WebError::Login)?)
                        .await?;
                    let mut result = redirect("/");
                    set_cookie(&mut result, &self.cookie("pitcrew_session", "", 0))?;
                    return Ok(result);
                }
                let viewer = self.viewer(&session).await?;
                let catalog = ViewerCatalog::load(
                    self.catalog_snapshot_file.as_deref(),
                    &self.catalog,
                    &viewer,
                    (self.now)(),
                )
                .await?;
                let visible: Vec<_> = catalog.items.iter().collect();
                let content = match (post, path) {
                    (false, "/report") => {
                        if let Some(id) = query.get("show") {
                            let item = visible
                                .iter()
                                .find(|c| &c.id == id)
                                .ok_or(WebError::Forbidden)?;
                            if item.episode.is_none() || query.contains_key("item") {
                                return Err(WebError::Input);
                            }
                            let page = query
                                .get("page")
                                .map(|v| v.parse::<usize>())
                                .transpose()
                                .map_err(|_| WebError::Input)?
                                .unwrap_or(0);
                            episode_picker(&visible, item, &catalog, page)?
                        } else if let Some(id) = query.get("item") {
                            let item = visible
                                .iter()
                                .find(|c| &c.id == id)
                                .ok_or(WebError::Forbidden)?;
                            wizard(item, &session.csrf, query.get("issue").map(String::as_str))?
                        } else {
                            picker(
                                &visible,
                                query.get("q").map(String::as_str).unwrap_or_default(),
                                &catalog,
                            )
                        }
                    }
                    (true, "/report") => {
                        let item = form.get("item").ok_or(WebError::Input)?;
                        let selected = visible
                            .iter()
                            .find(|c| &c.id == item)
                            .ok_or(WebError::Forbidden)?;
                        let nonce = form
                            .get("nonce")
                            .filter(|n| !n.is_empty() && n.len() <= 128)
                            .ok_or(WebError::Input)?;
                        let kind: WizardIssue = serde_json::from_value(serde_json::Value::String(
                            form.get("issue").cloned().ok_or(WebError::Input)?,
                        ))
                        .map_err(|_| WebError::Input)?;
                        if kind == WizardIssue::WrongEpisode && selected.episode.is_none() {
                            return Err(WebError::Input);
                        }
                        let answer = form.get("answer").cloned().unwrap_or_default();
                        let text = form.get("text").cloned().unwrap_or_default();
                        if !kind.valid(&answer, &text) {
                            return Err(WebError::Input);
                        }
                        let message = IncomingMessage {
                            sender: ChannelIdentity {
                                channel_id: "web".into(),
                                platform_user_id: session.account_id.clone(),
                            },
                            message_id: format!("{}:{nonce}", session.account_id),
                            conversation_id: format!("web:{}", session.account_id),
                            text,
                            received_at: (self.now)(),
                        };
                        let mut result = IntakeService.handle_selected(
                            &message,
                            Some(&viewer),
                            &catalog.items,
                            item,
                            &[kind.kind().unwrap_or(IssueKind::Playback)],
                        );
                        if result.outcome != IntakeOutcome::Report {
                            return Err(WebError::Forbidden);
                        }
                        // Review/advice retain parser authorization and suspicious-text checks,
                        // but expose no repair symptom to a downstream consumer.
                        if kind.kind().is_none() {
                            result.issues[0].kinds.clear();
                        }
                        let mut encoded =
                            serde_json::to_value(&result).map_err(|_| WebError::Input)?;
                        encoded["wizard"] = serde_json::json!({"issue":kind,"answer":answer,"details":message.text,"disposition":kind.disposition(&answer)});
                        let inserted = self
                            .store
                            .persist_web_report(
                                &session.account_id,
                                nonce,
                                &encoded,
                                message.received_at,
                            )
                            .await?;
                        if inserted {
                            wizard::receipt(kind, &answer).into()
                        } else {
                            "<h1>Report already received</h1><p>Your earlier submission is unchanged. Check My reports for its current status.</p><a href=\"/my-reports\">View my reports</a>".into()
                        }
                    }
                    (false, "/my-reports") => {
                        let rows = self.store.reports(&session.account_id).await?;
                        let progress: BTreeMap<_, _> = self
                            .store
                            .report_progress(&session.account_id)
                            .await?
                            .into_iter()
                            .map(|p| (p.report_id, p.state))
                            .collect();
                        let mut html = String::from("<h1>My reports</h1>");
                        if rows.is_empty() {
                            html.push_str("<p>No reports yet. If something goes wrong, choose a title and tell us what happened.</p><a href=\"/report\">Report a problem</a>");
                        }
                        for row in rows {
                            let Some(issues) =
                                row.response.get("issues").and_then(|v| v.as_array())
                            else {
                                continue;
                            };
                            for issue in issues {
                                let Some(item) = visible.iter().find(|c| {
                                    Some(c.id.as_str())
                                        == issue.get("item_id").and_then(|v| v.as_str())
                                }) else {
                                    continue;
                                };
                                let kinds: Vec<IssueKind> = serde_json::from_value(
                                    issue.get("kinds").cloned().unwrap_or_default(),
                                )
                                .unwrap_or_default();
                                let wizard_issue: Option<WizardIssue> = row
                                    .response
                                    .get("wizard")
                                    .and_then(|w| w.get("issue"))
                                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                                // Only the account-scoped engine projection can claim a
                                // repair outcome. Raw copy keys and intake text are never HTML.
                                let status = if row.response["wizard"]["disposition"].as_str()
                                    == Some("self_resolved")
                                {
                                    "Self-resolved · reported by you"
                                } else if let Some(state) = progress.get(&row.id) {
                                    match state.as_str() {
                                        "needs_review" => {
                                            "Needs review · an admin needs to check this report"
                                        }
                                        "advice" => {
                                            "Advice available · no automatic repair was started"
                                        }
                                        "working" => {
                                            "Working on it · the repair has not been verified yet"
                                        }
                                        "fixed" => "Fixed · the replacement passed playback checks",
                                        "could_not_fix" => {
                                            "Could not fix automatically · an admin needs to help"
                                        }
                                        _ => "Received · awaiting review",
                                    }
                                } else {
                                    match row.response["wizard"]["disposition"].as_str() {
                                        Some("advice") => "Advice provided",
                                        _ => "Received · awaiting review",
                                    }
                                };
                                let symptoms = kinds
                                    .into_iter()
                                    .map(issue_label)
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                let symptoms =
                                    wizard_issue.map(WizardIssue::label).unwrap_or(&symptoms);
                                html.push_str(&format!("<article><h2>{}</h2><p>{}</p><p>{}</p><p>{status}</p><small>Report {}</small></article>",escape(&item_title(item)),escape(&item.library_name),escape(symptoms),row.id));
                            }
                        }
                        html
                    }
                    (false, "/link") => include_str!("../templates/link.html")
                        .replace("{{CSRF}}", &escape(&session.csrf)),
                    (true, "/link") => {
                        let code = self
                            .store
                            .issue_link(&session.account_id, (self.now)())
                            .await?;
                        format!(
                            "<h1>Link your chat account</h1><p>Send this code privately to the configured Pit Crew bot using <code>link CODE</code>. It expires in 10 minutes and can be used once.</p><p class=\"code\">{}</p><p>Only share it with your own chat account.</p>",
                            escape(&code)
                        )
                    }
                    _ => return Err(WebError::Input),
                };
                let logout = format!(
                    "<form class=\"logout\" action=\"/logout\" method=\"post\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><button>Sign out</button></form>",
                    escape(&session.csrf)
                );
                Ok(page("Pit Crew", &(content + &logout)))
            }
            _ => Ok((
                StatusCode::NOT_FOUND,
                Html("Page not found. <a href=\"/\">Return to Pit Crew</a>"),
            )
                .into_response()),
        }
    }
}
async fn handler(State(app): State<Arc<App>>, request: Request<Body>) -> Response {
    let mut response = match app.dispatch(request).await {
        Ok(response) => response,
        Err(e) => e.into_response(),
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
#[derive(Debug)]
enum WebError {
    Login,
    Forbidden,
    Input,
    Storage,
    Catalog,
    Rate,
    Auth(AuthError),
}
impl From<pitcrew_intake_store::Error> for WebError {
    fn from(error: pitcrew_intake_store::Error) -> Self {
        match error {
            pitcrew_intake_store::Error::RateLimited => Self::Rate,
            _ => Self::Storage,
        }
    }
}
impl From<AuthError> for WebError {
    fn from(value: AuthError) -> Self {
        Self::Auth(value)
    }
}
impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::Rate => (
                StatusCode::TOO_MANY_REQUESTS,
                "Too many requests. Please try again later.",
            ),
            Self::Login => (
                StatusCode::UNAUTHORIZED,
                "Please sign in with Plex to continue.",
            ),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                "This request could not be authorized. Refresh the page and try again.",
            ),
            Self::Input => (
                StatusCode::BAD_REQUEST,
                "Check the report details and try again.",
            ),
            Self::Catalog => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The shared catalog is temporarily unavailable. Please try again shortly.",
            ),
            Self::Storage => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Your request could not be saved. Please try again.",
            ),
            Self::Auth(AuthError::Denied) => (
                StatusCode::FORBIDDEN,
                "Your Plex account does not have access to this server.",
            ),
            Self::Auth(_) => (
                StatusCode::BAD_GATEWAY,
                "Plex could not verify access. Please try again shortly.",
            ),
        };
        let mut response = page(
            "Please try again",
            &format!(
                "<h1>Unable to continue</h1><p>{message}</p><a href=\"/\">Return to sign-in</a>"
            ),
        );
        *response.status_mut() = status;
        response
    }
}
fn set_cookie(response: &mut Response, value: &str) -> Result<(), WebError> {
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(value).map_err(|_| WebError::Input)?,
    );
    Ok(())
}
fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|part| {
            let (key, value) = part.split_once('=')?;
            (key == name).then_some(value)
        })
}
fn redirect(path: &str) -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, path)]).into_response()
}
fn page(title: &str, content: &str) -> Response {
    Html(
        include_str!("../templates/base.html")
            .replace("{{TITLE}}", &escape(title))
            .replace("{{CONTENT}}", content),
    )
    .into_response()
}
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn item_title(item: &CatalogItem) -> String {
    match (item.season, item.episode) {
        (Some(s), Some(e)) => format!("{} · S{s} E{e}", item.title),
        _ => item.title.clone(),
    }
}
fn issue_label(kind: IssueKind) -> &'static str {
    match kind {
        IssueKind::Playback => "Playback",
        IssueKind::Buffering => "Buffering",
        IssueKind::Subtitles => "Subtitles",
        IssueKind::AudioLanguage => "Audio language",
        IssueKind::NoAudio => "No sound",
        IssueKind::WrongEpisode => "Wrong episode",
        IssueKind::Quality => "Picture quality",
    }
}
fn feed(heading: &str, items: &[CatalogItem], empty: &str, whole_shows: bool) -> String {
    let mut html = format!("<section aria-label=\"{heading}\"><h2>{heading}</h2>");
    if items.is_empty() {
        html.push_str(&format!("<p class=\"hint\">{empty}</p>"));
    } else {
        html.push_str("<ul class=\"titles\">");
        for item in items {
            let is_show = whole_shows && item.episode.is_some();
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair(if is_show { "show" } else { "item" }, &item.id)
                .finish();
            html.push_str(&format!(
                "<li><a href=\"/report?{}\"><strong>{}</strong><span>{}</span></a></li>",
                escape(&query),
                escape(&if is_show {
                    format!("{} · Whole show", item.title)
                } else {
                    item_title(item)
                }),
                escape(&item.library_name)
            ));
        }
        html.push_str("</ul>");
    }
    html.push_str("</section>");
    html
}
fn picker(items: &[&CatalogItem], query: &str, catalog: &ViewerCatalog) -> String {
    let mut html = include_str!("../templates/picker.html").replace("{{QUERY}}", &escape(query));
    if query.is_empty() {
        html.push_str(&feed(
            "Recently played",
            &catalog.recent,
            "No recent plays are available for this account. Search your libraries below.",
            false,
        ));
        html.push_str(&feed(
            "Your watchlist",
            &catalog.watchlist,
            "No watchlist titles are available in your shared libraries. Search below.",
            true,
        ));
    }
    html.push_str("<h2>Shared libraries</h2><ul class=\"titles\" aria-label=\"Titles\">");
    let found: Vec<_> = items
        .iter()
        .filter(|c| c.title.to_lowercase().contains(&query.to_lowercase()))
        .take(PICKER_PAGE_SIZE + 1)
        .collect();
    if found.is_empty() {
        html.push_str("<li>No matching titles in your shared libraries. Try another title.</li>");
    }
    let more = found.len() > PICKER_PAGE_SIZE;
    let mut shows = BTreeSet::new();
    for item in found.into_iter().take(PICKER_PAGE_SIZE) {
        if item.episode.is_some() && shows.insert((&item.library_id, catalog.series(item))) {
            let link = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("show", &item.id)
                .finish();
            html.push_str(&format!("<li><a href=\"/report?{}\"><strong>{} · Whole show</strong><span>{} · Choose the affected episode</span></a></li>", escape(&link), escape(&item.title), escape(&item.library_name)));
        }
        let link = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("item", &item.id)
            .finish();
        html.push_str(&format!(
            "<li><a href=\"/report?{}\"><strong>{}</strong><span>{}</span></a></li>",
            escape(&link),
            escape(&item_title(item)),
            escape(&item.library_name)
        ));
    }
    html.push_str("</ul>");
    if more {
        html.push_str("<p>Showing the first 100 matches. Refine your search to find another title. For a show, choose Whole show to browse every episode.</p>");
    }
    html
}
fn episode_picker(
    items: &[&CatalogItem],
    show: &CatalogItem,
    catalog: &ViewerCatalog,
    page: usize,
) -> Result<String, WebError> {
    let mut episodes: Vec<_> = items
        .iter()
        .copied()
        .filter(|item| {
            catalog.series(item) == catalog.series(show)
                && item.library_id == show.library_id
                && item.episode.is_some()
        })
        .collect();
    episodes.sort_by_key(|item| (item.season, item.episode));
    let start = page.checked_mul(PICKER_PAGE_SIZE).ok_or(WebError::Input)?;
    if start >= episodes.len() {
        return Err(WebError::Input);
    }
    let has_next = episodes
        .get(start.saturating_add(PICKER_PAGE_SIZE))
        .is_some();
    let mut html = format!(
        "<p><a href=\"/report\">Choose a different title</a></p><h1>Which episode is affected?</h1><p><strong>{}</strong><br>{}</p><p>For a whole-show problem, choose one episode where you saw it happen. We need an exact episode before checking the file.</p><ul class=\"titles\" aria-label=\"Episodes\">",
        escape(&show.title),
        escape(&show.library_name)
    );
    for item in episodes.into_iter().skip(start).take(PICKER_PAGE_SIZE) {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("item", &item.id)
            .finish();
        html.push_str(&format!(
            "<li><a href=\"/report?{}\">{}</a></li>",
            escape(&query),
            escape(&item_title(item))
        ));
    }
    html.push_str("</ul><nav aria-label=\"Episode pages\">");
    for (shown, number, label) in [
        (page > 0, page.saturating_sub(1), "Previous episodes"),
        (has_next, page.saturating_add(1), "Next episodes"),
    ] {
        if shown {
            let query = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("show", &show.id)
                .append_pair("page", &number.to_string())
                .finish();
            html.push_str(&format!(
                "<a href=\"/report?{}\">{label}</a>",
                escape(&query)
            ));
        }
    }
    html.push_str("</nav>");
    Ok(html)
}
fn wizard(item: &CatalogItem, csrf: &str, issue: Option<&str>) -> Result<String, WebError> {
    let Some(issue) = issue else {
        let episode_option = if item.episode.is_some() {
            "<option value=\"wrong_episode\">It is the wrong episode</option>"
        } else {
            ""
        };
        return Ok(include_str!("../templates/problem.html")
            .replace("{{EPISODE_OPTION}}", episode_option)
            .replace("{{ITEM}}", &escape(&item.id))
            .replace("{{TITLE}}", &escape(&item_title(item)))
            .replace("{{LIBRARY}}", &escape(&item.library_name)));
    };
    let kind: WizardIssue = serde_json::from_value(serde_json::Value::String(issue.into()))
        .map_err(|_| WebError::Input)?;
    if kind == WizardIssue::WrongEpisode && item.episode.is_none() {
        return Err(WebError::Input);
    }
    let (prompt, advice, choices) = question(kind);
    let mut options = String::new();
    for (value, label) in choices {
        options.push_str(&format!("<option value=\"{value}\">{label}</option>"));
    }
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| WebError::Storage)?;
    let nonce: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
    Ok(include_str!("../templates/wizard.html")
        .replace("{{ITEM}}", &escape(&item.id))
        .replace(
            "{{ITEM_QUERY}}",
            &escape(
                &url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("item", &item.id)
                    .finish(),
            ),
        )
        .replace("{{TITLE}}", &escape(&item_title(item)))
        .replace("{{LIBRARY}}", &escape(&item.library_name))
        .replace("{{CSRF}}", &escape(csrf))
        .replace("{{NONCE}}", &nonce)
        .replace("{{ISSUE}}", &escape(issue))
        .replace("{{QUESTION}}", prompt)
        .replace("{{ADVICE}}", advice)
        .replace("{{OPTIONS}}", &options)
        .replace(
            "{{DETAILS_LABEL}}",
            if kind == WizardIssue::Other {
                "Describe the problem"
            } else {
                "Anything else?"
            },
        )
        .replace(
            "{{DETAILS_MAX}}",
            if kind == WizardIssue::Other {
                "140"
            } else {
                "2000"
            },
        )
        .replace(
            "{{DETAILS_REQUIRED}}",
            if kind == WizardIssue::Other {
                "required"
            } else {
                ""
            },
        )
        .replace(
            "{{DETAILS_OPTIONAL}}",
            if kind == WizardIssue::Other {
                ""
            } else {
                "Optional"
            },
        ))
}
