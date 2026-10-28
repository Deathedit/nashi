mod convert;
mod detect;
mod jobs;

use std::{convert::Infallible, net::SocketAddr, path::PathBuf, time::Duration};

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, put},
};
use futures_util::{Stream, StreamExt};
use percent_encoding::{NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use tokio::io::AsyncWriteExt;
use tokio_stream::wrappers::WatchStream;
use tokio_util::io::ReaderStream;
use tower_http::{services::ServeDir, trace::TraceLayer};
use uuid::Uuid;

use crate::jobs::{AppState, Job, JobView, Platform, Stage};

struct AppError(StatusCode, String);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

fn not_found() -> AppError {
    AppError(StatusCode::NOT_FOUND, "no such job".into())
}

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nashi=info,tower_http=info".into()),
        )
        .init();

    let addr: SocketAddr = env_or("NASHI_ADDR", "127.0.0.1:8080".parse()?);
    let work_dir: PathBuf = env_or("NASHI_WORK_DIR", "work".into());
    let web_dir: PathBuf = env_or("NASHI_WEB_DIR", "web".into());
    let max_jobs: usize = env_or("NASHI_MAX_JOBS", 1);
    let ttl = Duration::from_secs(env_or("NASHI_TTL_SECS", 3600));

    let state = AppState::new(work_dir.clone(), max_jobs, ttl);
    state.clean_orphans().await?;

    let sweeper = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            sweeper.sweep_expired().await;
        }
    });

    let app = Router::new()
        .route("/api/jobs", put(create_job).get(list_jobs))
        .route("/api/jobs/{id}", get(get_job).delete(delete_job))
        .route("/api/jobs/{id}/events", get(job_events))
        .route("/api/jobs/{id}/download", get(download))
        .fallback_service(ServeDir::new(&web_dir))
        .layer(DefaultBodyLimit::disable())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(
        "nashi listening on http://{addr} (work dir {}, {max_jobs} concurrent job(s))",
        work_dir.display()
    );
    axum::serve(listener, app).await?;
    Ok(())
}

/// `PUT /api/jobs`: the raw ISO is the body, `X-Filename` (URI-encoded) names it,
/// `X-Platform` is `auto` / `ps2` / `x360`.
async fn create_job(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<JobView>, AppError> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("");
    let bad_request = |msg: String| AppError(StatusCode::BAD_REQUEST, msg);

    let raw_name = percent_decode_str(header("x-filename")).decode_utf8_lossy();
    let name = std::path::Path::new(raw_name.as_ref())
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !name.to_ascii_lowercase().ends_with(".iso") {
        return Err(bad_request(format!("{name:?} isn't an .iso file")));
    }
    let platform = Platform::from_header(header("x-platform")).map_err(bad_request)?;

    let job = state.create_job(name).await?;
    tracing::info!(id = %job.id, name = %job.name, "upload started");
    match receive_upload(&job, body).await {
        Ok(()) => {}
        Err(e) => {
            state.remove(job.id).await;
            let _ = tokio::fs::remove_dir_all(&job.dir).await;
            tracing::warn!(id = %job.id, "upload aborted: {}", e.1);
            return Err(e);
        }
    }

    let view = job.view();
    tokio::spawn(convert::run(state, job, platform));
    Ok(Json(view))
}

async fn receive_upload(job: &Job, body: Body) -> Result<(), AppError> {
    let mut file = tokio::fs::File::create(job.input_path()).await?;
    let mut stream = body.into_data_stream();
    loop {
        let chunk = tokio::select! {
            chunk = stream.next() => chunk,
            _ = job.cancel.cancelled() => {
                return Err(AppError(StatusCode::GONE, "cancelled".into()));
            }
        };
        match chunk {
            Some(Ok(bytes)) => file.write_all(&bytes).await?,
            Some(Err(e)) => return Err(AppError(StatusCode::BAD_REQUEST, e.to_string())),
            None => break,
        }
    }
    file.flush().await?;
    Ok(())
}

async fn list_jobs(State(state): State<AppState>) -> Json<Vec<JobView>> {
    Json(state.list().await)
}

async fn get_job(State(state): State<AppState>, Path(id): Path<Uuid>) -> Result<Json<JobView>, AppError> {
    let job = state.get(id).await.ok_or_else(not_found)?;
    Ok(Json(job.view()))
}

/// Cancels a running job or deletes a finished one's output.
async fn delete_job(State(state): State<AppState>, Path(id): Path<Uuid>) -> Result<StatusCode, AppError> {
    let job = state.remove(id).await.ok_or_else(not_found)?;
    if job.view().stage.is_finished() {
        let _ = tokio::fs::remove_dir_all(&job.dir).await;
    } else {
        // The upload handler / conversion task notices and cleans up after itself.
        job.cancel.cancel();
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Server-sent events with the job's latest state on every change.
async fn job_events(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    let job = state.get(id).await.ok_or_else(not_found)?;
    let stream = WatchStream::new(job.subscribe()).map(|view| {
        Ok(Event::default()
            .json_data(&view)
            .unwrap_or_else(|_| Event::default()))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn download(State(state): State<AppState>, Path(id): Path<Uuid>) -> Result<Response, AppError> {
    let job = state.get(id).await.ok_or_else(not_found)?;
    let view = job.view();
    let (Stage::Done, Some(platform)) = (view.stage, view.platform) else {
        return Err(AppError(StatusCode::CONFLICT, "job isn't finished".into()));
    };
    job.touch();

    let file = tokio::fs::File::open(job.output_path(platform)).await?;
    let len = file.metadata().await?.len();
    let name = job.download_name(platform);
    let ascii_name: String = name
        .chars()
        .map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' })
        .collect();
    let disposition = format!(
        "attachment; filename=\"{ascii_name}\"; filename*=UTF-8''{}",
        utf8_percent_encode(&name, NON_ALPHANUMERIC)
    );

    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_LENGTH, len.to_string()),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        Body::from_stream(ReaderStream::with_capacity(file, 1 << 20)),
    )
        .into_response())
}
