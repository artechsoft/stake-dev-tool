use crate::config;
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::types::{
    AuthenticateResponse, Balance, BalanceResponse, BetEventResponse, EndRoundResponse, EventEntry,
    PlayResponse, Round,
};
use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/rgs/:game/wallet/authenticate", post(authenticate))
        .route("/api/rgs/:game/wallet/balance", post(balance))
        .route("/api/rgs/:game/wallet/play", post(play))
        .route("/api/rgs/:game/wallet/end-round", post(end_round))
        .route("/api/rgs/:game/bet/event", post(bet_event))
        .with_state(state)
}

#[derive(Deserialize)]
struct AuthBody {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
    language: Option<String>,
}

async fn authenticate(
    State(state): State<Arc<AppState>>,
    Path(game): Path<String>,
    Json(body): Json<AuthBody>,
) -> AppResult<Json<AuthenticateResponse>> {
    let session_id = body.session_id.ok_or(AppError::MissingField("sessionID"))?;
    let session = state
        .sessions
        .get_or_create(&session_id, &game, body.language);

    let engine = state.engine.clone();
    let game_clone = game.clone();
    tokio::spawn(async move {
        if let Err(e) = engine.preload(&game_clone).await {
            tracing::warn!(?e, game = %game_clone, "preload failed");
        }
    });

    Ok(Json(AuthenticateResponse {
        balance: Balance {
            amount: session.balance,
            currency: session.currency,
        },
        round: session.active_round,
        config: config::auth_config(),
        meta: None,
    }))
}

#[derive(Deserialize)]
struct BalanceBody {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
}

async fn balance(
    State(state): State<Arc<AppState>>,
    Path(_game): Path<String>,
    Json(body): Json<BalanceBody>,
) -> AppResult<Json<BalanceResponse>> {
    let session_id = body.session_id.ok_or(AppError::MissingField("sessionID"))?;
    let session = state
        .sessions
        .get(&session_id)
        .ok_or(AppError::SessionNotFound)?;
    Ok(Json(BalanceResponse {
        balance: Balance {
            amount: session.balance,
            currency: session.currency,
        },
    }))
}

#[derive(Deserialize)]
struct PlayBody {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
    mode: Option<String>,
    amount: Option<u64>,
}

async fn play(
    State(state): State<Arc<AppState>>,
    Path(game): Path<String>,
    Json(body): Json<PlayBody>,
) -> AppResult<Json<PlayResponse>> {
    let session_id = body.session_id.ok_or(AppError::MissingField("sessionID"))?;
    let mode = body.mode.ok_or(AppError::MissingField("mode"))?;
    let amount = body.amount.ok_or(AppError::MissingField("amount"))?;

    let existing = state
        .sessions
        .get(&session_id)
        .ok_or(AppError::SessionNotFound)?;

    // The RGS allows one active round per player and rejects a second /play
    // until /end-round settles it. Rejecting here keeps the pending payout
    // intact and surfaces front ends that skip /end-round on a win.
    if existing.active_round.is_some() {
        return Err(AppError::InvalidRequest("round already active"));
    }
    if !(config::MIN_BET..=config::MAX_BET).contains(&amount) || amount % config::STEP_BET != 0 {
        return Err(AppError::InvalidRequest("invalid amount"));
    }

    let total_cost = state.engine.total_bet_cost(&game, &mode, amount).await?;

    state
        .sessions
        .deduct_bet(&session_id, total_cost)
        .ok_or(AppError::InsufficientBalance)?;

    // If a forced event is set for this mode, bypass the RNG and return it.
    let forced = state
        .forced_event
        .lock()
        .as_ref()
        .filter(|f| f.mode.eq_ignore_ascii_case(&mode))
        .cloned();

    let result = if let Some(f) = forced {
        state
            .engine
            .play_forced(&game, &mode, total_cost, f.event_id)
            .await?
    } else {
        state.engine.play_spin(&game, &mode, total_cost).await?
    };

    // Winnings are NOT credited here — the RGS contract credits payouts at
    // /end-round, so the balance returned by /play reflects only the bet
    // deduction. See end_round() below.
    state
        .sessions
        .set_last_event(&session_id, result.event_id, result.payout_multiplier);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let forced_flag = state
        .forced_event
        .lock()
        .as_ref()
        .is_some_and(|f| f.mode.eq_ignore_ascii_case(&mode));
    state.sessions.push_event(
        &session_id,
        EventEntry {
            event_id: result.event_id,
            mode: mode.clone(),
            bet_amount: total_cost,
            payout: result.payout,
            payout_multiplier: result.payout_multiplier,
            forced: forced_flag,
            at: now_ms,
        },
    );

    let round = Round {
        bet_id: state.sessions.next_bet_id(),
        // The RGS charges bonus-buy modes by `amount * mode_cost`, but the
        // round contract reports the player's selected base stake. This keeps
        // resumed `/authenticate` rounds from reopening bonus buys at the
        // inflated charged amount.
        amount,
        payout: result.payout,
        payout_multiplier: result.payout_multiplier as f64 / 100.0,
        // As on the RGS, a 0x round closes inside /play. A winning round
        // stays active until /end-round credits the payout.
        active: result.payout > 0,
        mode,
        event: Some(result.event_id.to_string()),
        state: result.state,
    };

    state
        .sessions
        .set_active_round(&session_id, round.active.then(|| round.clone()));

    let final_session = state
        .sessions
        .get(&session_id)
        .ok_or(AppError::SessionNotFound)?;

    Ok(Json(PlayResponse {
        balance: Balance {
            amount: final_session.balance,
            currency: final_session.currency,
        },
        round,
    }))
}

#[derive(Deserialize)]
struct EndRoundBody {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
}

async fn end_round(
    State(state): State<Arc<AppState>>,
    Path(_game): Path<String>,
    Json(body): Json<EndRoundBody>,
) -> AppResult<Json<EndRoundResponse>> {
    let session_id = body.session_id.ok_or(AppError::MissingField("sessionID"))?;
    let session = state
        .sessions
        .get(&session_id)
        .ok_or(AppError::SessionNotFound)?;

    // Credit the pending round's payout now. /play only deducts the bet and
    // stores the outcome on active_round; we pay it out here so the client's
    // balance animation (spin → credit) matches the Stake Engine RGS contract.
    // With no active round (a 0x round, or a repeated call) the RGS answers
    // ERR_VAL.
    let round = session
        .active_round
        .as_ref()
        .ok_or(AppError::InvalidRequest("no active round"))?;
    state.sessions.add_winnings(&session_id, round.payout);
    state.sessions.set_active_round(&session_id, None);

    let session = state
        .sessions
        .get(&session_id)
        .ok_or(AppError::SessionNotFound)?;
    Ok(Json(EndRoundResponse {
        balance: Balance {
            amount: session.balance,
            currency: session.currency,
        },
        round: None,
        config: config::auth_config(),
        meta: None,
    }))
}

#[derive(Deserialize)]
struct BetEventBody {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
    event: Option<String>,
}

async fn bet_event(
    State(state): State<Arc<AppState>>,
    Path(_game): Path<String>,
    Json(body): Json<BetEventBody>,
) -> AppResult<Json<BetEventResponse>> {
    let session_id = body.session_id.ok_or(AppError::MissingField("sessionID"))?;
    state
        .sessions
        .get(&session_id)
        .ok_or(AppError::SessionNotFound)?;
    Ok(Json(BetEventResponse { event: body.event }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::state::ForcedEvent;
    use crate::tenant::{TenantId, TenantRegistry};
    use std::io::Write;

    const BET: u64 = 1_000_000;

    /// One `base` mode with a 0x round (id 1) and a 2x round (id 2).
    pub(crate) fn demo_state(dir: &std::path::Path) -> Arc<AppState> {
        let game_dir = dir.join("demo");
        std::fs::create_dir_all(&game_dir).expect("create game dir");
        std::fs::write(
            game_dir.join("index.json"),
            r#"{"modes":[{"name":"base","cost":1,"events":"books_base.jsonl.zst","weights":"lookUpTable_base_0.csv"}]}"#,
        )
        .expect("write index");
        std::fs::write(game_dir.join("lookUpTable_base_0.csv"), "1,1,0\n2,1,200\n")
            .expect("write weights");
        let books = "{\"id\":1,\"events\":[],\"payoutMultiplier\":0}\n\
                     {\"id\":2,\"events\":[],\"payoutMultiplier\":200}\n";
        let mut enc = zstd::Encoder::new(Vec::new(), 0).expect("zstd encoder");
        enc.write_all(books.as_bytes()).expect("compress books");
        std::fs::write(
            game_dir.join("books_base.jsonl.zst"),
            enc.finish().expect("finish zstd"),
        )
        .expect("write books");
        TenantRegistry::new().get_or_create_disk(TenantId::from("test"), dir)
    }

    async fn auth(state: &Arc<AppState>) -> u64 {
        let body = AuthBody {
            session_id: Some("s1".into()),
            language: None,
        };
        authenticate(State(state.clone()), Path("demo".into()), Json(body))
            .await
            .expect("authenticate")
            .0
            .balance
            .amount
    }

    async fn play_as(
        state: &Arc<AppState>,
        mode: &str,
        amount: u64,
        event_id: Option<u32>,
    ) -> AppResult<Round> {
        *state.forced_event.lock() = event_id.map(|event_id| ForcedEvent {
            mode: mode.into(),
            event_id,
        });
        let body = PlayBody {
            session_id: Some("s1".into()),
            mode: Some(mode.into()),
            amount: Some(amount),
        };
        play(State(state.clone()), Path("demo".into()), Json(body))
            .await
            .map(|r| r.0.round)
    }

    async fn end(state: &Arc<AppState>) -> AppResult<u64> {
        let body = EndRoundBody {
            session_id: Some("s1".into()),
        };
        end_round(State(state.clone()), Path("demo".into()), Json(body))
            .await
            .map(|r| r.0.balance.amount)
    }

    #[tokio::test]
    async fn zero_win_round_closes_in_play() {
        let tmp = tempfile::tempdir().expect("tmp");
        let state = demo_state(tmp.path());
        let start = auth(&state).await;

        let round = play_as(&state, "base", BET, Some(1)).await.expect("play");
        assert!(!round.active);
        assert!(matches!(
            end(&state).await,
            Err(AppError::InvalidRequest("no active round"))
        ));
        // The next bet goes through without an end-round in between.
        play_as(&state, "base", BET, Some(1))
            .await
            .expect("second play");
        assert_eq!(
            state.sessions.get("s1").expect("session").balance,
            start - 2 * BET
        );
    }

    #[tokio::test]
    async fn winning_round_stays_active_until_end_round() {
        let tmp = tempfile::tempdir().expect("tmp");
        let state = demo_state(tmp.path());
        let start = auth(&state).await;

        let round = play_as(&state, "base", BET, Some(2)).await.expect("play");
        assert!(round.active);
        assert!(matches!(
            play_as(&state, "base", BET, Some(2)).await,
            Err(AppError::InvalidRequest("round already active"))
        ));
        assert_eq!(end(&state).await.expect("end-round"), start + BET);
    }

    #[tokio::test]
    async fn invalid_amount_and_unknown_mode_debit_nothing_and_case_is_ignored() {
        let tmp = tempfile::tempdir().expect("tmp");
        let state = demo_state(tmp.path());
        let start = auth(&state).await;

        for amount in [
            config::MIN_BET - 1,
            config::MAX_BET + config::STEP_BET,
            BET + 1,
        ] {
            assert!(matches!(
                play_as(&state, "base", amount, None).await,
                Err(AppError::InvalidRequest("invalid amount"))
            ));
        }
        // Mode names match ignoring case, as front ends send "BASE".
        play_as(&state, "BASE", BET, Some(1))
            .await
            .expect("upper-case mode");
        assert!(matches!(
            play_as(&state, "bonus", BET, None).await,
            Err(AppError::ModeNotFound { .. })
        ));
        assert_eq!(
            state.sessions.get("s1").expect("session").balance,
            start - BET
        );
    }
}
