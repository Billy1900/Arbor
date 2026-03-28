// re-exported from attach.rs — nothing else needed in M1
use axum::Router;
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
}
