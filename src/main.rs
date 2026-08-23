use axum::extract::{Form, Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use minijinja::{context, AutoEscape, Environment};
use serde::{Deserialize, Serialize};
use std::error::Error as StdError;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tddp_client::{SmartHomeClient, SmartPlug};
use tokio::task;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);

struct AppState {
    client: SmartHomeClient,
    templates: Environment<'static>,
}

#[derive(Debug, Serialize)]
struct PlugView {
    address: String,
    model: String,
    alias: String,
    device_id: String,
    software_version: String,
    relay_on: bool,
}

impl From<SmartPlug> for PlugView {
    fn from(plug: SmartPlug) -> Self {
        Self {
            address: plug.address.to_string(),
            model: plug.model,
            alias: plug.alias,
            device_id: plug.device_id,
            software_version: plug.software_version,
            relay_on: plug.relay_on,
        }
    }
}

#[derive(Deserialize)]
struct RelayForm {
    on: bool,
}

#[derive(Debug)]
struct AppError(String);

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl StdError for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.0).into_response()
    }
}

impl From<tddp_client::Error> for AppError {
    fn from(error: tddp_client::Error) -> Self {
        Self(error.to_string())
    }
}

impl From<minijinja::Error> for AppError {
    fn from(error: minijinja::Error) -> Self {
        Self(error.to_string())
    }
}

impl From<task::JoinError> for AppError {
    fn from(error: task::JoinError) -> Self {
        Self(format!("device operation failed: {error}"))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn StdError>> {
    let state = Arc::new(AppState {
        client: SmartHomeClient::new(),
        templates: templates()?,
    });
    let app = Router::new()
        .route("/", get(index))
        .route("/refresh", post(refresh))
        .route("/plugs/{address}/relay", post(set_relay))
        .with_state(state);

    let address: SocketAddr = std::env::var("BIND_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:3000".to_owned())
        .parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    println!("Smart plug manager listening on http://{address}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
    let plugs = discover(state.client.clone()).await?;
    let page = state
        .templates
        .get_template("index.html")?
        .render(context! { plugs })?;
    Ok(Html(page))
}

async fn refresh(State(state): State<Arc<AppState>>) -> Result<Html<String>, AppError> {
    let plugs = discover(state.client.clone()).await?;
    let fragment = state
        .templates
        .get_template("plug-list.html")?
        .render(context! { plugs })?;
    Ok(Html(fragment))
}

async fn set_relay(
    State(state): State<Arc<AppState>>,
    Path(address): Path<IpAddr>,
    Form(form): Form<RelayForm>,
) -> Result<Html<String>, AppError> {
    let client = state.client.clone();
    let plug = task::spawn_blocking(move || {
        client.set_relay(address, form.on)?;
        client.get_sysinfo(address)
    })
    .await??;
    let plug = PlugView::from(plug);
    let fragment = state
        .templates
        .get_template("plug.html")?
        .render(context! { plug })?;
    Ok(Html(fragment))
}

async fn discover(client: SmartHomeClient) -> Result<Vec<PlugView>, AppError> {
    let plugs = task::spawn_blocking(move || client.get_inventory(DISCOVERY_TIMEOUT)).await??;
    Ok(plugs.into_iter().map(PlugView::from).collect())
}

fn templates() -> Result<Environment<'static>, minijinja::Error> {
    let mut templates = Environment::new();
    templates.set_auto_escape_callback(|name| {
        if name.ends_with(".html") {
            AutoEscape::Html
        } else {
            AutoEscape::None
        }
    });
    templates.add_template("index.html", include_str!("../templates/index.html"))?;
    templates.add_template(
        "plug-list.html",
        include_str!("../templates/plug-list.html"),
    )?;
    templates.add_template("plug.html", include_str!("../templates/plug.html"))?;
    Ok(templates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_renders_controls_and_escapes_device_alias() {
        let plugs = vec![PlugView {
            address: "192.0.2.1".to_owned(),
            model: "HS105(US)".to_owned(),
            alias: "<Desk lamp>".to_owned(),
            device_id: "device-1".to_owned(),
            software_version: "1.5.6".to_owned(),
            relay_on: true,
        }];

        let page = templates()
            .unwrap()
            .get_template("index.html")
            .unwrap()
            .render(context! { plugs })
            .unwrap();

        assert!(page.contains("hx-post=\"/refresh\""));
        assert!(page.contains("hx-post=\"/plugs/192.0.2.1/relay\""));
        assert!(page.contains("&lt;Desk lamp&gt;"));
        assert!(!page.contains("<Desk lamp>"));
    }
}
