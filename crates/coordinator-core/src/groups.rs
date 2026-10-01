//! Device groups over the API.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::{no_content, ApiError, ApiResult};
use crate::events::Event;
use crate::state::AppState;
use crate::store::{self, GroupError};

const MAX_NAME_CHARS: usize = 64;

/// The name from a request body, trimmed and checked. The body is read as
/// loose JSON so that a missing or mistyped name is `invalid_name` rather
/// than a generic rejection.
fn parse_name(body: &Value) -> Result<String, ApiError> {
    let name = body.get("name").and_then(Value::as_str).map(str::trim).unwrap_or_default();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(ApiError::bad_request(
            "invalid_name",
            format!("the name must be 1 to {MAX_NAME_CHARS} characters"),
        ));
    }
    Ok(name.to_owned())
}

fn name_taken() -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "name_taken", "a group with that name already exists")
}

fn failure(error: GroupError) -> ApiError {
    match error {
        GroupError::NotFound => ApiError::not_found("group"),
        GroupError::NameTaken => name_taken(),
        GroupError::UnknownDevice(id) => {
            ApiError::bad_request("unknown_device", format!("no device with id {id:?}"))
        }
    }
}

pub async fn list(State(state): State<Arc<AppState>>) -> ApiResult {
    let groups = state.store.call(|conn| store::list_groups(conn)).await?;
    Ok(Json(json!({ "groups": groups })))
}

pub async fn create(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> ApiResult<Response> {
    let name = parse_name(&body)?;
    let group = state
        .store
        .call(move |conn| store::create_group(conn, &name))
        .await?
        .ok_or_else(name_taken)?;
    tracing::info!(group_id = %group.id, name = %group.name, "group created");
    state.publish(Event::GroupsChanged);
    Ok((StatusCode::CREATED, Json(json!({ "group": group }))).into_response())
}

pub async fn rename(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult {
    let name = parse_name(&body)?;
    let group = state
        .store
        .call(move |conn| store::rename_group(conn, &id, &name))
        .await?
        .map_err(failure)?;
    state.publish(Event::GroupsChanged);
    Ok(Json(json!({ "group": group })))
}

pub async fn delete(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Response> {
    let deleted = state.store.call(move |conn| store::delete_group(conn, &id)).await?;
    if !deleted {
        return Err(ApiError::not_found("group"));
    }
    state.publish(Event::GroupsChanged);
    Ok(no_content())
}

#[derive(Deserialize)]
pub struct Members {
    device_ids: Vec<String>,
}

pub async fn set_members(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(request): Json<Members>,
) -> ApiResult {
    let group = state
        .store
        .call(move |conn| store::set_group_members(conn, &id, &request.device_ids))
        .await?
        .map_err(failure)?;
    state.publish(Event::GroupsChanged);
    Ok(Json(json!({ "group": group })))
}
