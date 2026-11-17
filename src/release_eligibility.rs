//! Fail-closed device eligibility for store-specific application releases.

use serde::{Deserialize, Serialize};

/// Canonical bounded versions are compared numerically, never lexicographically.
fn version_components(version: &str) -> Option<[u32; 3]> {
    if version.len() > 20 {
        return None;
    }
    let mut parts = version.split('.');
    let mut components = [0; 3];
    for component in &mut components {
        let raw = parts.next()?;
        if raw.is_empty()
            || raw.len() > 6
            || !raw.bytes().all(|byte| byte.is_ascii_digit())
            || (raw.len() > 1 && raw.starts_with('0'))
        {
            return None;
        }
        *component = raw.parse().ok()?;
    }
    parts.next().is_none().then_some(components)
}

fn valid_platform_channel(platform: &str, channel: &str) -> bool {
    matches!(
        (platform, channel),
        ("ios", "app_store") | ("android", "google_play" | "zapstore")
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReleaseEligibility {
    platform: String,
    channel: String,
    version: String,
}

/// A validated release constraint from the campaign delivery API.
#[derive(Debug, Deserialize)]
#[serde(try_from = "RawReleaseEligibility")]
pub(crate) struct ReleaseEligibility {
    platform: String,
    channel: String,
    version: String,
}

impl TryFrom<RawReleaseEligibility> for ReleaseEligibility {
    type Error = &'static str;

    fn try_from(raw: RawReleaseEligibility) -> Result<Self, Self::Error> {
        if !valid_platform_channel(&raw.platform, &raw.channel)
            || version_components(&raw.version).is_none()
        {
            return Err("invalid release eligibility");
        }
        Ok(Self {
            platform: raw.platform,
            channel: raw.channel,
            version: raw.version,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawDeviceReleaseMetadata {
    platform: String,
    install_channel: String,
    app_version: String,
}

/// Validated metadata from an encrypted device registration, stored only here.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", try_from = "RawDeviceReleaseMetadata")]
pub struct DeviceReleaseMetadata {
    platform: String,
    install_channel: String,
    app_version: String,
}

impl TryFrom<RawDeviceReleaseMetadata> for DeviceReleaseMetadata {
    type Error = &'static str;

    fn try_from(raw: RawDeviceReleaseMetadata) -> Result<Self, Self::Error> {
        if !valid_platform_channel(&raw.platform, &raw.install_channel)
            || version_components(&raw.app_version).is_none()
        {
            return Err("invalid device release metadata");
        }
        Ok(Self {
            platform: raw.platform,
            install_channel: raw.install_channel,
            app_version: raw.app_version,
        })
    }
}

impl DeviceReleaseMetadata {
    /// Incomplete or invalid registrations clear the previous device metadata.
    pub(crate) fn from_registration(
        platform: Option<&str>,
        install_channel: Option<&str>,
        app_version: Option<&str>,
    ) -> Option<Self> {
        Self::try_from(RawDeviceReleaseMetadata {
            platform: platform?.to_string(),
            install_channel: install_channel?.to_string(),
            app_version: app_version?.to_string(),
        })
        .ok()
    }

    pub(crate) fn is_eligible_for(&self, release: &ReleaseEligibility) -> bool {
        self.platform == release.platform
            && self.install_channel == release.channel
            && version_components(&self.app_version)
                .zip(version_components(&release.version))
                .is_some_and(|(installed, available)| installed < available)
    }
}
