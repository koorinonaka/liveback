//! Asks GitHub whether a newer release exists, and hands the installer over to
//! Windows when the user says so (task t260920-2be2).
//!
//! Deliberately small. The app never replaces its own files: `installer/`'s NSIS
//! script already probes the running exe, warns, kills only on consent, polls
//! until the handle is really gone, and aborts without touching the install
//! directory if it never is (task3001). So "update" here is download to `%TEMP%`,
//! start it, and exit.
//!
//! There is no server. `https://api.github.com/repos/<owner>/<repo>/releases/latest`
//! carries `tag_name` and every asset's download URL, and 60 requests an hour
//! unauthenticated is far more than one-per-launch needs. The repository comes
//! from `CARGO_PKG_REPOSITORY`, so the URL is never written out by hand.
//!
//! Nothing is sent. These are GETs with no body, no query and no identifier --
//! which is what the README promises, so keep it that way.

use std::path::PathBuf;
use std::time::Duration;

/// Hosts a redirect may land on. Measured 2026-09-20 against a real release
/// asset: `github.com/<o>/<r>/releases/download/...` answers 302 to
/// **`release-assets.githubusercontent.com`**, not the `objects.` host the task
/// named -- a list without it refuses every download while still looking
/// correct. `objects.` stays because older assets still answer from it.
const ALLOWED_HOSTS: [&str; 4] = [
    "api.github.com",
    "github.com",
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
];

/// Enough hops for `download/` -> signed storage, with room to spare. A cap at
/// all so a redirect loop cannot hang the worker thread.
const MAX_REDIRECTS: usize = 5;

const TIMEOUT: Duration = Duration::from_secs(20);

/// What the latest release says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// `tag_name` with any leading `v` removed.
    pub version: String,
    /// The installer asset, when the release carries one.
    pub installer_url: Option<String>,
}

/// The feed this build asks. `LIVEBACK_UPDATE_FEED` overrides it so the two
/// states the UI has to show -- "newer available" and "cannot be checked" --
/// can be produced without a rebuild; nothing in the app sets it.
pub fn feed_url() -> String {
    if let Ok(url) = std::env::var("LIVEBACK_UPDATE_FEED") {
        if !url.is_empty() {
            return url;
        }
    }
    let repo = env!("CARGO_PKG_REPOSITORY");
    let slug = repo
        .trim_end_matches('/')
        .trim_start_matches("https://github.com/");
    format!("https://api.github.com/repos/{slug}/releases/latest")
}

/// Where the installer records the checkbox on its last page. `None` means the
/// key is absent -- a build run from source, or an install that predates the
/// checkbox -- and the caller then keeps the default (on).
///
/// A registry DWORD rather than a line in `settings.json`: NSIS writes registry
/// values in one instruction and cannot edit JSON without a plugin, and the
/// installer runs before the app has ever written a settings file.
pub fn installer_preference() -> Option<bool> {
    use windows::core::w;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Liveback"),
            w!("UpdateCheck"),
            RRF_RT_REG_DWORD,
            None,
            Some(std::ptr::addr_of_mut!(value).cast()),
            Some(&mut size),
        )
    };
    status.is_ok().then_some(value != 0)
}

/// The version this build is, for comparing against a release.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn host_of(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    // Strip any userinfo, which would otherwise let `evil.example/@github.com`
    // read as an allowed host.
    if host.contains('@') {
        return None;
    }
    Some(host)
}

/// Only `https`, and only the hosts above. TLS is what stops a man in the
/// middle; this stops a redirect walking the download somewhere else entirely.
/// There is no signature check and no hash: the whole distribution path is
/// trusted to GitHub (decided 2026-09-20).
pub fn host_allowed(url: &str) -> bool {
    host_of(url).is_some_and(|host| ALLOWED_HOSTS.contains(&host))
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        // Redirects are followed by hand below so every hop is checked against
        // `ALLOWED_HOSTS`, not just the one the caller passed.
        .max_redirects(0)
        .timeout_global(Some(TIMEOUT))
        .build()
        .into()
}

/// Follows redirects by hand, refusing any hop off `ALLOWED_HOSTS`.
fn get_checked(url: &str) -> Result<ureq::http::Response<ureq::Body>, String> {
    let mut next = url.to_string();
    let agent = agent();
    for _ in 0..=MAX_REDIRECTS {
        if !host_allowed(&next) {
            return Err(format!("refused a redirect to {next}"));
        }
        let response = agent
            .get(&next)
            .header("User-Agent", "liveback-update-check")
            .header("Accept", "application/vnd.github+json")
            .call()
            .map_err(|error| error.to_string())?;
        let status = response.status().as_u16();
        if !(300..400).contains(&status) {
            return Ok(response);
        }
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| format!("a {status} with no Location"))?;
        next = location.to_string();
    }
    Err("too many redirects".into())
}

/// `tag_name` and the `*-setup.exe` asset of the newest release.
pub fn fetch_latest() -> Result<Release, String> {
    let body = get_checked(&feed_url())?
        .body_mut()
        .read_to_string()
        .map_err(|error| error.to_string())?;
    parse_release(&body)
}

/// Split out so the shape of the answer can be tested without a network.
pub fn parse_release(body: &str) -> Result<Release, String> {
    let json: serde_json::Value = serde_json::from_str(body).map_err(|error| error.to_string())?;
    let tag = json
        .get("tag_name")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "the release carries no tag_name".to_string())?;
    let installer_url = json
        .get("assets")
        .and_then(|value| value.as_array())
        .and_then(|assets| {
            assets.iter().find_map(|asset| {
                let name = asset.get("name")?.as_str()?;
                // The bare `liveback.exe` is uploaded as a CI artifact, never
                // attached to a release -- but match the installer by its own
                // suffix rather than `.exe` so that stays true by construction.
                name.ends_with("-setup.exe")
                    .then(|| asset.get("browser_download_url")?.as_str())
                    .flatten()
                    .map(str::to_string)
            })
        });
    Ok(Release {
        version: tag.trim_start_matches('v').to_string(),
        installer_url,
    })
}

/// `1.2.3` -> `(1, 2, 3)`. Anything after a `-` is a pre-release marker and is
/// dropped before parsing, so `0.9.1-rc1` reads as `0.9.1`.
fn triple(version: &str) -> Option<(u32, u32, u32)> {
    let core = version.trim().trim_start_matches('v');
    let core = core.split('-').next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

/// Is `latest` newer than `current`? A version either side that will not parse
/// answers `false`: a malformed tag must not nag the user into an update that
/// may not exist.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (triple(latest), triple(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

/// Downloads the installer to `%TEMP%` and starts it. The caller exits right
/// afterwards: NSIS handles a running exe itself, but only one of the two should
/// be holding the install directory.
pub fn download_installer(url: &str) -> Result<PathBuf, String> {
    let bytes = get_checked(url)?
        .body_mut()
        .read_to_vec()
        .map_err(|error| error.to_string())?;
    let name = url
        .rsplit('/')
        .next()
        .and_then(|tail| tail.split('?').next())
        .filter(|name| name.ends_with(".exe"))
        .unwrap_or("Liveback_setup.exe");
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
    Ok(path)
}

/// Starts the downloaded installer, detached.
pub fn launch(installer: &PathBuf) -> Result<(), String> {
    std::process::Command::new(installer)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_higher_minor_is_newer_even_when_its_digits_are_fewer() {
        // The reason this is a triple compare and not a string compare.
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.9.0", "0.10.0"));
    }

    #[test]
    fn the_same_version_is_not_an_update() {
        assert!(!is_newer("0.9.0", "0.9.0"));
        assert!(!is_newer("v0.9.0", "0.9.0"));
    }

    #[test]
    fn a_tag_that_will_not_parse_never_offers_an_update() {
        assert!(!is_newer("nightly", "0.9.0"));
        assert!(!is_newer("0.9", "0.9.0.1"));
        assert!(!is_newer("", "0.9.0"));
    }

    #[test]
    fn a_pre_release_suffix_is_dropped_before_comparing() {
        assert!(is_newer("0.9.1-rc1", "0.9.0"));
        assert!(!is_newer("0.9.0-rc1", "0.9.0"));
    }

    #[test]
    fn only_github_hosts_over_https_are_allowed() {
        // The control: the hosts a real download walks through (measured
        // 2026-09-20) all pass...
        assert!(host_allowed(
            "https://api.github.com/repos/a/b/releases/latest"
        ));
        assert!(host_allowed(
            "https://github.com/a/b/releases/download/v1/x-setup.exe"
        ));
        assert!(host_allowed(
            "https://release-assets.githubusercontent.com/x?sig=y"
        ));
        // ...and everything else is refused, including the shapes that try to
        // read as an allowed host.
        assert!(!host_allowed("http://github.com/a/b"));
        assert!(!host_allowed("https://github.com.evil.example/a"));
        assert!(!host_allowed("https://evil.example/@github.com/a"));
        assert!(!host_allowed("https://evil.example/x"));
    }

    #[test]
    fn the_release_json_yields_the_tag_and_the_installer() {
        let body = r#"{
            "tag_name": "v0.9.1",
            "assets": [
                {"name": "liveback.exe", "browser_download_url": "https://github.com/a/b/releases/download/v0.9.1/liveback.exe"},
                {"name": "Liveback_0.9.1_x64-setup.exe", "browser_download_url": "https://github.com/a/b/releases/download/v0.9.1/Liveback_0.9.1_x64-setup.exe"}
            ]
        }"#;
        let release = parse_release(body).expect("a release");
        assert_eq!(release.version, "0.9.1");
        assert_eq!(
            release.installer_url.as_deref(),
            Some("https://github.com/a/b/releases/download/v0.9.1/Liveback_0.9.1_x64-setup.exe")
        );
    }

    #[test]
    fn a_release_without_an_installer_still_reports_its_version() {
        let body = r#"{"tag_name": "0.9.1", "assets": []}"#;
        let release = parse_release(body).expect("a release");
        assert_eq!(release.version, "0.9.1");
        assert_eq!(release.installer_url, None);
    }

    #[test]
    fn a_body_that_is_not_a_release_is_an_error_rather_than_a_panic() {
        assert!(parse_release("{}").is_err());
        assert!(parse_release("not json").is_err());
    }

    #[test]
    fn the_feed_url_is_built_from_the_manifest() {
        // No `LIVEBACK_UPDATE_FEED` in a test run, so this is the real one.
        let url = feed_url();
        assert!(url.starts_with("https://api.github.com/repos/"), "{url}");
        assert!(url.ends_with("/releases/latest"), "{url}");
        assert!(host_allowed(&url), "{url}");
    }
}
