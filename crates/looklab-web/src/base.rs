// SPDX-License-Identifier: AGPL-3.0-or-later
//! Deployment paths: the public URL prefix and the API origin ([docs/01](../../../docs/01-architecture.md)).
//! The site template writes the deployment prefix into its first stylesheet link, and the page derives every asset URL from it.

use std::sync::LazyLock;

/// Where this build expects the API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Server {
    /// No server: actions that need the API are disabled.
    Preview,
    SameOrigin,
    Remote(&'static str),
}

/// `LOOKLAB_PREVIEW=1` marks the serverless preview; otherwise `LOOKLAB_API_BASE` names the API origin.
pub fn server() -> Server {
    if matches!(option_env!("LOOKLAB_PREVIEW"), Some(value) if !value.trim().is_empty()) {
        return Server::Preview;
    }
    match option_env!("LOOKLAB_API_BASE") {
        Some(base) => match base.trim().trim_end_matches('/') {
            "" => Server::SameOrigin,
            origin => Server::Remote(origin),
        },
        None => Server::SameOrigin,
    }
}

pub fn server_available() -> bool {
    !matches!(server(), Server::Preview)
}

static PUBLIC_PREFIX: LazyLock<String> = LazyLock::new(detect_prefix);

/// The path prefix the site is served under, always with a trailing slash.
pub fn public_prefix() -> &'static str {
    &PUBLIC_PREFIX
}

/// The same prefix without its trailing slash, for the router's `base`.
pub fn router_base() -> &'static str {
    public_prefix().trim_end_matches('/')
}

/// An absolute URL for a static-site path.
pub fn site_url(path: &str) -> String {
    join(public_prefix(), path)
}

pub(crate) fn query_component(value: &str) -> String {
    js_sys::encode_uri_component(value).into()
}

/// An absolute URL for an API path.
pub fn api_url(path: &str) -> String {
    match server() {
        Server::Remote(origin) => join(&format!("{origin}/"), path),
        Server::Preview | Server::SameOrigin => join("/", path),
    }
}

/// Where the running deployment advertises its current publication root.
pub fn site_root_url() -> String {
    match server() {
        Server::Remote(origin) => format!("{origin}/data/site-root.json"),
        Server::Preview | Server::SameOrigin => "/data/site-root.json".to_owned(),
    }
}

/// An absolute URL for published catalog data, from the API when one is configured.
pub fn publication_url(path: &str) -> String {
    match server() {
        Server::Preview => site_url(path),
        Server::SameOrigin | Server::Remote(_) => api_url(path),
    }
}

/// The router path for a full `window.location.pathname`.
pub fn page_path(pathname: &str) -> String {
    path_after_prefix(pathname, public_prefix()).into_owned()
}

/// The router target for a full URL: the page path plus query and fragment. `navigate` adds the prefix itself.
pub fn route_of(url: &web_sys::Url) -> String {
    route(public_prefix(), &url.pathname(), &url.search(), &url.hash())
}

fn route(prefix: &str, pathname: &str, search: &str, hash: &str) -> String {
    format!("{}{search}{hash}", path_after_prefix(pathname, prefix))
}

fn join(prefix: &str, path: &str) -> String {
    format!("{prefix}{}", path.trim_start_matches('/'))
}

fn path_after_prefix<'a>(pathname: &'a str, prefix: &'a str) -> std::borrow::Cow<'a, str> {
    let rest = pathname.strip_prefix(prefix).unwrap_or(pathname);
    if rest.starts_with('/') {
        rest.into()
    } else {
        format!("/{rest}").into()
    }
}

fn detect_prefix() -> String {
    let Some(window) = web_sys::window() else {
        return "/".to_owned();
    };
    let Some(document) = window.document() else {
        return "/".to_owned();
    };
    let origin = window.location().origin().unwrap_or_default();
    let href = window.location().href().unwrap_or_default();
    for selector in [
        "link[rel=\"stylesheet\"][href]",
        "link[rel=\"modulepreload\"][href]",
        "script[src]",
    ] {
        let Ok(Some(element)) = document.query_selector(selector) else {
            continue;
        };
        let Some(attribute) = &element
            .get_attribute("href")
            .or_else(|| element.get_attribute("src"))
        else {
            continue;
        };
        let Ok(url) = web_sys::Url::new_with_base(attribute, &href) else {
            continue;
        };
        if url.origin() != origin {
            continue;
        }
        if let Some(prefix) = prefix_of_pathname(&url.pathname()) {
            return prefix;
        }
    }
    "/".to_owned()
}

fn prefix_of_pathname(pathname: &str) -> Option<String> {
    if !pathname.starts_with('/') {
        return None;
    }
    let (directory, _file) = pathname.rsplit_once('/')?;
    Some(format!("{directory}/"))
}

#[cfg(test)]
mod tests {
    use super::{join, path_after_prefix, prefix_of_pathname, route};

    #[test]
    fn prefix_is_the_asset_directory() {
        assert_eq!(
            prefix_of_pathname("/looklab/styles.css").as_deref(),
            Some("/looklab/")
        );
        assert_eq!(prefix_of_pathname("/styles.css").as_deref(), Some("/"));
        assert_eq!(prefix_of_pathname("/a/b/c.js").as_deref(), Some("/a/b/"));
        assert_eq!(prefix_of_pathname("relative.css"), None);
    }

    #[test]
    fn site_urls_carry_the_prefix_once() {
        assert_eq!(join("/", "/data/index.json"), "/data/index.json");
        assert_eq!(
            join("/looklab/", "/data/index.json"),
            "/looklab/data/index.json"
        );
        assert_eq!(
            join("/looklab/", "data/index.json"),
            "/looklab/data/index.json"
        );
    }

    #[test]
    fn page_paths_drop_the_prefix() {
        assert_eq!(
            path_after_prefix("/looklab/pattern/ab", "/looklab/"),
            "/pattern/ab"
        );
        assert_eq!(path_after_prefix("/looklab/", "/looklab/"), "/");
        assert_eq!(path_after_prefix("/pattern/ab", "/"), "/pattern/ab");
    }

    #[test]
    fn routes_drop_the_prefix_and_keep_query_and_fragment() {
        assert_eq!(
            route("/looklab/", "/looklab/compare", "?a=x&b=y", ""),
            "/compare?a=x&b=y"
        );
        assert_eq!(
            route("/looklab/", "/looklab/log", "?show=moderation", "#top"),
            "/log?show=moderation#top"
        );
        assert_eq!(route("/", "/compare", "?a=x", ""), "/compare?a=x");
    }
}
