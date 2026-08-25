use crate::server::common::audio_normalization::AudioSampleStore;
use crate::server::errors::report_to_response;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use std::path::PathBuf;

pub fn audio_normalization_router(working_directory: PathBuf) -> Router<()> {
    Router::new()
        .route("/v1/audio-normalization/sample/status", get(status))
        .route(
            "/v1/audio-normalization/sample",
            get(sample).delete(delete_sample),
        )
        .route(
            "/v1/audio-normalization/sample/capture",
            post(arm_capture).delete(cancel_capture),
        )
        .with_state(AudioSampleStore::for_working_directory(working_directory))
}

async fn status(State(store): State<AudioSampleStore>) -> Response {
    match store.status().await {
        Ok(status) => Json(status).into_response(),
        Err(error) => report_to_response(error),
    }
}

async fn sample(State(store): State<AudioSampleStore>) -> Response {
    match tokio::fs::read(store.sample_path()).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "audio/mp4"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            StatusCode::NOT_FOUND.into_response()
        }
        Err(error) => report_to_response(error_stack::Report::new(
            crate::server::errors::AppError::Custom(format!("read audio sample failed: {error}")),
        )),
    }
}

async fn arm_capture(State(store): State<AudioSampleStore>) -> Response {
    unit(store.arm_capture().await)
}

async fn cancel_capture(State(store): State<AudioSampleStore>) -> Response {
    unit(store.cancel_capture().await)
}

async fn delete_sample(State(store): State<AudioSampleStore>) -> Response {
    unit(store.delete_sample().await)
}

fn unit(result: crate::server::errors::AppResult<()>) -> Response {
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => report_to_response(error),
    }
}
