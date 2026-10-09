//! Who may do what at the gate. This machine and `trusted` addresses may do everything; other devices need an API
//! key for anything beyond the page itself; the router's built-in tools act on this machine, so other devices may use
//! them only with a key made with `--allow tools`.

use hyper::StatusCode;

/// Who is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requester {
    /// This machine, or an address in `trusted`.
    Local,
    /// Another device with a valid key.
    Key { allow_tools: bool },
    /// Another device without a valid key.
    Stranger,
}

/// Whether `requester` may make this request, or the status that refuses it.
pub fn decide(method: &str, path: &str, requester: Requester) -> Result<(), StatusCode> {
    if is_open(method, path) || requester == Requester::Local {
        return Ok(());
    }
    match requester {
        Requester::Stranger => Err(StatusCode::UNAUTHORIZED),
        Requester::Key { allow_tools } if acts_on_this_machine(path) && !allow_tools => Err(StatusCode::FORBIDDEN),
        Requester::Key { .. } | Requester::Local => Ok(()),
    }
}

/// The page, its files, the health check, CORS preflight and the off page's gauges: what a browser needs before it
/// can send a key, and nothing that runs a model or a tool.
fn is_open(method: &str, path: &str) -> bool {
    method == "OPTIONS"
        || matches!(
            path,
            "/" | "/index.html"
                | "/health"
                | "/v1/health"
                | "/favicon.svg"
                | "/favicon.ico"
                | "/manifest.webmanifest"
                | "/sw.js"
                | "/corral/status"
        )
        || path.starts_with("/_app/")
}

/// The router's built-in tools and its fetch proxy run on this machine, whichever device asks.
fn acts_on_this_machine(path: &str) -> bool {
    ["/tools", "/cors-proxy"].iter().any(|prefix| {
        path == *prefix || path.starts_with(&format!("{prefix}/")) || path.starts_with(&format!("{prefix}?"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STRANGER: Requester = Requester::Stranger;
    const PHONE: Requester = Requester::Key { allow_tools: false };
    const TOOLS: Requester = Requester::Key { allow_tools: true };

    #[test]
    fn the_page_is_open_to_everyone() {
        for path in [
            "/",
            "/_app/immutable/bundle.js",
            "/health",
            "/favicon.svg",
            "/corral/status",
        ] {
            assert_eq!(decide("GET", path, STRANGER), Ok(()), "{path}");
        }
        assert_eq!(decide("OPTIONS", "/v1/chat/completions", STRANGER), Ok(()));
    }

    #[test]
    fn models_need_a_key_from_other_devices() {
        for path in [
            "/v1/chat/completions",
            "/v1/models",
            "/models",
            "/props",
            "/slots",
            "/metrics",
        ] {
            assert_eq!(decide("POST", path, STRANGER), Err(StatusCode::UNAUTHORIZED), "{path}");
            assert_eq!(decide("POST", path, PHONE), Ok(()), "{path}");
            assert_eq!(decide("POST", path, Requester::Local), Ok(()), "{path}");
        }
    }

    #[test]
    fn tools_only_for_this_machine_and_keys_that_allow_them() {
        for path in ["/tools", "/cors-proxy", "/cors-proxy?url=https://example.com"] {
            assert_eq!(decide("POST", path, STRANGER), Err(StatusCode::UNAUTHORIZED), "{path}");
            assert_eq!(decide("POST", path, PHONE), Err(StatusCode::FORBIDDEN), "{path}");
            assert_eq!(decide("POST", path, TOOLS), Ok(()), "{path}");
            assert_eq!(decide("POST", path, Requester::Local), Ok(()), "{path}");
        }
        assert_eq!(decide("GET", "/toolsy", PHONE), Ok(()));
    }
}
