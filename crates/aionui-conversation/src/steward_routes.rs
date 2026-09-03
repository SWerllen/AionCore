#![allow(clippy::disallowed_types)]

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};

use aionui_api_types::{
    ApiResponse, AskStewardTaskRequest, BindStewardTaskSessionRequest, BootstrapStewardRequest,
    CreateStewardTaskRequest, DispatchStewardTaskRequest, ExecuteStewardCommandRequest, ListStewardTasksQuery,
    ResolveStewardTaskRequest, ResumeStewardTaskRequest, SendMessageResponse, StewardCommandResponse,
    StewardOverviewResponse, StewardProfileResponse, StewardTaskCandidateResponse, StewardTaskInquiryResponse,
    StewardTaskResponse, SwitchStewardAssistantRequest, UpdateStewardTaskRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;

use crate::state::ConversationRouterState;

pub fn steward_routes(state: ConversationRouterState) -> Router {
    Router::new()
        .route("/api/steward/profile", get(profile))
        .route("/api/steward/assistant", post(switch_assistant))
        .route("/api/steward/bootstrap", post(bootstrap))
        .route("/api/steward/commands", post(execute_command))
        .route("/api/steward/overview", get(overview))
        .route("/api/steward/tasks", get(list_tasks).post(create_task))
        .route("/api/steward/tasks/resolve", post(resolve_task))
        .route("/api/steward/tasks/{id}", get(get_task).patch(update_task))
        .route("/api/steward/tasks/{id}/sessions", post(bind_session))
        .route("/api/steward/tasks/{id}/dispatch", post(dispatch_task))
        .route("/api/steward/tasks/{id}/ask", post(ask_task))
        .route("/api/steward/tasks/{id}/resume", post(resume_task))
        .with_state(state)
}

async fn switch_assistant(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<SwitchStewardAssistantRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<StewardProfileResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state.steward.switch_assistant(&user.id, request).await?,
    )))
}

async fn execute_command(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<ExecuteStewardCommandRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<StewardCommandResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state.steward.try_execute_command(&user.id, &request.content).await?,
    )))
}

async fn profile(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Option<StewardProfileResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.steward.profile(&user.id).await?)))
}

async fn bootstrap(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<BootstrapStewardRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<StewardProfileResponse>>), ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let response = state.steward.bootstrap(&user.id, request).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(response))))
}

async fn overview(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<StewardOverviewResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.steward.overview(&user.id).await?)))
}

async fn list_tasks(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ListStewardTasksQuery>,
) -> Result<Json<ApiResponse<Vec<StewardTaskResponse>>>, ApiError> {
    let tasks = state
        .steward
        .list_tasks(&user.id, query.query, query.lifecycle, query.limit.unwrap_or(100))
        .await?;
    Ok(Json(ApiResponse::ok(tasks)))
}

async fn create_task(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateStewardTaskRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<StewardTaskResponse>>), ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let task = state.steward.create_task(&user.id, request).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(task))))
}

async fn resolve_task(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<ResolveStewardTaskRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<Vec<StewardTaskCandidateResponse>>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(state.steward.resolve(&user.id, request).await?)))
}

async fn get_task(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<StewardTaskResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.steward.get_task(&user.id, &id).await?)))
}

async fn update_task(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateStewardTaskRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<StewardTaskResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state.steward.update_task(&user.id, &id, request).await?,
    )))
}

async fn bind_session(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<BindStewardTaskSessionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<StewardTaskResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state.steward.bind_session(&user.id, &id, request).await?,
    )))
}

async fn dispatch_task(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<DispatchStewardTaskRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<SendMessageResponse>>), ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let response = state.steward.dispatch(&user.id, &id, request).await?;
    Ok((StatusCode::ACCEPTED, Json(ApiResponse::ok(response))))
}

async fn ask_task(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<AskStewardTaskRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<StewardTaskInquiryResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(state.steward.ask(&user.id, &id, request).await?)))
}

async fn resume_task(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<ResumeStewardTaskRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<StewardTaskResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state.steward.resume(&user.id, &id, request).await?,
    )))
}
