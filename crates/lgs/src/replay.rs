use crate::error::AppError;
use crate::state::AppState;
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use serde_json::value::RawValue;
use std::sync::Arc;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        // Canonical Stake Engine replay path — mounted at the LGS root so the
        // game's `{rgs_url}/bet/replay/...` fetch lands here.
        .route(
            "/bet/replay/:game/:version/:mode/:event",
            get(replay_handler),
        )
        .with_state(state)
}

#[derive(Serialize)]
pub struct ReplayResponse {
    #[serde(rename = "payoutMultiplier")]
    pub payout_multiplier: f64,
    #[serde(rename = "costMultiplier")]
    pub cost_multiplier: f64,
    pub state: Arc<RawValue>,
}

async fn replay_handler(
    State(state): State<Arc<AppState>>,
    Path((game, _version, mode, event)): Path<(String, String, String, u32)>,
) -> Response {
    // `version` is accepted for protocol compatibility but unused — our LGS
    // serves a single math version per game folder.
    match state.engine.replay_event(&game, &mode, event).await {
        Ok(result) => Json(ReplayResponse {
            payout_multiplier: result.payout_multiplier as f64 / 100.0,
            cost_multiplier: result.cost_multiplier,
            state: result.state,
        })
        .into_response(),
        // The RGS answers an unknown game, mode or event with a plain 404 and
        // no JSON body; games are reviewed on how they handle it.
        Err(
            AppError::ModeNotFound { .. } | AppError::EventNotFound(_) | AppError::FileNotFound(_),
        ) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => e.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::tests::demo_state;

    async fn status(state: &Arc<AppState>, game: &str, mode: &str, event: u32) -> StatusCode {
        let path = Path((game.into(), "1".into(), mode.into(), event));
        replay_handler(State(state.clone()), path).await.status()
    }

    #[tokio::test]
    async fn unknown_game_mode_or_event_is_a_plain_404() {
        let tmp = tempfile::tempdir().expect("tmp");
        let state = demo_state(tmp.path());

        assert_eq!(status(&state, "demo", "base", 2).await, StatusCode::OK);
        assert_eq!(
            status(&state, "demo", "base", 99).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status(&state, "demo", "bonus", 2).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status(&state, "nogame", "base", 2).await,
            StatusCode::NOT_FOUND
        );
    }
}
