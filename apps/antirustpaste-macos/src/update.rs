//! Release checking and staged-update helpers.
//!
//! Everything here is platform-free: version parsing, reading the GitHub
//! release JSON, selecting the zip and checksum assets, locating the `.app`
//! bundle and composing the detached install script. `app.rs` owns the
//! `NSURLSession` requests and the AppKit prompts.

use std::cmp::Ordering;
use std::fmt;
use std::path::{Path, PathBuf};

/// `releases/latest` is the only endpoint the app talks to. Drafts and
/// pre-releases are excluded by GitHub for this endpoint.
pub(crate) const RELEASES_API: &str =
    "https://api.github.com/repos/d-rouge/AntiRustPaste/releases/latest";
pub(crate) const RELEASES_PAGE: &str = "https://github.com/d-rouge/AntiRustPaste/releases/latest";

/// Upper bound for the release metadata response.
pub(crate) const RELEASE_JSON_LIMIT: usize = 512 * 1024;
/// Upper bound for `SHA256SUMS.txt`.
pub(crate) const CHECKSUMS_LIMIT: usize = 64 * 1024;
/// Upper bound for the universal app zip.
pub(crate) const ZIP_LIMIT: usize = 64 * 1024 * 1024;

const BUNDLE_IDENTIFIER: &str = "com.rustclip.app";

/// A `major.minor.patch` version with an optional pre-release suffix.
///
/// A pre-release (`0.3.0-beta`) sorts before the matching release (`0.3.0`).
/// Build metadata after `+` is ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: bool,
}

impl Version {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text
            .strip_prefix('v')
            .or_else(|| text.strip_prefix('V'))
            .unwrap_or(text);
        if text.is_empty() {
            return None;
        }

        let (core, suffix) = match text.find(['-', '+']) {
            Some(index) => (&text[..index], &text[index..]),
            None => (text, ""),
        };
        let prerelease = suffix.starts_with('-');

        let mut parts = core.split('.');
        let major = parse_number(parts.next()?)?;
        let minor = parse_number(parts.next().unwrap_or("0"))?;
        let patch = parse_number(parts.next().unwrap_or("0"))?;
        if parts.next().is_some() {
            return None;
        }

        Some(Self {
            major,
            minor,
            patch,
            prerelease,
        })
    }
}

fn parse_number(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse::<u64>().ok()
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| other.prerelease.cmp(&self.prerelease))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A downloadable asset from the release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Asset {
    pub(crate) name: String,
    pub(crate) url: String,
}

/// A newer release that can be offered to the operator.
#[derive(Clone, Debug)]
pub(crate) struct AvailableUpdate {
    pub(crate) version: Version,
    pub(crate) zip: Asset,
    pub(crate) sums: Option<Asset>,
    pub(crate) page_url: String,
}

/// Read GitHub's `releases/latest` payload and return the update when its tag
/// is newer than `current`.
pub(crate) fn latest_release(
    payload: &[u8],
    current: &str,
) -> Result<Option<AvailableUpdate>, String> {
    let current =
        Version::parse(current).ok_or_else(|| "the running version is not valid".to_string())?;

    let value: serde_json::Value = serde_json::from_slice(payload)
        .map_err(|_| "the release information could not be read".to_string())?;
    let object = value
        .as_object()
        .ok_or_else(|| "the release information is not an object".to_string())?;

    if object.get("draft").and_then(|v| v.as_bool()) == Some(true)
        || object.get("prerelease").and_then(|v| v.as_bool()) == Some(true)
    {
        return Ok(None);
    }

    let tag = object
        .get("tag_name")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "the release has no tag".to_string())?;
    let version = Version::parse(tag)
        .ok_or_else(|| format!("the release tag \"{tag}\" is not a version"))?;
    if version <= current {
        return Ok(None);
    }

    let assets = object
        .get("assets")
        .and_then(|value| value.as_array())
        .ok_or_else(|| "the release has no assets".to_string())?;
    let mut zips = Vec::new();
    let mut sums = None;
    for asset in assets {
        let Some(name) = asset.get("name").and_then(|value| value.as_str()) else {
            continue;
        };
        let Some(url) = asset.get("browser_download_url").and_then(|value| value.as_str()) else {
            continue;
        };
        if !url.starts_with("https://") {
            continue;
        }
        if name == "SHA256SUMS.txt" {
            sums = Some(Asset {
                name: name.to_string(),
                url: url.to_string(),
            });
        } else if name.starts_with("AntiRustPaste-") && name.ends_with(".zip") {
            zips.push(Asset {
                name: name.to_string(),
                url: url.to_string(),
            });
        }
    }

    let zip = zips
        .iter()
        .find(|asset| asset.name.contains("-universal"))
        .or_else(|| zips.first())
        .cloned()
        .ok_or_else(|| "the release has no application zip".to_string())?;

    let page_url = object
        .get("html_url")
        .and_then(|value| value.as_str())
        .filter(|url| url.starts_with("https://github.com/"))
        .unwrap_or(RELEASES_PAGE)
        .to_string();

    Ok(Some(AvailableUpdate {
        version,
        zip,
        sums,
        page_url,
    }))
}

/// Find the SHA-256 for `file` in a `SHA256SUMS.txt` payload.
pub(crate) fn checksum_for(sums: &str, file: &str) -> Option<String> {
    for line in sums.lines() {
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else {
            continue;
        };
        let Some(name) = parts.next() else {
            continue;
        };
        let name = name.trim_start_matches('*');
        if name == file && is_sha256(hash) {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Locate `Foo.app` from a running `Foo.app/Contents/MacOS/foo` executable.
pub(crate) fn bundle_root(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    if macos.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    let bundle = contents.parent()?;
    if bundle.extension()? != "app" {
        return None;
    }
    Some(bundle.to_path_buf())
}

/// Gatekeeper's app translocation runs a downloaded bundle from a read-only
/// random path; it cannot update itself in place.
pub(crate) fn is_translocated(bundle: &Path) -> bool {
    bundle
        .to_string_lossy()
        .contains("/AppTranslocation/")
}

/// `PID`, target bundle, staged bundle and work directory are passed as `$1`
/// to `$4`. The script waits for the app to exit, swaps the bundles with
/// `ditto` and reopens the app. A failed copy restores the old bundle.
pub(crate) const INSTALL_SCRIPT: &str = r#"#!/bin/sh
# Installed by AntiRustPaste to finish a self-update after it quits.
set -eu
pid=$1
target=$2
staged=$3
work=$4
backup="${target}.antirustpaste-old"

while kill -0 "$pid" 2>/dev/null; do
  sleep 0.2
done

rm -rf "$backup"
if [ -e "$target" ]; then
  mv "$target" "$backup"
fi

if /usr/bin/ditto "$staged" "$target"; then
  /usr/bin/xattr -dr com.apple.quarantine "$target" 2>/dev/null || true
  rm -rf "$backup"
  rm -rf "$work"
  /usr/bin/open "$target"
else
  rm -rf "$target"
  if [ -e "$backup" ]; then
    mv "$backup" "$target"
    /usr/bin/open "$target"
  fi
  rm -rf "$work"
  exit 1
fi
"#;

/// The bundle identifier the staged app must declare before it is installed.
pub(crate) const EXPECTED_BUNDLE_IDENTIFIER: &str = BUNDLE_IDENTIFIER;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_prefixed_versions() {
        let version = Version::parse("v1.2.3").expect("version");
        assert_eq!(version.to_string(), "1.2.3");
        assert_eq!(Version::parse("1.2"), Some(Version {
            major: 1,
            minor: 2,
            patch: 0,
            prerelease: false,
        }));
        assert_eq!(Version::parse("2"), Some(Version {
            major: 2,
            minor: 0,
            patch: 0,
            prerelease: false,
        }));
    }

    #[test]
    fn rejects_malformed_versions() {
        assert_eq!(Version::parse(""), None);
        assert_eq!(Version::parse("v"), None);
        assert_eq!(Version::parse("1.2.3.4"), None);
        assert_eq!(Version::parse("1.x.3"), None);
        assert_eq!(Version::parse("-1.0.0"), None);
        assert_eq!(Version::parse("99999999999999999999.0.0"), None);
    }

    #[test]
    fn compares_versions_and_prereleases() {
        let old = Version::parse("0.2.1").expect("old");
        let new = Version::parse("0.3.0").expect("new");
        assert!(new > old);
        assert!(Version::parse("0.2.2") > old);
        assert!(old > Version::parse("0.2.0").expect("older"));
        assert!(Version::parse("0.3.0-beta") < new);
        assert_eq!(Version::parse("0.2.1"), Some(old));
    }

    fn release_json(tag: &str) -> Vec<u8> {
        format!(
            r#"{{
                "tag_name": "{tag}",
                "html_url": "https://github.com/d-rouge/AntiRustPaste/releases/tag/{tag}",
                "draft": false,
                "prerelease": false,
                "assets": [
                    {{"name": "AntiRustPaste-0.3.0-universal.zip",
                      "browser_download_url": "https://github.com/d-rouge/AntiRustPaste/releases/download/{tag}/AntiRustPaste-0.3.0-universal.zip"}},
                    {{"name": "AntiRustPaste-0.3.0-universal.dmg",
                      "browser_download_url": "https://github.com/d-rouge/AntiRustPaste/releases/download/{tag}/AntiRustPaste-0.3.0-universal.dmg"}},
                    {{"name": "SHA256SUMS.txt",
                      "browser_download_url": "https://github.com/d-rouge/AntiRustPaste/releases/download/{tag}/SHA256SUMS.txt"}}
                ]
            }}"#
        )
        .into_bytes()
    }

    #[test]
    fn finds_a_newer_release_and_its_assets() {
        let update = latest_release(&release_json("v0.3.0"), "0.2.1")
            .expect("release")
            .expect("update");
        assert_eq!(update.version.to_string(), "0.3.0");
        assert_eq!(update.zip.name, "AntiRustPaste-0.3.0-universal.zip");
        assert!(update.zip.url.starts_with("https://github.com/"));
        assert_eq!(
            update.sums.as_ref().map(|asset| asset.name.as_str()),
            Some("SHA256SUMS.txt")
        );
    }

    #[test]
    fn ignores_the_running_or_an_older_release() {
        assert!(latest_release(&release_json("v0.2.1"), "0.2.1")
            .expect("release")
            .is_none());
        assert!(latest_release(&release_json("v0.2.0"), "0.2.1")
            .expect("release")
            .is_none());
    }

    #[test]
    fn rejects_a_release_without_a_zip() {
        let payload = br#"{
            "tag_name": "v9.0.0",
            "assets": [{"name": "SHA256SUMS.txt",
                        "browser_download_url": "https://example.invalid/SHA256SUMS.txt"}]
        }"#;
        assert!(latest_release(payload, "0.2.1").is_err());
    }

    #[test]
    fn rejects_broken_release_json() {
        assert!(latest_release(b"{", "0.2.1").is_err());
        assert!(latest_release(b"[]", "0.2.1").is_err());
        assert!(latest_release(br#"{"assets": []}"#, "0.2.1").is_err());
        assert!(latest_release(br#"{"tag_name": "nope", "assets": []}"#, "0.2.1").is_err());
    }

    #[test]
    fn reads_checksums_in_both_formats() {
        let sums = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  AntiRustPaste-0.3.0-universal.zip\r\n\
                    bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb *other.zip\n";
        assert_eq!(
            checksum_for(sums, "AntiRustPaste-0.3.0-universal.zip").as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            checksum_for(sums, "other.zip").as_deref(),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
        assert_eq!(checksum_for(sums, "missing.zip"), None);
        assert_eq!(checksum_for("not-a-hash  AntiRustPaste.zip", "AntiRustPaste.zip"), None);
    }

    #[test]
    fn derives_the_bundle_root_from_the_executable() {
        assert_eq!(
            bundle_root(Path::new("/Applications/AntiRustPaste.app/Contents/MacOS/AntiRustPaste")),
            Some(PathBuf::from("/Applications/AntiRustPaste.app"))
        );
        assert_eq!(
            bundle_root(Path::new("/Users/me/target/release/antirustpaste")),
            None
        );
        assert_eq!(
            bundle_root(Path::new("/Applications/AntiRustPaste.app/Contents/AntiRustPaste")),
            None
        );
        assert!(!is_translocated(Path::new(
            "/Applications/AntiRustPaste.app"
        )));
        assert!(is_translocated(Path::new(
            "/private/var/folders/x/AppTranslocation/ABC/d/AntiRustPaste.app"
        )));
    }
}
