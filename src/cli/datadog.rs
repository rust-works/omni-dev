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
    pub async fn execute(self) -> Result<()> {
        match self.command {
            DatadogSubcommands::Auth(cmd) => cmd.execute().await,
            DatadogSubcommands::Dashboard(cmd) => cmd.execute().await,
            DatadogSubcommands::Downtime(cmd) => cmd.execute().await,
            DatadogSubcommands::Events(cmd) => cmd.execute().await,
            DatadogSubcommands::Hosts(cmd) => cmd.execute().await,
            DatadogSubcommands::Logs(cmd) => cmd.execute().await,
            DatadogSubcommands::Metrics(cmd) => cmd.execute().await,
            DatadogSubcommands::Monitor(cmd) => cmd.execute().await,
            DatadogSubcommands::Slo(cmd) => cmd.execute().await,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::cli::datadog::format::OutputFormat;
    use crate::datadog::test_support::{with_empty_home, EnvGuard};

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
    async fn datadog_command_dispatches_auth_logout() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Auth(auth::AuthCommand {
                command: auth::AuthSubcommands::Logout(auth::LogoutCommand),
            }),
        };
        cmd.execute().await.unwrap();
    }

    #[tokio::test]
    async fn datadog_command_dispatches_metrics_query() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
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
        // Fails at credential loading, not at dispatch — which is what we're
        // verifying here: the Metrics arm is wired through.
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[tokio::test]
    async fn datadog_command_dispatches_monitor_get() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Monitor(monitor::MonitorCommand {
                command: monitor::MonitorSubcommands::Get(monitor::get::GetCommand {
                    id: 1,
                    output: OutputFormat::Table,
                }),
            }),
        };
        // Fails at credential loading, not at dispatch — verifies the Monitor
        // arm is wired through to a leaf command's `execute`.
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[tokio::test]
    async fn datadog_command_dispatches_monitor_list() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Monitor(monitor::MonitorCommand {
                command: monitor::MonitorSubcommands::List(monitor::list::ListCommand {
                    name: None,
                    tags: None,
                    monitor_tags: None,
                    limit: 5,
                    output: OutputFormat::Table,
                }),
            }),
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[tokio::test]
    async fn datadog_command_dispatches_monitor_search() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Monitor(monitor::MonitorCommand {
                command: monitor::MonitorSubcommands::Search(monitor::search::SearchCommand {
                    query: "q".into(),
                    limit: 5,
                    output: OutputFormat::Table,
                }),
            }),
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
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
    async fn datadog_command_dispatches_dashboard_list() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Dashboard(dashboard::DashboardCommand {
                command: dashboard::DashboardSubcommands::List(dashboard::list::ListCommand {
                    filter_shared: true,
                    output: OutputFormat::Table,
                }),
            }),
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[tokio::test]
    async fn datadog_command_dispatches_dashboard_get() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Dashboard(dashboard::DashboardCommand {
                command: dashboard::DashboardSubcommands::Get(dashboard::get::GetCommand {
                    id: "abc".into(),
                    output: OutputFormat::Table,
                }),
            }),
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
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
    async fn datadog_command_dispatches_logs_search() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
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
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
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
    async fn datadog_command_dispatches_events_list() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
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
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
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
    async fn datadog_command_dispatches_slo_list() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
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
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[tokio::test]
    async fn datadog_command_dispatches_slo_get() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Slo(slo::SloCommand {
                command: slo::SloSubcommands::Get(slo::get::GetCommand {
                    id: "abc".into(),
                    output: OutputFormat::Table,
                }),
            }),
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
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
    async fn datadog_command_dispatches_hosts_list() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
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
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
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
    async fn datadog_command_dispatches_downtime_list() {
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);
        let cmd = DatadogCommand {
            command: DatadogSubcommands::Downtime(downtime::DowntimeCommand {
                command: downtime::DowntimeSubcommands::List(downtime::list::ListCommand {
                    active_only: true,
                    output: OutputFormat::Table,
                }),
            }),
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }
}
