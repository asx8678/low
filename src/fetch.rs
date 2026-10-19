//! fetch() on an open host, made by low itself rather than through DeepSeek's tools API: the body comes back as the host
//! served it, costs no paid request, and needs no search provider. What only DeepSeek reads well, an HTML page, and
//! what a retry there may get past, a rate limit or a server error, go to the tools API instead when one is set up.

use anyhow::{Result, bail};
use reqwest::{StatusCode, Url, header};
use std::time::Duration;

const USER_AGENT: &str = concat!("low/", env!("CARGO_PKG_VERSION"));
const TIMEOUT: Duration = Duration::from_secs(30);
/// Redirects followed, each only to an open host.
const HOPS: usize = 5;
/// The largest body taken: as much as the worker reads of a run's output. A cut one would not parse, so a larger one fails.
const MAX: usize = 8 << 20;
/// The bytes looked at for a NUL, as show() does to call a file binary.
const SNIFF: usize = 8192;

pub enum Fetched {
    /// The body, as served.
    Text(String),
    /// Why this one is for the tools API: an HTML page, a redirect off the open hosts, or a failure there may get past.
    Elsewhere(String),
}

/// What a response's status and content type make of it.
#[derive(Debug, PartialEq)]
enum Route {
    Read,
    Elsewhere,
    Fail,
}

fn route(status: StatusCode, kind: &str) -> Route {
    match status.as_u16() {
        200..=299 if kind.contains("html") => Route::Elsewhere,
        200..=299 => Route::Read,
        // GitHub's API answers 403 past its unauthenticated limit of 60 an hour, as others do 429.
        403 | 408 | 429 | 500..=599 => Route::Elsewhere,
        _ => Route::Fail,
    }
}

/// Gets `url`, following a redirect only to a URL `open` takes. Fails with what the model should hear: a missing page,
/// a binary file, one too large.
pub async fn direct(url: &str, open: impl Fn(&str) -> bool) -> Result<Fetched> {
    let builder = reqwest::Client::builder().user_agent(USER_AGENT).timeout(TIMEOUT);
    let http = crate::worker::proxied(builder.redirect(reqwest::redirect::Policy::none())).build()?;
    let mut at = url.to_string();
    for _ in 0..=HOPS {
        let mut response = match http.get(served_from(&at)).send().await {
            Ok(response) => response,
            Err(e) => return Ok(Fetched::Elsewhere(format!("{:#}", anyhow::Error::from(e)))),
        };
        let status = response.status();
        if status.is_redirection() {
            let location = response.headers().get(header::LOCATION).and_then(|l| l.to_str().ok());
            let Some(next) = location.and_then(|l| Url::parse(&at).ok()?.join(l).ok()) else {
                bail!("{status} with no usable Location");
            };
            if !open(next.as_str()) {
                let host = next.host_str().unwrap_or("another host");
                return Ok(Fetched::Elsewhere(format!("it redirects to {host}, which fetch() doesn't read directly")));
            }
            at = next.into();
            continue;
        }
        let kind = response.headers().get(header::CONTENT_TYPE).and_then(|k| k.to_str().ok()).unwrap_or_default();
        let kind = kind.to_lowercase();
        match route(status, &kind) {
            Route::Read => {}
            Route::Elsewhere if status.is_success() => return Ok(Fetched::Elsewhere("it is an HTML page".into())),
            Route::Elsewhere => return Ok(Fetched::Elsewhere(status.to_string())),
            Route::Fail => bail!("{status}"),
        }
        let mut body = vec![];
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) if body.len() + chunk.len() > MAX => {
                    bail!("the body is over {} MB; fetch a smaller resource, such as one version's", MAX >> 20)
                }
                Ok(Some(chunk)) => body.extend_from_slice(&chunk),
                Ok(None) => break,
                Err(e) => return Ok(Fetched::Elsewhere(format!("{:#}", anyhow::Error::from(e)))),
            }
        }
        if body[..body.len().min(SNIFF)].contains(&0) {
            bail!("a binary file ({kind}): fetch() reads text");
        }
        return match String::from_utf8(body) {
            Ok(text) => Ok(Fetched::Text(text)),
            Err(_) => bail!("not UTF-8 text ({kind}): fetch() reads text"),
        };
    }
    bail!("more than {HOPS} redirects")
}

/// Where the request goes. A debug build sends it to $LOW_OPEN_ORIGIN when that is set, as /open/<host><path>, so the
/// end-to-end tests can serve the open hosts from their mock.
fn served_from(url: &str) -> String {
    #[cfg(debug_assertions)]
    if let (Ok(origin), Ok(parsed)) = (std::env::var("LOW_OPEN_ORIGIN"), Url::parse(url)) {
        let query = parsed.query().map(|q| format!("?{q}")).unwrap_or_default();
        return format!("{origin}/open/{}{}{query}", parsed.host_str().unwrap_or_default(), parsed.path());
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes() {
        let ok = StatusCode::OK;
        assert_eq!(route(ok, "application/json; charset=utf-8"), Route::Read);
        assert_eq!(route(ok, "text/plain"), Route::Read);
        assert_eq!(route(ok, ""), Route::Read);
        assert_eq!(route(ok, "text/html; charset=utf-8"), Route::Elsewhere);
        assert_eq!(route(ok, "application/xhtml+xml"), Route::Elsewhere);
        for status in [403, 408, 429, 500, 502, 503] {
            assert_eq!(route(StatusCode::from_u16(status).unwrap(), "application/json"), Route::Elsewhere, "{status}");
        }
        for status in [400, 401, 404, 410] {
            assert_eq!(route(StatusCode::from_u16(status).unwrap(), "application/json"), Route::Fail, "{status}");
        }
    }
}
