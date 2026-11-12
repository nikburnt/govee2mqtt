use super::*;
use crate::service::state::State as ServiceState;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
enum Mode {
    Good,
    RejectOnce(StatusCode),
    RejectAlways,
    ServerError,
    Forbidden,
    RateLimited,
    InvalidPayload,
    Verify,
    InvalidCode,
}

struct Mock {
    mode: Mode,
    login_count: usize,
    community_count: usize,
    verification_count: usize,
    scene_count: usize,
    token_ttl: u32,
    tokens: Vec<String>,
}

async fn handle(State(mock): State<Arc<Mutex<Mock>>>, request: Request<Body>) -> Response {
    let path = request.uri().path().to_string();
    let token = request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let mut m = mock.lock().unwrap();
    match path.as_str() {
        "/os/v1/login" => {
            m.community_count += 1;
            Json(json!({"status":200,"message":"ok","data":{"email":"test@example.com","expiredAt":4102444800000_u64,"headerUrl":"","id":1,"nickName":"test","token":"wrong-community-token"}})).into_response()
        }
        "/account/rest/account/v2/login" => {
            m.login_count += 1;
            let status = match m.mode {
                Mode::Verify => 454,
                Mode::InvalidCode => 455,
                _ => 200,
            };
            if status != 200 {
                return Json(json!({"status":status,"message":"Verification required"}))
                    .into_response();
            }
            Json(json!({"status":200,"message":"ok","client":{"A":"a","B":"b","accountId":1,"client":"test","isSavvyUser":false,"token":format!("account-token-{}",m.login_count),"tokenExpireCycle":m.token_ttl,"topic":"test"}})).into_response()
        }
        "/account/rest/account/v1/verification" => {
            m.verification_count += 1;
            Json(json!({"status":200})).into_response()
        }
        "/bff-app/v1/exec-plat/home" => {
            m.scene_count += 1;
            m.tokens.push(token.clone());
            let status = match m.mode {
                Mode::RejectOnce(status) if m.scene_count == 1 => Some(status),
                Mode::RejectAlways => Some(StatusCode::OK),
                _ if token != format!("Bearer account-token-{}", m.login_count) => {
                    Some(StatusCode::OK)
                }
                _ => None,
            };
            if let Some(http_status) = status {
                return (
                    http_status,
                    Json(json!({"status":401,"message":"authorization token is invalid"})),
                )
                    .into_response();
            }
            if matches!(m.mode, Mode::ServerError) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"status":500,"message":"unavailable"})),
                )
                    .into_response();
            }
            if matches!(m.mode, Mode::Forbidden) {
                return Json(json!({"status":403,"message":"forbidden"})).into_response();
            }
            if matches!(m.mode, Mode::RateLimited) {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({"status":429,"message":"rate limited"})),
                )
                    .into_response();
            }
            if matches!(m.mode, Mode::InvalidPayload) {
                return Json(json!({"status":200,"message":"ok","data":{"components":"invalid"}}))
                    .into_response();
            }
            Json(
                serde_json::from_str::<JsonValue>(include_str!(
                    "../test-data/undoc-one-click.json"
                ))
                .unwrap(),
            )
            .into_response()
        }
        _ => (StatusCode::NOT_FOUND, Json(json!({"status":404}))).into_response(),
    }
}

fn mode(mock: &Mutex<Mock>, mode: Mode) -> usize {
    let mut m = mock.lock().unwrap();
    m.mode = mode;
    m.scene_count = 0;
    m.tokens.clear();
    m.login_count
}

#[test]
fn scene_auth_recovery_uses_an_isolated_cache() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("govee-auth-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir)?;
    let status = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "undoc_api::auth_tests::scene_auth_cases",
            "--ignored",
            "--nocapture",
        ])
        .env("GOVEE_CACHE_DIR", &dir)
        .env("GOVEE_AUTH_TEST_CHILD", "1")
        .status()?;
    std::fs::remove_dir_all(dir)?;
    anyhow::ensure!(
        status.success(),
        "isolated scene authentication regression failed"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "runs automatically in the isolated-cache parent test"]
async fn scene_auth_cases() -> anyhow::Result<()> {
    anyhow::ensure!(
        std::env::var("GOVEE_AUTH_TEST_CHILD").as_deref() == Ok("1"),
        "isolated test process required"
    );
    let mock = Arc::new(Mutex::new(Mock {
        mode: Mode::Good,
        login_count: 0,
        community_count: 0,
        verification_count: 0,
        scene_count: 0,
        token_ttl: 3600,
        tokens: vec![],
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let mut client = GoveeUndocumentedApi::new("test@example.com", "not-a-real-password");
    client.test_origin = Some(format!("http://{}", listener.local_addr()?));
    let app = Router::new().fallback(any(handle)).with_state(mock.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let path = crate::cache::cache_dir().join("saved-scenes.sqlite");
    let state = ServiceState::new();
    state.restore_offline(&path).await?;
    state.set_undoc_client(client.clone()).await;

    let result = state.refresh_one_clicks().await;
    anyhow::ensure!(
        result.is_ok(),
        "scene refresh must use an accepted account token, not Community credentials: {result:?}"
    );
    anyhow::ensure!(
        mock.lock().unwrap().community_count == 0,
        "scene refresh used the wrong login endpoint"
    );
    let saved = serde_json::to_value(state.saved_one_clicks().await)?;
    anyhow::ensure!(!saved.as_array().unwrap().is_empty(), "no scenes imported");
    let logins = mode(&mock, Mode::Good);
    state.refresh_one_clicks().await?;
    anyhow::ensure!(
        mock.lock().unwrap().login_count == logins,
        "valid session was not reused"
    );

    for http_status in [StatusCode::UNAUTHORIZED, StatusCode::OK] {
        let before = mode(&mock, Mode::RejectOnce(http_status));
        state.refresh_one_clicks().await?;
        let m = mock.lock().unwrap();
        anyhow::ensure!(
            m.login_count == before + 1 && m.scene_count == 2,
            "401 did not renew exactly once"
        );
        anyhow::ensure!(m.tokens[0] != m.tokens[1], "rejected token was reused");
    }

    let before = mode(&mock, Mode::RejectAlways);
    anyhow::ensure!(
        state.refresh_one_clicks().await.is_err(),
        "persistent 401 was hidden"
    );
    {
        let m = mock.lock().unwrap();
        anyhow::ensure!(
            m.scene_count == 2 && m.login_count == before + 1,
            "unbounded authentication retry"
        );
    }
    anyhow::ensure!(
        serde_json::to_value(state.saved_one_clicks().await)? == saved,
        "401 replaced saved scenes"
    );

    for mode_value in [
        Mode::ServerError,
        Mode::Forbidden,
        Mode::RateLimited,
        Mode::InvalidPayload,
    ] {
        let before = mode(&mock, mode_value);
        anyhow::ensure!(
            state.refresh_one_clicks().await.is_err(),
            "non-auth failure was hidden"
        );
        {
            let m = mock.lock().unwrap();
            anyhow::ensure!(
                m.scene_count == 1 && m.login_count <= before + 1,
                "non-auth failure caused repeated logins"
            );
        }
        anyhow::ensure!(
            serde_json::to_value(state.saved_one_clicks().await)? == saved,
            "failure replaced saved scenes"
        );
    }

    for login_mode in [Mode::Verify, Mode::InvalidCode] {
        crate::cache::invalidate_key("undoc-api", "account-info")?;
        let before = mode(&mock, login_mode);
        anyhow::ensure!(
            state.refresh_one_clicks().await.is_err(),
            "2FA requirement was hidden"
        );
        {
            let m = mock.lock().unwrap();
            anyhow::ensure!(
                m.login_count == before + 1 && m.scene_count == 0,
                "2FA caused a login loop"
            );
        }
        anyhow::ensure!(
            serde_json::to_value(state.saved_one_clicks().await)? == saved,
            "2FA failure discarded saved scenes"
        );
    }
    anyhow::ensure!(
        mock.lock().unwrap().verification_count == 1,
        "unexpected verification email requests"
    );

    crate::cache::invalidate_key("undoc-api", "account-info")?;
    mode(&mock, Mode::Good);
    mock.lock().unwrap().token_ttl = 0;
    state.refresh_one_clicks().await?;
    let before = mock.lock().unwrap().login_count;
    state.refresh_one_clicks().await?;
    anyhow::ensure!(
        mock.lock().unwrap().login_count == before + 1,
        "expired session was not renewed"
    );
    drop(state);
    let restored = ServiceState::new();
    restored.restore_offline(&path).await?;
    anyhow::ensure!(
        serde_json::to_value(restored.saved_one_clicks().await)? == saved,
        "saved scenes changed after reopening storage"
    );
    server.abort();
    Ok(())
}
