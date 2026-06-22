//! Datadog CLI commands (read-only).

pub(crate) mod auth;
pub(crate) mod dashboard;
pub(crate) mod downtime;
pub(crate) mod events;
pub(crate) mod format;
pub(crate) mod helpers;
pub(crate) mod hosts;
pub(crate) mod logs;
pub(crate) mod metrics;
pub(crate) mod monitor;
pub(crate) mod slo;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::datadog::client::DatadogClient;

/// Datadog: read-only API operations.
#[derive(Parser)]
pub struct DatadogCommand {
    /// The Datadog subcommand to execute.
    #[command(subcommand)]
    pub command: DatadogSubcommands,
}

/// Datadog subcommands.
#[derive(Subcommand)]
pub enum DatadogSubcommands {
    /// Manages Datadog API credentials.
    Auth(auth::AuthCommand),
    /// Inspects Datadog dashboards.
    Dashboard(dashboard::DashboardCommand),
    /// Inspects Datadog scheduled downtimes.
    Downtime(downtime::DowntimeCommand),
    /// Inspects the Datadog events stream.
    Events(events::EventsCommand),
    /// Inspects Datadog reporting hosts.
    Hosts(hosts::HostsCommand),
    /// Searches Datadog logs.
    Logs(logs::LogsCommand),
    /// Queries Datadog metrics.
    Metrics(metrics::MetricsCommand),
    /// Inspects Datadog monitors.
    Monitor(monitor::MonitorCommand),
    /// Inspects Datadog Service Level Objectives.
    Slo(slo::SloCommand),
}

impl DatadogCommand {
    /// Executes the Datadog command.
    ///
    /// `auth` manages credentials and must run without them; every other
    /// subcommand needs an authenticated client, which is resolved **once**
    /// here and threaded down so each leaf takes `&DatadogClient` and stays
    /// free of process env (issue #1030).
    pub async fn execute(self) -> Result<()> {
        match self.command {
            DatadogSubcommands::Auth(cmd) => cmd.execute().await,
            data => {
                // The one `create_client` call for the read-only surface.
                let (client, _site) = helpers::create_client()?;
                data.dispatch(&client).await
            }
        }
    }
}

impl DatadogSubcommands {
    /// Routes a non-`Auth` subcommand against the shared client. Kept separate
    /// from credential resolution so it is testable without env (tests pass a
    /// client pointed at an unreachable URL). The `Auth` arm is unreachable
    /// because it is handled before client resolution in
    /// [`DatadogCommand::execute`].
    async fn dispatch(self, client: &DatadogClient) -> Result<()> {
        match self {
            Self::Auth(_) => {
                unreachable!("Auth is dispatched before client resolution")
            }
            Self::Dashboard(cmd) => cmd.execute(client).await,
            Self::Downtime(cmd) => cmd.execute(client).await,
            Self::Events(cmd) => cmd.execute(client).await,
            Self::Hosts(cmd) => cmd.execute(client).await,
            Self::Logs(cmd) => cmd.execute(client).await,
            Self::Metrics(cmd) => cmd.execute(client).await,
            Self::Monitor(cmd) => cmd.execute(client).await,
            Self::Slo(cmd) => cmd.execute(client).await,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::cli::datadog::format::OutputFormat;
    use crate::datadog::client::DatadogClient;

    /// A client pointed at an unreachable URL. Routing tests use it so a
    /// command runs through dispatch -> mid -> leaf to the HTTP layer and fails
    /// with a connection error — exercising the routing without touching
    /// credentials, the process environment, or a mock server.
    fn dead_client() -> DatadogClient {
        DatadogClient::new("http://127.0.0.1:1", "api", "app").unwrap()
    }

    #[test]
    fn datadog_subcommands_auth_variant() {
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Auth(auth::AuthCommand {
                command: auth::AuthSubcommands::Status(auth::StatusCommand),
            }),
        };
        assert!(matches!(cmd.command, DatadogSubcommands::Auth(_)));
    }

    #[test]
    fn datadog_subcommands_metrics_variant() {
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Metrics(metrics::MetricsCommand {
                command: metrics::MetricsSubcommands::Query(metrics::query::QueryCommand {
                    query: "m".into(),
                    from: "1h".into(),
                    to: None,
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, DatadogSubcommands::Metrics(_)));
    }

    #[tokio::test]
    async fn dispatch_routes_metrics_query() {
        let cmd = DatadogSubcommands::Metrics(metrics::MetricsCommand {
            command: metrics::MetricsSubcommands::Query(metrics::query::QueryCommand {
                query: "m".into(),
                from: "1h".into(),
                to: None,
                output: OutputFormat::Table,
            }),
        });
        // Verifies routing reaches the leaf's HTTP call without env.
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[tokio::test]
    async fn dispatch_routes_monitor_get() {
        let cmd = DatadogSubcommands::Monitor(monitor::MonitorCommand {
            command: monitor::MonitorSubcommands::Get(monitor::get::GetCommand {
                id: 1,
                output: OutputFormat::Table,
            }),
        });
        // Verifies routing reaches the leaf's HTTP call without env.
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[tokio::test]
    async fn dispatch_routes_monitor_list() {
        let cmd = DatadogSubcommands::Monitor(monitor::MonitorCommand {
            command: monitor::MonitorSubcommands::List(monitor::list::ListCommand {
                name: None,
                tags: None,
                monitor_tags: None,
                limit: 5,
                output: OutputFormat::Table,
            }),
        });
        // Verifies routing reaches the leaf's HTTP call without env.
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[tokio::test]
    async fn dispatch_routes_monitor_search() {
        let cmd = DatadogSubcommands::Monitor(monitor::MonitorCommand {
            command: monitor::MonitorSubcommands::Search(monitor::search::SearchCommand {
                query: "q".into(),
                limit: 5,
                output: OutputFormat::Table,
            }),
        });
        // Verifies routing reaches the leaf's HTTP call without env.
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[test]
    fn datadog_subcommands_dashboard_variant() {
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Dashboard(dashboard::DashboardCommand {
                command: dashboard::DashboardSubcommands::List(dashboard::list::ListCommand {
                    filter_shared: false,
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, DatadogSubcommands::Dashboard(_)));
    }

    #[tokio::test]
    async fn dispatch_routes_dashboard_list() {
        let cmd = DatadogSubcommands::Dashboard(dashboard::DashboardCommand {
            command: dashboard::DashboardSubcommands::List(dashboard::list::ListCommand {
                filter_shared: true,
                output: OutputFormat::Table,
            }),
        });
        // Verifies routing reaches the leaf's HTTP call without env.
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[tokio::test]
    async fn dispatch_routes_dashboard_get() {
        let cmd = DatadogSubcommands::Dashboard(dashboard::DashboardCommand {
            command: dashboard::DashboardSubcommands::Get(dashboard::get::GetCommand {
                id: "abc".into(),
                output: OutputFormat::Table,
            }),
        });
        // Verifies routing reaches the leaf's HTTP call without env.
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[test]
    fn datadog_subcommands_logs_variant() {
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Logs(logs::LogsCommand {
                command: logs::LogsSubcommands::Search(logs::search::SearchCommand {
                    filter: "*".into(),
                    from: "15m".into(),
                    to: "now".into(),
                    limit: 10,
                    sort: logs::search::SortArg::TimestampDesc,
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, DatadogSubcommands::Logs(_)));
    }

    #[tokio::test]
    async fn dispatch_routes_logs_search() {
        let cmd = DatadogSubcommands::Logs(logs::LogsCommand {
            command: logs::LogsSubcommands::Search(logs::search::SearchCommand {
                filter: "*".into(),
                from: "15m".into(),
                to: "now".into(),
                limit: 10,
                sort: logs::search::SortArg::TimestampDesc,
                output: OutputFormat::Table,
            }),
        });
        // Verifies routing reaches the leaf's HTTP call without env.
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[test]
    fn datadog_subcommands_events_variant() {
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Events(events::EventsCommand {
                command: events::EventsSubcommands::List(events::list::ListCommand {
                    filter: None,
                    from: "1h".into(),
                    to: "now".into(),
                    limit: 10,
                    sources: None,
                    tags: None,
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, DatadogSubcommands::Events(_)));
    }

    #[tokio::test]
    async fn dispatch_routes_events_list() {
        let cmd = DatadogSubcommands::Events(events::EventsCommand {
            command: events::EventsSubcommands::List(events::list::ListCommand {
                filter: None,
                from: "1h".into(),
                to: "now".into(),
                limit: 10,
                sources: None,
                tags: None,
                output: OutputFormat::Table,
            }),
        });
        // Verifies routing reaches the leaf's HTTP call without env.
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[test]
    fn datadog_subcommands_slo_variant() {
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Slo(slo::SloCommand {
                command: slo::SloSubcommands::List(slo::list::ListCommand {
                    tags: None,
                    query: None,
                    ids: None,
                    metrics_query: None,
                    limit: 5,
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, DatadogSubcommands::Slo(_)));
    }

    #[tokio::test]
    async fn dispatch_routes_slo_list() {
        let cmd = DatadogSubcommands::Slo(slo::SloCommand {
            command: slo::SloSubcommands::List(slo::list::ListCommand {
                tags: None,
                query: None,
                ids: None,
                metrics_query: None,
                limit: 5,
                output: OutputFormat::Table,
            }),
        });
        // Verifies routing reaches the leaf's HTTP call without env.
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[tokio::test]
    async fn dispatch_routes_slo_get() {
        let cmd = DatadogSubcommands::Slo(slo::SloCommand {
            command: slo::SloSubcommands::Get(slo::get::GetCommand {
                id: "abc".into(),
                output: OutputFormat::Table,
            }),
        });
        // Verifies routing reaches the leaf's HTTP call without env.
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[test]
    fn datadog_subcommands_hosts_variant() {
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Hosts(hosts::HostsCommand {
                command: hosts::HostsSubcommands::List(hosts::list::ListCommand {
                    filter: None,
                    from: None,
                    limit: 5,
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, DatadogSubcommands::Hosts(_)));
    }

    #[tokio::test]
    async fn dispatch_routes_hosts_list() {
        let cmd = DatadogSubcommands::Hosts(hosts::HostsCommand {
            command: hosts::HostsSubcommands::List(hosts::list::ListCommand {
                filter: None,
                from: None,
                limit: 5,
                output: OutputFormat::Table,
            }),
        });
        // Verifies routing reaches the leaf's HTTP call without env.
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[test]
    fn datadog_subcommands_downtime_variant() {
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Downtime(downtime::DowntimeCommand {
                command: downtime::DowntimeSubcommands::List(downtime::list::ListCommand {
                    active_only: false,
                    output: OutputFormat::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, DatadogSubcommands::Downtime(_)));
    }

    #[tokio::test]
    async fn dispatch_routes_downtime_list() {
        let cmd = DatadogSubcommands::Downtime(downtime::DowntimeCommand {
            command: downtime::DowntimeSubcommands::List(downtime::list::ListCommand {
                active_only: true,
                output: OutputFormat::Table,
            }),
        });
        // Verifies routing reaches the leaf's HTTP call without env.
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }

    #[tokio::test]
    async fn dispatch_routes_metrics_catalog_list() {
        let cmd = DatadogSubcommands::Metrics(metrics::MetricsCommand {
            command: metrics::MetricsSubcommands::Catalog(metrics::catalog::CatalogCommand {
                command: metrics::catalog::CatalogSubcommands::List(
                    metrics::catalog::list::ListCommand {
                        host: None,
                        from: None,
                        output: OutputFormat::Table,
                    },
                ),
            }),
        });
        // Verifies routing reaches the leaf's HTTP call without env.
        assert!(cmd.dispatch(&dead_client()).await.is_err());
    }
}
