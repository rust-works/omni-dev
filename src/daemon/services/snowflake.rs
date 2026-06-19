//! The Snowflake daemon service.
//!
//! A thin adapter that hosts the account-agnostic [`SnowflakeEngine`] under the
//! daemon's lifecycle and exposes query/sessions/disconnect over the control
//! socket, plus a tray submenu.
//!
//! All real work (lazy multiplexed auth, per-query `USE …`, heartbeats, the
//! arbitrary-schema → JSON mapping) lives in [`crate::snowflake`]; this adapter
//! only routes ops and renders the menu/status. Unlike the bridge it persists no
//! secret to disk — sessions live only in memory.

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::{json, Value};

use crate::daemon::service::{DaemonService, MenuAction, MenuItem, MenuSnapshot, ServiceStatus};
use crate::snowflake::{QueryRequest, SnowflakeEngine, SnowflakeEngineConfig};

/// The Snowflake service name (the control-socket routing key).
pub const SERVICE_NAME: &str = "snowflake";

/// Hosts a [`SnowflakeEngine`] as a [`DaemonService`].
pub struct SnowflakeService {
    engine: SnowflakeEngine,
}

impl SnowflakeService {
    /// Creates the service. Cheap — no eager auth or I/O; each `(account, user)`
    /// session is authenticated lazily on its first query.
    #[must_use]
    pub fn new(config: SnowflakeEngineConfig) -> Self {
        Self {
            engine: SnowflakeEngine::new(config),
        }
    }
}

#[async_trait]
impl DaemonService for SnowflakeService {
    fn name(&self) -> &'static str {
        SERVICE_NAME
    }

    async fn handle(&self, op: &str, payload: Value) -> Result<Value> {
        match op {
            "query" => {
                let req: QueryRequest =
                    serde_json::from_value(payload).context("invalid `query` payload")?;
                if req.sql.trim().is_empty() {
                    bail!("`query` requires a non-empty `sql`");
                }
                self.engine.query(req).await
            }
            "sessions" => Ok(json!({ "sessions": self.engine.sessions() })),
            "disconnect" => {
                let account = payload
                    .get("account")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("`disconnect` requires `account`"))?;
                let user = payload
                    .get("user")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("`disconnect` requires `user`"))?;
                Ok(json!({ "disconnected": self.engine.disconnect(account, user) }))
            }
            other => bail!("unknown snowflake op: {other}"),
        }
    }

    fn menu(&self) -> MenuSnapshot {
        let sessions = self.engine.sessions();
        let mut items = Vec::new();
        if sessions.is_empty() {
            items.push(MenuItem::Label("No sessions".to_string()));
        } else {
            for session in &sessions {
                items.push(MenuItem::Label(format!(
                    "{} · {} · {}/{} sessions · {} queries",
                    session.account,
                    session.user,
                    session.sessions,
                    session.max_sessions,
                    session.query_count
                )));
                // One line per individual authenticated session (auth), with what
                // it's doing: the running query + elapsed when busy, else idle time.
                for member in &session.members {
                    let state = if let Some(running) = &member.running {
                        let secs = (Utc::now() - running.started_at).num_seconds().max(0);
                        format!("running {secs}s: {}", running.sql)
                    } else if member.busy {
                        "busy".to_string()
                    } else {
                        let idle = (Utc::now() - member.last_used).num_seconds().max(0);
                        format!("idle {idle}s · {} queries", member.query_count)
                    };
                    items.push(MenuItem::Label(format!(
                        "    #{} {} · {state}",
                        member.id,
                        member.context.summary(),
                    )));
                }
            }
            items.push(MenuItem::Separator);
            for session in &sessions {
                items.push(MenuItem::Action(MenuAction {
                    id: format!("disconnect:{}", session.id),
                    label: format!("Disconnect {} · {}", session.account, session.user),
                    enabled: true,
                }));
            }
            items.push(MenuItem::Action(MenuAction {
                id: "disconnect-all".to_string(),
                label: "Disconnect all".to_string(),
                enabled: true,
            }));
        }
        MenuSnapshot {
            title: "Snowflake".to_string(),
            items,
        }
    }

    async fn menu_action(&self, action_id: &str) -> Result<()> {
        if action_id == "disconnect-all" {
            self.engine.disconnect_all();
            return Ok(());
        }
        if let Some(id) = action_id.strip_prefix("disconnect:") {
            let id: u64 = id
                .parse()
                .with_context(|| format!("invalid session id in action {action_id}"))?;
            self.engine.disconnect_by_id(id);
            return Ok(());
        }
        bail!("unknown snowflake menu action: {action_id}")
    }

    async fn status(&self) -> ServiceStatus {
        let sessions = self.engine.sessions();
        let live: usize = sessions.iter().map(|s| s.sessions).sum();
        ServiceStatus {
            name: SERVICE_NAME.to_string(),
            healthy: true,
            summary: format!("{} pool(s), {live} session(s)", sessions.len()),
            detail: json!({ "sessions": sessions }),
        }
    }

    async fn shutdown(&self) {
        self.engine.shutdown().await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A service with no resolvable defaults, so query resolution fails before
    /// any network/auth — keeping these tests offline and deterministic.
    fn offline_service() -> SnowflakeService {
        SnowflakeService::new(SnowflakeEngineConfig::default())
    }

    #[tokio::test]
    async fn name_and_unknown_op() {
        let svc = offline_service();
        assert_eq!(svc.name(), "snowflake");
        assert!(svc.handle("frobnicate", Value::Null).await.is_err());
    }

    #[tokio::test]
    async fn sessions_op_is_empty_initially() {
        let svc = offline_service();
        let payload = svc.handle("sessions", Value::Null).await.unwrap();
        assert_eq!(payload, json!({ "sessions": [] }));
    }

    #[tokio::test]
    async fn empty_sql_is_rejected_before_auth() {
        let svc = offline_service();
        assert!(svc.handle("query", json!({ "sql": "   " })).await.is_err());
    }

    #[tokio::test]
    async fn query_without_account_errors_not_panics() {
        let svc = offline_service();
        // Non-empty SQL but no resolvable account: errors on resolution, no auth.
        let err = svc
            .handle("query", json!({ "sql": "SELECT 1" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("account"));
    }

    #[tokio::test]
    async fn disconnect_requires_account_and_user() {
        let svc = offline_service();
        assert!(svc.handle("disconnect", json!({})).await.is_err());
        assert!(svc
            .handle("disconnect", json!({ "account": "ACCT" }))
            .await
            .is_err());
        // Both present: evicts nothing on an empty engine, but succeeds.
        let payload = svc
            .handle("disconnect", json!({ "account": "ACCT", "user": "me" }))
            .await
            .unwrap();
        assert_eq!(payload, json!({ "disconnected": false }));
    }

    #[tokio::test]
    async fn menu_and_status_shape_with_no_sessions() {
        let svc = offline_service();
        let menu = svc.menu();
        assert_eq!(menu.title, "Snowflake");
        assert!(matches!(
            menu.items.first(),
            Some(MenuItem::Label(text)) if text == "No sessions"
        ));
        let status = svc.status().await;
        assert_eq!(status.name, "snowflake");
        assert!(status.healthy);
        assert_eq!(status.summary, "0 pool(s), 0 session(s)");
    }

    #[tokio::test]
    async fn menu_actions_route_and_reject_unknown() {
        let svc = offline_service();
        // Both forms are no-ops on an empty engine but must not error.
        svc.menu_action("disconnect-all").await.unwrap();
        svc.menu_action("disconnect:7").await.unwrap();
        assert!(svc.menu_action("disconnect:not-a-number").await.is_err());
        assert!(svc.menu_action("bogus").await.is_err());
        svc.shutdown().await;
    }
}
