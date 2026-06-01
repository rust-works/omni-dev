//! Renders the browser-side JS snippet that the operator pastes into a DevTools
//! console. The template is embedded at compile time; the WebSocket port and
//! session token are interpolated at print time.

/// The snippet template, with `__OMNI_BRIDGE_PORT__` / `__OMNI_BRIDGE_TOKEN__`
/// placeholders.
const TEMPLATE: &str = include_str!("../templates/browser-bridge.js");

/// Renders the snippet for the given WebSocket port and session token.
///
/// The port is interpolated as a bare numeric literal and the token inside the
/// single-quoted string literal already present in the template.
#[must_use]
pub fn render(ws_port: u16, token: &str) -> String {
    TEMPLATE
        .replace("__OMNI_BRIDGE_PORT__", &ws_port.to_string())
        .replace("__OMNI_BRIDGE_TOKEN__", token)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn render_interpolates_and_leaves_no_placeholders() {
        let out = render(9999, "deadbeef-token");
        assert!(out.contains("const PORT = 9999"));
        assert!(out.contains("'deadbeef-token'"));
        assert!(!out.contains("__OMNI_BRIDGE_PORT__"));
        assert!(!out.contains("__OMNI_BRIDGE_TOKEN__"));
    }

    #[test]
    fn render_uses_the_supplied_port() {
        let out = render(40000, "t");
        assert!(out.contains("const PORT = 40000"));
    }

    #[test]
    fn render_includes_binary_body_handling() {
        let out = render(9999, "t");
        assert!(out.contains("arrayBuffer"));
        assert!(out.contains("base64"));
        assert!(out.contains("btoa"));
    }

    #[test]
    fn render_includes_streaming() {
        let out = render(9999, "t");
        assert!(out.contains("getReader"));
        assert!(out.contains("activeStreams"));
        assert!(out.contains("cancel"));
    }
}
