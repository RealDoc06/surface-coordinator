//! The script library over the API.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use protocol::Shell;
use serde_json::{json, Value};

use crate::api::{no_content, ApiError, ApiResult};
use crate::state::AppState;
use crate::store::{self, ScriptError};

const MAX_NAME_CHARS: usize = 100;
const MAX_BODY_BYTES: usize = 256 * 1024;

/// A name as the library stores it: trimmed, 1 to 100 characters.
pub fn valid_name(name: &str) -> bool {
    (1..=MAX_NAME_CHARS).contains(&name.trim().chars().count())
}

/// A body that does something and fits the limit.
pub fn valid_body(body: &str) -> bool {
    !body.trim().is_empty() && body.len() <= MAX_BODY_BYTES
}

struct ScriptInput {
    name: String,
    shell: &'static str,
    body: String,
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::bad_request("invalid_script", message)
}

/// Reads a request body as loose JSON so that every problem with it, a
/// missing or mistyped field included, is `invalid_script`.
fn parse(body: &Value) -> Result<ScriptInput, ApiError> {
    let field = |name: &str| body.get(name).and_then(Value::as_str);
    let name = field("name").ok_or_else(|| invalid("name must be a string"))?;
    if !valid_name(name) {
        return Err(invalid(format!("name must be 1 to {MAX_NAME_CHARS} characters")));
    }
    let shell = field("shell").ok_or_else(|| invalid("shell must be \"powershell\" or \"cmd\""))?;
    let shell = match serde_json::from_value::<Shell>(Value::String(shell.to_owned())) {
        Ok(Shell::PowerShell) => "powershell",
        Ok(Shell::Cmd) => "cmd",
        Err(_) => return Err(invalid("shell must be \"powershell\" or \"cmd\"")),
    };
    let script = field("body").ok_or_else(|| invalid("body must be a string"))?;
    if !valid_body(script) {
        return Err(invalid(format!(
            "body must not be empty and must be at most {} KiB",
            MAX_BODY_BYTES / 1024
        )));
    }
    Ok(ScriptInput { name: name.trim().to_owned(), shell, body: script.to_owned() })
}

fn failure(error: ScriptError) -> ApiError {
    match error {
        ScriptError::NotFound => ApiError::not_found("script"),
        ScriptError::NameTaken => name_taken(),
        ScriptError::BuiltIn => {
            ApiError::new(StatusCode::FORBIDDEN, "built_in", "built-in scripts cannot be changed or deleted")
        }
    }
}

fn name_taken() -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "name_taken", "a script with that name already exists")
}

pub async fn list(State(state): State<Arc<AppState>>) -> ApiResult {
    let scripts = state.store.call(|conn| store::list_scripts(conn)).await?;
    Ok(Json(json!({ "scripts": scripts })))
}

pub async fn create(State(state): State<Arc<AppState>>, Json(body): Json<Value>) -> ApiResult<Response> {
    let input = parse(&body)?;
    let script = state
        .store
        .call(move |conn| store::create_script(conn, &input.name, input.shell, &input.body))
        .await?
        .ok_or_else(name_taken)?;
    tracing::info!(script_id = %script.id, name = %script.name, "script created");
    Ok((StatusCode::CREATED, Json(json!({ "script": script }))).into_response())
}

pub async fn replace(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult {
    let input = parse(&body)?;
    let script = state
        .store
        .call(move |conn| store::update_script(conn, &id, &input.name, input.shell, &input.body))
        .await?
        .map_err(failure)?;
    Ok(Json(json!({ "script": script })))
}

pub async fn delete(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Response> {
    state.store.call(move |conn| store::delete_script(conn, &id)).await?.map_err(failure)?;
    Ok(no_content())
}
