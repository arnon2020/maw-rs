//! `GET /api/oracles` and `GET /api/config` — the roster maw-ui-lite's summon
//! panel is built from.
//!
//! The client tries `/api/oracles` and falls back to `/api/config`
//! (`maw-ui-lite/src/lib/oracleRegistry.ts`). maw-js answers only the fallback;
//! both are served here so either order resolves. Payload construction lives in
//! `core_impl/serve_oracle_registry.rs` — this module is routing only.

use super::ServecoreModuleRegistration;
use crate::serve_core::ServecoreLifecycleModule;
use axum::{http::StatusCode, response::IntoResponse, routing::get, Extension, Json, Router};
use serde_json::{json, Value};
use std::sync::Arc;

type OracleRegistryHandler = Arc<dyn Fn() -> Result<Value, String> + Send + Sync>;

#[derive(Clone)]
struct OracleRegistryProvider {
    oracles: OracleRegistryHandler,
    config: OracleRegistryHandler,
}

#[must_use]
pub fn oracles_lifecycle_module() -> ServecoreLifecycleModule {
    ServecoreLifecycleModule {
        name: "oracles".to_owned(),
        weight: 50,
    }
}

#[must_use]
pub fn oracles_registration<S>() -> ServecoreModuleRegistration<S>
where
    S: Clone + Send + Sync + 'static,
{
    ServecoreModuleRegistration {
        lifecycle: oracles_lifecycle_module(),
        mount: oracles_mount,
    }
}

pub fn oracles_mount<S>(router: Router<S>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    oracles_mount_with_provider(
        router,
        OracleRegistryProvider {
            oracles: Arc::new(crate::core_impl::serveoracles_http_payload_read_only),
            config: Arc::new(crate::core_impl::serveconfig_http_payload_read_only),
        },
    )
}

fn oracles_mount_with_provider<S>(router: Router<S>, provider: OracleRegistryProvider) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router
        .route("/api/oracles", get(oracles_get))
        .route("/api/config", get(oracles_config_get))
        .layer(Extension(provider))
}

async fn oracles_get(Extension(provider): Extension<OracleRegistryProvider>) -> impl IntoResponse {
    oracles_render((provider.oracles.as_ref())())
}

async fn oracles_config_get(
    Extension(provider): Extension<OracleRegistryProvider>,
) -> impl IntoResponse {
    oracles_render((provider.config.as_ref())())
}

fn oracles_render(payload: Result<Value, String>) -> axum::response::Response {
    match payload {
        Ok(value) => Json(value).into_response(),
        Err(message) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": message})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve_core::servecore_apply_pipeline;
    use std::{net::Ipv4Addr, time::Duration};
    use tokio::sync::oneshot;

    fn oracles_provider(
        oracles: impl Fn() -> Result<Value, String> + Send + Sync + 'static,
        config: impl Fn() -> Result<Value, String> + Send + Sync + 'static,
    ) -> OracleRegistryProvider {
        OracleRegistryProvider {
            oracles: Arc::new(oracles),
            config: Arc::new(config),
        }
    }

    // The fakes intentionally keep the same Result shape as real providers.
    #[allow(clippy::unnecessary_wraps)]
    fn oracles_fake_roster() -> Result<Value, String> {
        Ok(json!({"oracles": ["atlas", "hound"], "count": 2, "version": "1.2.3"}))
    }

    #[allow(clippy::unnecessary_wraps)]
    fn oracles_fake_config() -> Result<Value, String> {
        Ok(json!({
            "agents": {"atlas": "local", "hound-oracle": "local"},
            "sessions": {},
            "commands": {},
            "env": {},
            "envMasked": {"OPENAI_API_KEY": "****...mnop"},
            "node": "local"
        }))
    }

    fn oracles_failing_payload() -> Result<Value, String> {
        Err("config unreadable".to_owned())
    }

    #[allow(clippy::unnecessary_wraps)]
    fn oracles_empty_roster() -> Result<Value, String> {
        Ok(json!({"oracles": [], "count": 0, "version": "1.2.3"}))
    }

    async fn oracles_spawn(provider: OracleRegistryProvider) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let router = oracles_mount_with_provider(Router::new(), provider);
        let app = servecore_apply_pipeline(router);
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let server = axum::serve(listener, app).with_graceful_shutdown(async move {
                let _ = rx.await;
            });
            server.await.expect("server");
        });
        std::mem::forget(tx);
        addr
    }

    fn oracles_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("client")
    }

    fn oracles_config_fixture(name: &str, content: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "maw-rs-oracles-routes-{name}-{}",
            std::process::id()
        ));
        let config_dir = root.join("config");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&config_dir).expect("config dir");
        let path = config_dir.join("maw.config.json");
        std::fs::write(&path, content).expect("config");
        path
    }

    #[test]
    fn oracles_lifecycle_matches_public_module_contract() {
        let module = oracles_lifecycle_module();
        assert_eq!(module.name, "oracles");
        assert_eq!(module.weight, 50);
    }

    #[tokio::test]
    async fn oracles_route_is_public_and_returns_the_roster() {
        let addr = oracles_spawn(oracles_provider(oracles_fake_roster, oracles_fake_config)).await;
        let response = oracles_client()
            .get(format!("http://{addr}/api/oracles"))
            .send()
            .await
            .expect("oracles");
        assert_eq!(response.status(), StatusCode::OK);
        let payload = response.json::<Value>().await.expect("json");
        // The client reads payload.oracles ?? payload.agents ?? payload.names.
        assert_eq!(payload["oracles"], json!(["atlas", "hound"]));
        assert_eq!(payload["count"], 2);
    }

    #[tokio::test]
    async fn oracles_config_route_is_public_and_keeps_agents_a_map() {
        let addr = oracles_spawn(oracles_provider(oracles_fake_roster, oracles_fake_config)).await;
        let response = oracles_client()
            .get(format!("http://{addr}/api/config"))
            .send()
            .await
            .expect("config");
        assert_eq!(response.status(), StatusCode::OK);
        let payload = response.json::<Value>().await.expect("json");
        assert!(payload["agents"].is_object());
        assert_eq!(payload["env"], json!({}));
    }

    #[tokio::test]
    async fn oracles_route_reports_a_read_failure_instead_of_an_empty_roster() {
        // An empty 200 would be worse than a 500: the client stops at the first
        // ok response, so it would render "no agents" as if that were the truth.
        let addr = oracles_spawn(oracles_provider(
            oracles_failing_payload,
            oracles_failing_payload,
        ))
        .await;
        let response = oracles_client()
            .get(format!("http://{addr}/api/oracles"))
            .send()
            .await
            .expect("oracles");
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let payload = response.json::<Value>().await.expect("json");
        assert_eq!(payload["error"], "config unreadable");
    }

    #[tokio::test]
    async fn oracles_route_real_provider_reports_corrupt_config_as_5xx() {
        let path = oracles_config_fixture("corrupt-provider", "{ invalid json");
        let root = path
            .parent()
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf)
            .expect("fixture root");
        let config_path = path.clone();
        let addr = oracles_spawn(oracles_provider(
            move || crate::core_impl::serveoracles_http_payload_from_config_file(&config_path),
            oracles_fake_config,
        ))
        .await;
        let response = oracles_client()
            .get(format!("http://{addr}/api/oracles"))
            .send()
            .await
            .expect("oracles");
        assert!(
            response.status().is_server_error(),
            "status should be 5xx, got {}",
            response.status()
        );
        let payload = response.json::<Value>().await.expect("json");
        assert!(
            payload["error"]
                .as_str()
                .is_some_and(|message| message.contains("failed to parse config JSON")),
            "{payload}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn oracles_route_keeps_agentless_config_as_empty_ok_roster() {
        let addr = oracles_spawn(oracles_provider(oracles_empty_roster, oracles_fake_config)).await;
        let response = oracles_client()
            .get(format!("http://{addr}/api/oracles"))
            .send()
            .await
            .expect("oracles");
        assert_eq!(response.status(), StatusCode::OK);
        let payload = response.json::<Value>().await.expect("json");
        assert_eq!(payload["oracles"], json!([]));
        assert_eq!(payload["count"], 0);
    }
}
