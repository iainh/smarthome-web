use crate::database::{Database, StoredDevice};
use crate::group::{DeviceGroup, GroupEngine};
use crate::AppState;
use axum::body::{to_bytes, Body};
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::Router;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use smarthome::{SmartHomeClient, SmartPlug};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, OwnedMutexGuard, Semaphore};

const MAX_BODY_BYTES: usize = 1024;
const MAX_DEVICES: usize = 256;
const MAX_GROUPS: usize = 50;
const MAX_GROUP_MEMBERS: usize = 256;
const MAX_INVENTORY_BYTES: usize = 1024 * 1024;
const MAX_WORKERS: usize = 32;
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const DEVICE_TIMEOUT: Duration = Duration::from_secs(3);

type BoxError = Box<dyn Error + Send + Sync>;

#[derive(Clone)]
pub enum Auth {
    Disabled,
    Enabled([u8; 32]),
}

impl Auth {
    pub fn from_environment() -> Result<Self, BoxError> {
        let Some(path) = std::env::var_os("PEBBLE_API_TOKEN_FILE") else {
            return Ok(Self::Disabled);
        };
        let token = fs::read_to_string(PathBuf::from(path))?;
        let token = token.strip_suffix('\n').unwrap_or(&token);
        let token = token.strip_suffix('\r').unwrap_or(token);
        decode_token(token).map(Self::Enabled).ok_or_else(|| {
            "PEBBLE_API_TOKEN_FILE must contain exactly 64 lowercase hex characters".into()
        })
    }
}

trait DeviceIo: Send + Sync {
    fn probe(&self, address: IpAddr, deadline: Instant) -> smarthome::Result<SmartPlug>;
    fn relay(&self, address: IpAddr, on: bool, deadline: Instant) -> smarthome::Result<()>;
    fn brightness(
        &self,
        address: IpAddr,
        brightness: u8,
        deadline: Instant,
    ) -> smarthome::Result<()>;
}

impl DeviceIo for SmartHomeClient {
    fn probe(&self, address: IpAddr, deadline: Instant) -> smarthome::Result<SmartPlug> {
        self.get_sysinfo_before(address, deadline)
    }

    fn relay(&self, address: IpAddr, on: bool, deadline: Instant) -> smarthome::Result<()> {
        self.set_relay_before(address, on, deadline)
    }

    fn brightness(
        &self,
        address: IpAddr,
        brightness: u8,
        deadline: Instant,
    ) -> smarthome::Result<()> {
        self.set_brightness_before(address, brightness, deadline)
    }
}

struct ApiState {
    auth: Auth,
    client: Arc<dyn DeviceIo>,
    database: Arc<Database>,
    groups: Arc<GroupEngine>,
    admission: Arc<Mutex<()>>,
}

pub fn router(app: Arc<AppState>, auth: Auth) -> Router<Arc<AppState>> {
    let state = Arc::new(ApiState {
        auth,
        client: Arc::new(app.client.clone()),
        database: app.database.clone(),
        groups: app.groups.clone(),
        admission: Arc::new(Mutex::new(())),
    });
    api_router(state)
}

fn api_router(state: Arc<ApiState>) -> Router<Arc<AppState>> {
    routes().with_state(state)
}

fn routes() -> Router<Arc<ApiState>> {
    Router::new().nest(
        "/api/v1",
        Router::new()
            .route("/info", get(info))
            .route("/inventory", get(inventory))
            .route("/devices/{device_id}/relay", put(device_relay))
            .route("/devices/{device_id}/brightness", put(device_brightness))
            .route("/groups/{group_id}/relay", put(group_relay))
            .fallback(api_not_found)
            .method_not_allowed_fallback(api_not_found),
    )
}

#[derive(Clone)]
struct RequestContext {
    id: String,
}

fn authorize(state: &ApiState, headers: &HeaderMap) -> Result<RequestContext, ApiError> {
    let id = request_id(headers);
    let Auth::Enabled(expected) = &state.auth else {
        return Err(ApiError::new(
            id,
            StatusCode::SERVICE_UNAVAILABLE,
            "api_disabled",
            "The Pebble API is not configured.",
            false,
        ));
    };
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(decode_token);
    let valid = supplied
        .as_ref()
        .is_some_and(|supplied| constant_time_eq(expected, supplied));
    if !valid {
        return Err(ApiError::new(
            id,
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "A valid Bearer token is required.",
            false,
        ));
    }
    Ok(RequestContext { id })
}

async fn info(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let context = authorize(&state, &headers)?;
    Ok(json_response(
        StatusCode::OK,
        &context.id,
        &json!({
            "api_version": 1,
            "name": "Smart Home",
            "features": {
                "device_relay": true,
                "device_brightness": true,
                "group_relay": true,
                "group_brightness": false
            },
            "limits": {
                "devices": MAX_DEVICES,
                "groups": MAX_GROUPS,
                "group_members": MAX_GROUP_MEMBERS,
                "inventory_bytes": MAX_INVENTORY_BYTES,
                "brightness_min": 1,
                "brightness_max": 100
            }
        }),
    ))
}

async fn inventory(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let context = authorize(&state, &headers)?;
    let guard = admit(&state, &context.id)?;
    let operation_state = state.clone();
    let id = context.id.clone();
    let result = tokio::spawn(async move {
        let _guard = guard;
        build_inventory(operation_state, id).await
    })
    .await
    .map_err(|_| ApiError::internal(context.id.clone()))??;
    let body = serde_json::to_vec(&result).map_err(|_| ApiError::internal(context.id.clone()))?;
    if body.len() > MAX_INVENTORY_BYTES {
        return Err(ApiError::new(
            context.id,
            StatusCode::UNPROCESSABLE_ENTITY,
            "inventory_too_large",
            "The inventory exceeds the API size limit.",
            false,
        ));
    }
    Ok(bytes_response(StatusCode::OK, &context.id, body))
}

async fn device_relay(
    State(state): State<Arc<ApiState>>,
    Path(device_id): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Result<Response, ApiError> {
    let context = authorize(&state, &headers)?;
    let body: RelayRequest = parse_body(request, &context.id).await?;
    let stored = find_device(&state, &device_id, &context.id)?;
    let guard = admit(&state, &context.id)?;
    let operation_state = state.clone();
    let on = body.on;
    let result = tokio::spawn(async move {
        let _guard = guard;
        run_device(operation_state, stored, Requested::Relay(on)).await
    })
    .await
    .map_err(|_| ApiError::internal_may_have_run(context.id.clone()))?;
    Ok(json_response(
        StatusCode::OK,
        &context.id,
        &mutation_response(
            &context.id,
            "device",
            &device_id,
            Requested::Relay(on),
            vec![result],
        ),
    ))
}

async fn device_brightness(
    State(state): State<Arc<ApiState>>,
    Path(device_id): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Result<Response, ApiError> {
    let context = authorize(&state, &headers)?;
    let body: BrightnessRequest = parse_body(request, &context.id).await?;
    if !(1..=100).contains(&body.brightness) {
        return Err(ApiError::new(
            context.id,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_brightness",
            "Brightness must be an integer from 1 through 100.",
            false,
        )
        .details(json!({ "field": "brightness" })));
    }
    let stored = find_device(&state, &device_id, &context.id)?;
    let guard = admit(&state, &context.id)?;
    let operation_state = state.clone();
    let requested = Requested::Brightness(body.brightness as u8);
    let id = context.id.clone();
    let result = tokio::spawn(async move {
        let _guard = guard;
        preflight_brightness(operation_state, stored, requested, id).await
    })
    .await
    .map_err(|_| ApiError::internal_may_have_run(context.id.clone()))??;
    Ok(json_response(
        StatusCode::OK,
        &context.id,
        &mutation_response(&context.id, "device", &device_id, requested, vec![result]),
    ))
}

async fn group_relay(
    State(state): State<Arc<ApiState>>,
    Path(group_id): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Result<Response, ApiError> {
    let context = authorize(&state, &headers)?;
    let body: RelayRequest = parse_body(request, &context.id).await?;
    let id = group_id.parse::<u64>().map_err(|_| {
        ApiError::new(
            context.id.clone(),
            StatusCode::NOT_FOUND,
            "group_not_found",
            "The requested group was not found.",
            false,
        )
    })?;
    let group = state
        .groups
        .get(id)
        .map_err(|_| ApiError::internal(context.id.clone()))?
        .ok_or_else(|| ApiError::not_found(context.id.clone(), "group_not_found", "group"))?;
    if group.device_ids.len() > MAX_GROUP_MEMBERS {
        return Err(ApiError::new(
            context.id,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
            "The group exceeds the member limit.",
            false,
        ));
    }
    let guard = admit(&state, &context.id)?;
    let stored = state
        .database
        .stored_devices()
        .map_err(|_| ApiError::internal(context.id.clone()))?;
    let operation_state = state.clone();
    let on = body.on;
    let results = tokio::spawn(async move {
        let _guard = guard;
        run_group(operation_state, group, stored, on).await
    })
    .await
    .map_err(|_| ApiError::internal_may_have_run(context.id.clone()))?;
    Ok(json_response(
        StatusCode::OK,
        &context.id,
        &mutation_response(
            &context.id,
            "group",
            &group_id,
            Requested::Relay(on),
            results,
        ),
    ))
}

async fn api_not_found(headers: HeaderMap, State(state): State<Arc<ApiState>>) -> Response {
    let id = request_id(&headers);
    if let Err(error) = authorize(&state, &headers) {
        return error.into_response();
    }
    ApiError::new(
        id,
        StatusCode::NOT_FOUND,
        "invalid_request",
        "The requested API route was not found.",
        false,
    )
    .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RelayRequest {
    on: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BrightnessRequest {
    brightness: i64,
}

async fn parse_body<T: DeserializeOwned>(request: Request, id: &str) -> Result<T, ApiError> {
    let content_type_ok = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(';').next() == Some("application/json"));
    if !content_type_ok {
        return Err(ApiError::new(
            id.to_owned(),
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Content-Type must be application/json.",
            false,
        ));
    }
    let bytes = to_bytes(request.into_body(), MAX_BODY_BYTES)
        .await
        .map_err(|_| {
            ApiError::new(
                id.to_owned(),
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "The request body exceeds 1024 bytes.",
                false,
            )
        })?;
    serde_json::from_slice(&bytes).map_err(|_| {
        ApiError::new(
            id.to_owned(),
            StatusCode::BAD_REQUEST,
            "invalid_json",
            "The request body is not valid for this operation.",
            false,
        )
    })
}

#[derive(Clone, Copy)]
enum Requested {
    Relay(bool),
    Brightness(u8),
}

#[derive(Serialize)]
struct DeviceResult {
    device_id: String,
    outcome: &'static str,
    code: Option<&'static str>,
    observed: Option<ObservedState>,
}

#[derive(Serialize)]
struct ObservedState {
    relay: &'static str,
    brightness: Option<u8>,
    observed_at: i64,
}

async fn preflight_brightness(
    state: Arc<ApiState>,
    stored: StoredDevice,
    requested: Requested,
    request_id: String,
) -> Result<DeviceResult, ApiError> {
    let deadline = Instant::now() + DEVICE_TIMEOUT;
    let client = state.client.clone();
    let address = stored.device.address;
    let probe = tokio::task::spawn_blocking(move || client.probe(address, deadline))
        .await
        .map_err(|_| ApiError::internal(request_id.clone()))?;
    let Ok(probed) = probe else {
        return Ok(ApiError::evaluated_failed(
            &stored.device.device_id,
            "device_unreachable",
        ));
    };
    if probed.device_id != stored.device.device_id {
        return Ok(ApiError::evaluated_failed(
            &stored.device.device_id,
            "identity_mismatch",
        ));
    }
    state
        .database
        .remember_devices(std::slice::from_ref(&probed))
        .map_err(|_| ApiError::internal(request_id.clone()))?;
    if probed.brightness.is_none() {
        return Err(ApiError::new(
            request_id,
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_capability",
            "This device does not support brightness.",
            false,
        )
        .details(json!({ "field": "brightness" })));
    }
    if !probed.relay_on {
        return Err(ApiError::new(
            request_id,
            StatusCode::CONFLICT,
            "relay_off",
            "Brightness cannot be changed while the relay is off.",
            false,
        ));
    }
    Ok(run_device_after_probe(state, stored, requested, deadline).await)
}

async fn run_device(
    state: Arc<ApiState>,
    stored: StoredDevice,
    requested: Requested,
) -> DeviceResult {
    let deadline = Instant::now() + DEVICE_TIMEOUT;
    let client = state.client.clone();
    let address = stored.device.address;
    let probe = tokio::task::spawn_blocking(move || client.probe(address, deadline)).await;
    let Ok(Ok(probed)) = probe else {
        return ApiError::evaluated_failed(&stored.device.device_id, "device_unreachable");
    };
    if probed.device_id != stored.device.device_id {
        return ApiError::evaluated_failed(&stored.device.device_id, "identity_mismatch");
    }
    if state
        .database
        .remember_devices(std::slice::from_ref(&probed))
        .is_err()
    {
        return ApiError::evaluated_failed(&stored.device.device_id, "verification_failed");
    }
    run_device_after_probe(state, stored, requested, deadline).await
}

async fn run_device_after_probe(
    state: Arc<ApiState>,
    stored: StoredDevice,
    requested: Requested,
    deadline: Instant,
) -> DeviceResult {
    let device_id = stored.device.device_id;
    let address = stored.device.address;
    let client = state.client.clone();
    let command = tokio::task::spawn_blocking(move || match requested {
        Requested::Relay(on) => client.relay(address, on, deadline),
        Requested::Brightness(brightness) => client.brightness(address, brightness, deadline),
    })
    .await;
    if !matches!(command, Ok(Ok(()))) {
        return DeviceResult {
            device_id,
            outcome: "unknown",
            code: Some("verification_failed"),
            observed: None,
        };
    }
    let client = state.client.clone();
    let readback = tokio::task::spawn_blocking(move || client.probe(address, deadline)).await;
    let Ok(Ok(observed)) = readback else {
        return DeviceResult {
            device_id,
            outcome: "unknown",
            code: Some("verification_failed"),
            observed: None,
        };
    };
    if observed.device_id != device_id {
        return DeviceResult {
            device_id,
            outcome: "unknown",
            code: Some("identity_mismatch"),
            observed: None,
        };
    }
    let matches = match requested {
        Requested::Relay(on) => observed.relay_on == on,
        Requested::Brightness(brightness) => observed.brightness == Some(brightness),
    };
    let observed_at = crate::database::unix_timestamp().unwrap_or(0);
    if state
        .database
        .remember_devices(std::slice::from_ref(&observed))
        .is_err()
    {
        return DeviceResult {
            device_id,
            outcome: "unknown",
            code: Some("verification_failed"),
            observed: Some(observed_state(&observed, observed_at)),
        };
    }
    DeviceResult {
        device_id,
        outcome: if matches { "confirmed" } else { "failed" },
        code: (!matches).then_some("state_mismatch"),
        observed: Some(observed_state(&observed, observed_at)),
    }
}

async fn run_group(
    state: Arc<ApiState>,
    group: DeviceGroup,
    stored: Vec<StoredDevice>,
    on: bool,
) -> Vec<DeviceResult> {
    let by_id: HashMap<_, _> = stored
        .into_iter()
        .map(|stored| (stored.device.device_id.clone(), stored))
        .collect();
    let semaphore = Arc::new(Semaphore::new(MAX_WORKERS));
    let mut tasks = tokio::task::JoinSet::new();
    let mut seen = HashSet::new();
    let mut results = Vec::new();
    for device_id in group.device_ids {
        if !seen.insert(device_id.clone()) {
            continue;
        }
        let Some(stored) = by_id.get(&device_id).cloned() else {
            results.push(DeviceResult {
                device_id,
                outcome: "not_attempted",
                code: Some("device_unreachable"),
                observed: None,
            });
            continue;
        };
        let worker_state = state.clone();
        let permits = semaphore.clone();
        tasks.spawn(async move {
            let _permit = permits.acquire_owned().await.ok();
            run_device(worker_state, stored, Requested::Relay(on)).await
        });
    }
    while let Some(result) = tasks.join_next().await {
        if let Ok(result) = result {
            results.push(result);
        }
    }
    results.sort_by(|left, right| left.device_id.cmp(&right.device_id));
    results
}

#[derive(Serialize)]
struct Inventory {
    api_version: u8,
    generated_at: i64,
    refresh_complete: bool,
    devices: Vec<InventoryDevice>,
    groups: Vec<InventoryGroup>,
}

#[derive(Serialize)]
struct InventoryDevice {
    id: String,
    name: String,
    capabilities: Capabilities,
    availability: &'static str,
    last_seen_at: i64,
    state: InventoryDeviceState,
}

#[derive(Serialize)]
struct Capabilities {
    relay: bool,
    brightness: bool,
}

#[derive(Serialize)]
struct InventoryDeviceState {
    relay: &'static str,
    brightness: Option<u8>,
    observed_at: i64,
}

#[derive(Serialize)]
struct InventoryGroup {
    id: String,
    name: String,
    member_ids: Vec<String>,
    capabilities: Capabilities,
    availability: &'static str,
    members: MemberCounts,
    state: GroupState,
}

#[derive(Default, Serialize)]
struct MemberCounts {
    total: usize,
    online: usize,
    offline: usize,
    unknown: usize,
}

#[derive(Serialize)]
struct GroupState {
    relay: &'static str,
    observed_at: Option<i64>,
}

async fn build_inventory(state: Arc<ApiState>, request_id: String) -> Result<Inventory, ApiError> {
    let stored = state
        .database
        .stored_devices()
        .map_err(|_| ApiError::internal(request_id.clone()))?;
    let groups = state
        .groups
        .groups()
        .map_err(|_| ApiError::internal(request_id.clone()))?;
    if stored.len() > MAX_DEVICES
        || groups.len() > MAX_GROUPS
        || groups
            .iter()
            .any(|group| group.device_ids.len() > MAX_GROUP_MEMBERS)
    {
        return Err(ApiError::new(
            request_id,
            StatusCode::UNPROCESSABLE_ENTITY,
            "inventory_too_large",
            "The inventory exceeds the API item limits.",
            false,
        ));
    }
    let semaphore = Arc::new(Semaphore::new(MAX_WORKERS));
    let mut tasks = tokio::task::JoinSet::new();
    for remembered in stored.iter().cloned() {
        let permits = semaphore.clone();
        let client = state.client.clone();
        tasks.spawn(async move {
            let _permit = permits.acquire_owned().await.ok();
            let expected = remembered.device.device_id.clone();
            let address = remembered.device.address;
            let deadline = Instant::now() + PROBE_TIMEOUT;
            let probe = tokio::task::spawn_blocking(move || client.probe(address, deadline)).await;
            let status = match probe {
                Ok(Ok(device)) if device.device_id == expected => ProbeStatus::Online(device),
                Ok(Ok(_)) => ProbeStatus::Unknown,
                _ => ProbeStatus::Offline,
            };
            (expected, status)
        });
    }
    let mut statuses = HashMap::new();
    while let Some(result) = tasks.join_next().await {
        if let Ok((id, status)) = result {
            statuses.insert(id, status);
        }
    }
    let generated_at = crate::database::unix_timestamp().unwrap_or(0);
    let fresh: Vec<_> = statuses
        .values()
        .filter_map(|status| match status {
            ProbeStatus::Online(device) => Some(device.clone()),
            _ => None,
        })
        .collect();
    state
        .database
        .remember_devices(&fresh)
        .map_err(|_| ApiError::internal(request_id))?;
    let devices = stored
        .iter()
        .map(|remembered| {
            inventory_device(
                remembered,
                statuses.get(&remembered.device.device_id),
                generated_at,
            )
        })
        .collect();
    let group_views = groups
        .into_iter()
        .map(|group| inventory_group(group, &statuses, generated_at))
        .collect();
    Ok(Inventory {
        api_version: 1,
        generated_at,
        refresh_complete: true,
        devices,
        groups: group_views,
    })
}

enum ProbeStatus {
    Online(SmartPlug),
    Offline,
    Unknown,
}

fn inventory_device(
    remembered: &StoredDevice,
    status: Option<&ProbeStatus>,
    now: i64,
) -> InventoryDevice {
    let (device, availability, observed_at) = match status {
        Some(ProbeStatus::Online(device)) => (device, "online", now),
        Some(ProbeStatus::Offline) => (&remembered.device, "offline", remembered.last_seen_at),
        _ => (&remembered.device, "unknown", remembered.last_seen_at),
    };
    InventoryDevice {
        id: remembered.device.device_id.clone(),
        name: device.alias.clone(),
        capabilities: Capabilities {
            relay: true,
            brightness: device.brightness.is_some(),
        },
        availability,
        last_seen_at: observed_at,
        state: InventoryDeviceState {
            relay: relay_state(device.relay_on),
            brightness: device.brightness,
            observed_at,
        },
    }
}

fn inventory_group(
    group: DeviceGroup,
    statuses: &HashMap<String, ProbeStatus>,
    now: i64,
) -> InventoryGroup {
    let mut counts = MemberCounts {
        total: group.device_ids.len(),
        ..MemberCounts::default()
    };
    let mut on = 0;
    let mut off = 0;
    for device_id in &group.device_ids {
        match statuses.get(device_id) {
            Some(ProbeStatus::Online(device)) => {
                counts.online += 1;
                if device.relay_on {
                    on += 1
                } else {
                    off += 1
                }
            }
            Some(ProbeStatus::Offline) => counts.offline += 1,
            _ => counts.unknown += 1,
        }
    }
    let availability = if counts.total == 0 || counts.unknown == counts.total {
        "unknown"
    } else if counts.online == counts.total {
        "online"
    } else if counts.online > 0 {
        "partial"
    } else if counts.offline > 0 && counts.unknown == 0 {
        "offline"
    } else {
        "unknown"
    };
    let relay = if counts.online == counts.total && on == counts.total {
        "on"
    } else if counts.online == counts.total && off == counts.total {
        "off"
    } else if on > 0 && off > 0 {
        "mixed"
    } else {
        "unknown"
    };
    InventoryGroup {
        id: group.id.to_string(),
        name: group.name,
        member_ids: group.device_ids,
        capabilities: Capabilities {
            relay: true,
            brightness: false,
        },
        availability,
        members: counts,
        state: GroupState {
            relay,
            observed_at: (relay != "unknown").then_some(now),
        },
    }
}

fn mutation_response(
    request_id: &str,
    kind: &str,
    id: &str,
    requested: Requested,
    results: Vec<DeviceResult>,
) -> Value {
    let confirmed = results
        .iter()
        .filter(|result| result.outcome == "confirmed")
        .count();
    let failed = results
        .iter()
        .filter(|result| result.outcome == "failed")
        .count();
    let unknown = results
        .iter()
        .filter(|result| result.outcome == "unknown")
        .count();
    let not_attempted = results
        .iter()
        .filter(|result| result.outcome == "not_attempted")
        .count();
    let outcome = if confirmed == results.len() {
        "confirmed"
    } else if confirmed > 0 {
        "partial"
    } else if unknown > 0 {
        "unknown"
    } else {
        "failed"
    };
    let requested = match requested {
        Requested::Relay(on) => json!({ "relay": if on { "on" } else { "off" } }),
        Requested::Brightness(brightness) => json!({ "brightness": brightness }),
    };
    json!({
        "request_id": request_id,
        "target": { "kind": kind, "id": id },
        "requested": requested,
        "outcome": outcome,
        "counts": {
            "confirmed": confirmed,
            "failed": failed,
            "unknown": unknown,
            "not_attempted": not_attempted
        },
        "results": results
    })
}

fn find_device(
    state: &ApiState,
    device_id: &str,
    request_id: &str,
) -> Result<StoredDevice, ApiError> {
    state
        .database
        .stored_devices()
        .map_err(|_| ApiError::internal(request_id.to_owned()))?
        .into_iter()
        .find(|stored| stored.device.device_id == device_id)
        .ok_or_else(|| ApiError::not_found(request_id.to_owned(), "device_not_found", "device"))
}

fn admit(state: &ApiState, request_id: &str) -> Result<OwnedMutexGuard<()>, ApiError> {
    state.admission.clone().try_lock_owned().map_err(|_| {
        ApiError::new(
            request_id.to_owned(),
            StatusCode::TOO_MANY_REQUESTS,
            "busy",
            "Another Pebble API operation is in progress.",
            true,
        )
        .retry_after()
    })
}

fn observed_state(device: &SmartPlug, observed_at: i64) -> ObservedState {
    ObservedState {
        relay: relay_state(device.relay_on),
        brightness: device.brightness,
        observed_at,
    }
}

fn relay_state(on: bool) -> &'static str {
    if on {
        "on"
    } else {
        "off"
    }
}

#[derive(Debug)]
struct ApiError {
    request_id: String,
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    retryable: bool,
    execution: &'static str,
    details: Option<Value>,
    retry_after: bool,
}

impl ApiError {
    fn new(
        request_id: String,
        status: StatusCode,
        code: &'static str,
        message: &'static str,
        retryable: bool,
    ) -> Self {
        Self {
            request_id,
            status,
            code,
            message,
            retryable,
            execution: "not_started",
            details: None,
            retry_after: false,
        }
    }

    fn internal(request_id: String) -> Self {
        Self::new(
            request_id,
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "The server could not complete the request.",
            false,
        )
    }

    fn internal_may_have_run(request_id: String) -> Self {
        let mut error = Self::internal(request_id);
        error.execution = "may_have_run";
        error
    }

    fn not_found(request_id: String, code: &'static str, noun: &'static str) -> Self {
        Self::new(
            request_id,
            StatusCode::NOT_FOUND,
            code,
            if noun == "device" {
                "The requested device was not found."
            } else {
                "The requested group was not found."
            },
            false,
        )
    }

    fn details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
    fn retry_after(mut self) -> Self {
        self.retry_after = true;
        self
    }

    fn evaluated_failed(device_id: &str, code: &'static str) -> DeviceResult {
        DeviceResult {
            device_id: device_id.to_owned(),
            outcome: "failed",
            code: Some(code),
            observed: None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "request_id": self.request_id,
            "error": {
                "code": self.code,
                "message": self.message,
                "retryable": self.retryable,
                "execution": self.execution,
                "details": self.details
            }
        });
        let mut response = json_response(self.status, &self.request_id, &body);
        if self.status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        if self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        response
    }
}

fn json_response(status: StatusCode, request_id: &str, value: &impl Serialize) -> Response {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    bytes_response(status, request_id, body)
}

fn bytes_response(status: StatusCode, request_id: &str, body: Vec<u8>) -> Response {
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

fn request_id(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
        .map(str::to_owned)
        .unwrap_or_else(|| {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            format!(
                "srv-{}-{}",
                crate::database::unix_timestamp().unwrap_or(0),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )
        })
}

fn decode_token(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let mut token = [0; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        token[index] = (hex(pair[0])? << 4) | hex(pair[1])?;
    }
    Some(token)
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use serde_json::Value;
    use std::collections::{HashMap, HashSet};
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;
    use tower::ServiceExt;

    static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);

    struct FakeIo {
        devices: StdMutex<HashMap<IpAddr, SmartPlug>>,
        unreachable: StdMutex<HashSet<IpAddr>>,
        relay_calls: AtomicUsize,
        brightness_calls: AtomicUsize,
        delay: Duration,
    }

    impl FakeIo {
        fn new(devices: impl IntoIterator<Item = SmartPlug>) -> Self {
            Self {
                devices: StdMutex::new(
                    devices
                        .into_iter()
                        .map(|device| (device.address, device))
                        .collect(),
                ),
                unreachable: StdMutex::new(HashSet::new()),
                relay_calls: AtomicUsize::new(0),
                brightness_calls: AtomicUsize::new(0),
                delay: Duration::ZERO,
            }
        }

        fn with_delay(mut self, delay: Duration) -> Self {
            self.delay = delay;
            self
        }

        fn error() -> smarthome::Error {
            smarthome::Error::Io(io::Error::new(io::ErrorKind::TimedOut, "fault injected"))
        }

        fn wait(&self, deadline: Instant) -> smarthome::Result<()> {
            if !self.delay.is_zero() {
                std::thread::sleep(
                    self.delay.min(
                        deadline
                            .checked_duration_since(Instant::now())
                            .unwrap_or_default(),
                    ),
                );
            }
            if Instant::now() >= deadline {
                return Err(Self::error());
            }
            Ok(())
        }
    }

    impl DeviceIo for FakeIo {
        fn probe(&self, address: IpAddr, deadline: Instant) -> smarthome::Result<SmartPlug> {
            self.wait(deadline)?;
            if self.unreachable.lock().unwrap().contains(&address) {
                return Err(Self::error());
            }
            self.devices
                .lock()
                .unwrap()
                .get(&address)
                .cloned()
                .ok_or_else(Self::error)
        }

        fn relay(&self, address: IpAddr, on: bool, deadline: Instant) -> smarthome::Result<()> {
            self.wait(deadline)?;
            self.relay_calls.fetch_add(1, Ordering::SeqCst);
            self.devices
                .lock()
                .unwrap()
                .get_mut(&address)
                .ok_or_else(Self::error)?
                .relay_on = on;
            Ok(())
        }

        fn brightness(
            &self,
            address: IpAddr,
            brightness: u8,
            deadline: Instant,
        ) -> smarthome::Result<()> {
            self.wait(deadline)?;
            self.brightness_calls.fetch_add(1, Ordering::SeqCst);
            self.devices
                .lock()
                .unwrap()
                .get_mut(&address)
                .ok_or_else(Self::error)?
                .brightness = Some(brightness);
            Ok(())
        }
    }

    fn plug(address: &str, id: &str, relay_on: bool, brightness: Option<u8>) -> SmartPlug {
        SmartPlug {
            address: address.parse().unwrap(),
            model: "HS220(US)".to_owned(),
            alias: format!("Device {id}"),
            device_id: id.to_owned(),
            software_version: "1.0".to_owned(),
            relay_on,
            brightness,
            latitude: None,
            longitude: None,
        }
    }

    fn test_state(
        auth: Auth,
        remembered: &[SmartPlug],
        client: Arc<dyn DeviceIo>,
    ) -> Arc<ApiState> {
        let path = std::env::temp_dir().join(format!(
            "smarthome-api-{}-{}.sqlite3",
            std::process::id(),
            NEXT_DATABASE.fetch_add(1, Ordering::Relaxed)
        ));
        let database = Arc::new(Database::open(path).unwrap());
        database.remember_devices(remembered).unwrap();
        Arc::new(ApiState {
            auth,
            client,
            groups: Arc::new(GroupEngine::new(database.clone())),
            database,
            admission: Arc::new(Mutex::new(())),
        })
    }

    fn token() -> ([u8; 32], String) {
        ([0xab; 32], "ab".repeat(32))
    }

    fn request(method: &str, path: &str, token: Option<&str>, body: &str) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if !body.is_empty() {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        builder.body(Body::from(body.to_owned())).unwrap()
    }

    async fn response_json(response: Response) -> Value {
        serde_json::from_slice(
            &to_bytes(response.into_body(), MAX_INVENTORY_BYTES)
                .await
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn tokens_require_lowercase_fixed_length_hex() {
        assert!(decode_token(&"ab".repeat(32)).is_some());
        assert!(decode_token(&"AB".repeat(32)).is_none());
        assert!(decode_token("ab").is_none());
        assert!(constant_time_eq(&[7; 32], &[7; 32]));
        assert!(!constant_time_eq(&[7; 32], &[8; 32]));
    }

    #[test]
    fn group_state_requires_complete_fresh_observation() {
        let group = DeviceGroup {
            id: 7,
            name: "Lights".to_owned(),
            device_ids: vec!["on".to_owned(), "missing".to_owned()],
        };
        let mut statuses = HashMap::new();
        statuses.insert(
            "on".to_owned(),
            ProbeStatus::Online(SmartPlug {
                address: "192.0.2.1".parse().unwrap(),
                model: "HS100".to_owned(),
                alias: "One".to_owned(),
                device_id: "on".to_owned(),
                software_version: "1".to_owned(),
                relay_on: true,
                brightness: None,
                latitude: None,
                longitude: None,
            }),
        );
        let view = inventory_group(group, &statuses, 42);
        assert_eq!(view.availability, "partial");
        assert_eq!(view.state.relay, "unknown");
        assert_eq!(view.members.unknown, 1);
    }

    #[tokio::test]
    async fn disabled_and_unauthorized_requests_use_json_errors() {
        let fake = Arc::new(FakeIo::new([]));
        let disabled = routes().with_state(test_state(Auth::Disabled, &[], fake.clone()));
        let response = disabled
            .oneshot(request("GET", "/api/v1/info", None, ""))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response_json(response).await["error"]["code"],
            "api_disabled"
        );

        let (token_bytes, _) = token();
        let enabled = routes().with_state(test_state(Auth::Enabled(token_bytes), &[], fake));
        let response = enabled
            .oneshot(request("GET", "/api/v1/info", Some(&"00".repeat(32)), ""))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response_json(response).await["error"]["execution"],
            "not_started"
        );
    }

    #[tokio::test]
    async fn identity_mismatch_never_actuates_the_remembered_address() {
        let remembered = plug("192.0.2.10", "expected", false, None);
        let imposter = plug("192.0.2.10", "imposter", false, None);
        let fake = Arc::new(FakeIo::new([imposter]));
        let (token_bytes, token) = token();
        let app = routes().with_state(test_state(
            Auth::Enabled(token_bytes),
            &[remembered],
            fake.clone(),
        ));
        let response = app
            .oneshot(request(
                "PUT",
                "/api/v1/devices/expected/relay",
                Some(&token),
                r#"{"on":true}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["outcome"], "failed");
        assert_eq!(body["results"][0]["code"], "identity_mismatch");
        assert_eq!(fake.relay_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn relay_is_read_back_and_persisted_by_stable_id() {
        let old = plug("192.0.2.10", "stable", false, None);
        let moved = plug("192.0.2.11", "stable", false, None);
        let fake = Arc::new(FakeIo::new([moved.clone()]));
        let (token_bytes, token) = token();
        let state = test_state(
            Auth::Enabled(token_bytes),
            &[old, moved.clone()],
            fake.clone(),
        );
        let app = routes().with_state(state.clone());
        let response = app
            .oneshot(request(
                "PUT",
                "/api/v1/devices/stable/relay",
                Some(&token),
                r#"{"on":true}"#,
            ))
            .await
            .unwrap();
        let body = response_json(response).await;
        assert_eq!(body["outcome"], "confirmed");
        assert_eq!(body["results"][0]["observed"]["relay"], "on");
        let stored = state.database.devices().unwrap();
        assert_eq!(stored[0].address, moved.address);
        assert!(stored[0].relay_on);
    }

    #[tokio::test]
    async fn brightness_rejects_invalid_and_unmet_prerequisites() {
        let off_dimmer = plug("192.0.2.20", "dimmer", false, Some(40));
        let fake = Arc::new(FakeIo::new([off_dimmer.clone()]));
        let (token_bytes, token) = token();
        let app = routes().with_state(test_state(
            Auth::Enabled(token_bytes),
            &[off_dimmer],
            fake.clone(),
        ));
        let invalid = app
            .clone()
            .oneshot(request(
                "PUT",
                "/api/v1/devices/dimmer/brightness",
                Some(&token),
                r#"{"brightness":256}"#,
            ))
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            response_json(invalid).await["error"]["code"],
            "invalid_brightness"
        );

        let relay_off = app
            .oneshot(request(
                "PUT",
                "/api/v1/devices/dimmer/brightness",
                Some(&token),
                r#"{"brightness":70}"#,
            ))
            .await
            .unwrap();
        assert_eq!(relay_off.status(), StatusCode::CONFLICT);
        assert_eq!(response_json(relay_off).await["error"]["code"], "relay_off");
        assert_eq!(fake.brightness_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn inventory_preserves_offline_state_and_groups_are_honest() {
        let online = plug("192.0.2.30", "online", true, None);
        let offline = plug("192.0.2.31", "offline", false, None);
        let fake = Arc::new(FakeIo::new([online.clone(), offline.clone()]));
        fake.unreachable.lock().unwrap().insert(offline.address);
        let (token_bytes, token) = token();
        let state = test_state(Auth::Enabled(token_bytes), &[online, offline], fake);
        state
            .groups
            .add(
                "Mixed availability",
                vec!["online".to_owned(), "offline".to_owned()],
            )
            .unwrap();
        let response = routes()
            .with_state(state)
            .oneshot(request("GET", "/api/v1/inventory", Some(&token), ""))
            .await
            .unwrap();
        let body = response_json(response).await;
        let offline = body["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|device| device["id"] == "offline")
            .unwrap();
        assert_eq!(offline["availability"], "offline");
        assert_eq!(offline["state"]["relay"], "off");
        assert_eq!(body["groups"][0]["availability"], "partial");
        assert_eq!(body["groups"][0]["state"]["relay"], "unknown");
        assert_eq!(body["groups"][0]["members"]["offline"], 1);
    }

    #[tokio::test]
    async fn admission_stays_busy_after_a_request_is_cancelled() {
        let device = plug("192.0.2.40", "slow", false, None);
        let fake = Arc::new(FakeIo::new([device.clone()]).with_delay(Duration::from_millis(200)));
        let (token_bytes, token) = token();
        let state = test_state(Auth::Enabled(token_bytes), &[device], fake);
        let app = routes().with_state(state.clone());
        let first_app = app.clone();
        let first_token = token.clone();
        let first = tokio::spawn(async move {
            first_app
                .oneshot(request("GET", "/api/v1/inventory", Some(&first_token), ""))
                .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let database_started = Instant::now();
        assert_eq!(state.database.devices().unwrap().len(), 1);
        assert!(database_started.elapsed() < Duration::from_millis(100));
        first.abort();
        let response = app
            .oneshot(request("GET", "/api/v1/inventory", Some(&token), ""))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
    }

    #[tokio::test]
    async fn group_control_reports_partial_and_missing_members() {
        let online = plug("192.0.2.50", "online", true, None);
        let offline = plug("192.0.2.51", "offline", true, None);
        let fake = Arc::new(FakeIo::new([online.clone(), offline.clone()]));
        fake.unreachable.lock().unwrap().insert(offline.address);
        let (token_bytes, token) = token();
        let state = test_state(Auth::Enabled(token_bytes), &[online, offline], fake.clone());
        let group_id = state
            .groups
            .add(
                "All lights",
                vec![
                    "online".to_owned(),
                    "offline".to_owned(),
                    "missing".to_owned(),
                ],
            )
            .unwrap();
        let response = routes()
            .with_state(state)
            .oneshot(request(
                "PUT",
                &format!("/api/v1/groups/{group_id}/relay"),
                Some(&token),
                r#"{"on":false}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert_eq!(body["outcome"], "partial");
        assert_eq!(body["counts"]["confirmed"], 1);
        assert_eq!(body["counts"]["failed"], 1);
        assert_eq!(body["counts"]["not_attempted"], 1);
        assert_eq!(fake.relay_calls.load(Ordering::SeqCst), 1);
    }
}
