//! InnerTube client variants and the default fallback chain.
//!
//! YouTube's `/youtubei/v1/player` endpoint evaluates `playabilityStatus` per
//! `client.clientName` / `clientVersion` pair. A video that returns
//! `UNPLAYABLE` for the **WEB** client often returns `OK` for one of the
//! mobile / VR / embedded clients, because each variant skips a different
//! subset of YouTube's gating logic (JS-derived signatures, sign-in checks,
//! music-content blocks, …). The behaviour is the same workaround `yt-dlp`
//! uses internally — see its `youtube/_base.py` for the canonical client
//! list. Values pinned here drift over months; refresh if `/player` starts
//! returning empty `playerResponse` envelopes for healthy videos.
//!
//! The chain is walked by [`super::Youtube::load_player_response`] only when
//! the previous client returned a *retryable* refusal (see
//! [`super::is_retryable_refusal`]). Healthy videos answer on the first
//! client and pay no extra request volume.
//!
//! Plain `ANDROID` is intentionally absent — YouTube has tightened it over
//! the past year and yt-dlp has been moving away from it as a default.

use clap::ValueEnum;
use serde_json::{json, Value};

/// One InnerTube client variant.
///
/// Variants are ordered to match [`DEFAULT_CHAIN`]: WEB first (cheapest,
/// works for most public videos), then non-WEB clients that recover
/// WEB-specific refusals.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum InnertubeClient {
    /// Desktop web. Default; fails on videos that require JS-derived
    /// signatures or sign-in.
    Web,
    /// Android VR / Quest. Currently the most reliable recovery client for
    /// `UNPLAYABLE` on WEB.
    AndroidVr,
    /// TV HTML5 embedded player. Often unblocks age-gated content.
    TvEmbedded,
    /// iOS native. Last-resort fallback when the Android-family clients are
    /// also throttled.
    Ios,
}

/// Per-client request-shaping data used to build the InnerTube POST.
#[derive(Clone, Debug)]
pub struct ClientContext {
    /// `client.clientName` — uppercase, stable string YouTube switches on.
    pub name: &'static str,
    /// `client.clientVersion` — drifts over months.
    pub version: &'static str,
    /// Public InnerTube API key forwarded as `?key=`. Long-stable; not a
    /// credential. The same key works for every variant we emit, but is
    /// kept per-client so future drift is local.
    pub api_key: &'static str,
    /// `User-Agent` advertised on the request. YouTube's bot detection
    /// cross-checks UA against `clientName`; mismatches trigger
    /// `LOGIN_REQUIRED`.
    pub user_agent: &'static str,
    /// Extra fields merged into `context.client` (e.g. `androidSdkVersion`,
    /// `deviceModel`). `None` when the client needs no extras.
    pub extra_context: Option<Value>,
}

/// Public WEB-client API key. Long-stable across years, embedded in the
/// YouTube watch page's bootstrapped config.
pub(crate) const WEB_API_KEY: &str = "AIzaSyAO_FJ2SlqU8Q4STEHLGCilw_Y9_11qcW8";

/// User-Agent for the WEB client. A recent desktop Chrome string maximises
/// compatibility with the WEB context InnerTube expects.
const WEB_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// User-Agent for the Android VR client.
const ANDROID_VR_USER_AGENT: &str =
    "com.google.android.apps.youtube.vr.oculus/1.57.29 (Linux; U; Android 12; \
     Quest 3) gzip";

/// User-Agent for the TV embedded client. Mirrors a recent Smart TV YouTube
/// app build that YouTube's bot-detection treats as a first-class client.
const TV_EMBEDDED_USER_AGENT: &str = "Mozilla/5.0 (PlayStation; PlayStation 4/12.00) \
     AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.0 Safari/605.1.15";

/// User-Agent for the iOS client.
const IOS_USER_AGENT: &str =
    "com.google.ios.youtube/19.45.4 (iPhone16,2; U; CPU iOS 18_1_0 like Mac OS X;)";

impl InnertubeClient {
    /// Per-variant request-shaping data.
    pub fn context(self) -> ClientContext {
        match self {
            Self::Web => ClientContext {
                name: "WEB",
                version: "2.20250101.00.00",
                api_key: WEB_API_KEY,
                user_agent: WEB_USER_AGENT,
                extra_context: None,
            },
            Self::AndroidVr => ClientContext {
                name: "ANDROID_VR",
                version: "1.57.29",
                api_key: WEB_API_KEY,
                user_agent: ANDROID_VR_USER_AGENT,
                extra_context: Some(json!({
                    "androidSdkVersion": 32,
                    "deviceMake": "Oculus",
                    "deviceModel": "Quest 3",
                    "osName": "Android",
                    "osVersion": "12L",
                })),
            },
            Self::TvEmbedded => ClientContext {
                name: "TVHTML5_SIMPLY_EMBEDDED_PLAYER",
                version: "2.0",
                api_key: WEB_API_KEY,
                user_agent: TV_EMBEDDED_USER_AGENT,
                extra_context: None,
            },
            Self::Ios => ClientContext {
                name: "IOS",
                version: "19.45.4",
                api_key: WEB_API_KEY,
                user_agent: IOS_USER_AGENT,
                extra_context: Some(json!({
                    "deviceMake": "Apple",
                    "deviceModel": "iPhone16,2",
                    "osName": "iPhone",
                    "osVersion": "18.1.0.22B83",
                })),
            },
        }
    }
}

/// Default fallback chain walked by [`super::Youtube::load_player_response`].
///
/// Order is "cheapest first, broadest unblock last": WEB answers most
/// videos in one request; the remaining variants recover progressively
/// stricter refusals.
pub const DEFAULT_CHAIN: &[InnertubeClient] = &[
    InnertubeClient::Web,
    InnertubeClient::AndroidVr,
    InnertubeClient::TvEmbedded,
    InnertubeClient::Ios,
];

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn default_chain_starts_with_web() {
        assert_eq!(DEFAULT_CHAIN.first(), Some(&InnertubeClient::Web));
    }

    #[test]
    fn default_chain_has_no_duplicates() {
        let mut seen = Vec::new();
        for client in DEFAULT_CHAIN {
            assert!(!seen.contains(client), "duplicate variant in chain");
            seen.push(*client);
        }
    }

    #[test]
    fn every_variant_has_non_empty_strings() {
        for client in DEFAULT_CHAIN {
            let ctx = client.context();
            assert!(!ctx.name.is_empty());
            assert!(!ctx.version.is_empty());
            assert!(!ctx.api_key.is_empty());
            assert!(!ctx.user_agent.is_empty());
        }
    }

    #[test]
    fn client_names_are_unique() {
        let mut names = Vec::new();
        for client in DEFAULT_CHAIN {
            let ctx = client.context();
            assert!(
                !names.contains(&ctx.name),
                "duplicate clientName: {}",
                ctx.name
            );
            names.push(ctx.name);
        }
    }

    #[test]
    fn web_client_has_no_extra_context() {
        assert!(InnertubeClient::Web.context().extra_context.is_none());
    }

    #[test]
    fn android_vr_client_carries_android_sdk_version() {
        let ctx = InnertubeClient::AndroidVr.context();
        let extra = ctx.extra_context.as_ref().expect("android_vr has extras");
        assert_eq!(extra["androidSdkVersion"], 32);
        assert_eq!(extra["osName"], "Android");
    }

    #[test]
    fn ios_client_carries_ios_device_metadata() {
        let ctx = InnertubeClient::Ios.context();
        let extra = ctx.extra_context.as_ref().expect("ios has extras");
        assert_eq!(extra["deviceMake"], "Apple");
        assert_eq!(extra["osName"], "iPhone");
    }
}
