//! VA-API display access and capability probing.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use cros_codecs::libva::{Display, VAEntrypoint, VAProfile};

use crate::{Error, Result};

pub const DEFAULT_RENDER_NODE: &str = "/dev/dri/renderD128";

pub fn render_node(explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HAVER_RENDER_NODE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_RENDER_NODE))
}

pub fn open_display(node: &Path) -> Result<Rc<Display>> {
    Display::open_drm_display(node).map_err(|e| Error::Va(format!("open {}: {e}", node.display())))
}

#[derive(Debug, Clone)]
pub struct ProfileInfo {
    pub id: i32,
    pub name: &'static str,
    pub entrypoints: Vec<(u32, &'static str)>,
}

impl ProfileInfo {
    pub fn has(&self, entrypoint: u32) -> bool {
        self.entrypoints.iter().any(|(e, _)| *e == entrypoint)
    }
}

#[derive(Debug, Clone)]
pub struct VaInfo {
    pub node: PathBuf,
    pub vendor: String,
    pub profiles: Vec<ProfileInfo>,
}

impl VaInfo {
    pub fn profile(&self, id: i32) -> Option<&ProfileInfo> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn can_decode(&self, profile: i32) -> bool {
        self.profile(profile)
            .is_some_and(|p| p.has(VAEntrypoint::VAEntrypointVLD))
    }

    pub fn can_encode(&self, profile: i32) -> bool {
        self.profile(profile).is_some_and(|p| {
            p.has(VAEntrypoint::VAEntrypointEncSlice) || p.has(VAEntrypoint::VAEntrypointEncSliceLP)
        })
    }

    pub fn low_power_encode(&self, profile: i32) -> bool {
        self.profile(profile)
            .is_some_and(|p| p.has(VAEntrypoint::VAEntrypointEncSliceLP))
    }

    pub fn h264_encode(&self) -> bool {
        self.can_encode(VAProfile::VAProfileH264High)
            || self.can_encode(VAProfile::VAProfileH264Main)
    }

    pub fn h264_decode(&self) -> bool {
        self.can_decode(VAProfile::VAProfileH264High)
    }

    /// Any profile that encodes 4:4:4 natively.
    pub fn native_444_encode(&self) -> Vec<&'static str> {
        [
            VAProfile::VAProfileHEVCMain444,
            VAProfile::VAProfileHEVCSccMain444,
            VAProfile::VAProfileAV1Profile1,
        ]
        .into_iter()
        .filter(|p| self.can_encode(*p))
        .filter_map(|p| self.profile(p).map(|p| p.name))
        .collect()
    }
}

pub fn probe(node: &Path) -> Result<VaInfo> {
    let display = open_display(node)?;
    let vendor = display
        .query_vendor_string()
        .unwrap_or_else(|_| "unknown".to_owned());
    let mut profiles = Vec::new();
    for id in display
        .query_config_profiles()
        .map_err(|e| Error::Va(e.to_string()))?
    {
        let entrypoints = display
            .query_config_entrypoints(id)
            .map_err(|e| Error::Va(e.to_string()))?
            .into_iter()
            .map(|e| (e, entrypoint_name(e)))
            .collect();
        profiles.push(ProfileInfo {
            id,
            name: profile_name(id),
            entrypoints,
        });
    }
    Ok(VaInfo {
        node: node.to_path_buf(),
        vendor,
        profiles,
    })
}

pub fn profile_name(id: i32) -> &'static str {
    use VAProfile::*;
    match id {
        x if x == VAProfileMPEG2Simple => "MPEG2Simple",
        x if x == VAProfileMPEG2Main => "MPEG2Main",
        x if x == VAProfileH264Baseline => "H264Baseline",
        x if x == VAProfileH264Main => "H264Main",
        x if x == VAProfileH264High => "H264High",
        x if x == VAProfileH264High10 => "H264High10",
        x if x == VAProfileH264High422 => "H264High422",
        x if x == VAProfileH264ConstrainedBaseline => "H264ConstrainedBaseline",
        x if x == VAProfileJPEGBaseline => "JPEGBaseline",
        x if x == VAProfileVP8Version0_3 => "VP8",
        x if x == VAProfileHEVCMain => "HEVCMain",
        x if x == VAProfileHEVCMain10 => "HEVCMain10",
        x if x == VAProfileHEVCMain12 => "HEVCMain12",
        x if x == VAProfileHEVCMain422_10 => "HEVCMain422_10",
        x if x == VAProfileHEVCMain422_12 => "HEVCMain422_12",
        x if x == VAProfileHEVCMain444 => "HEVCMain444",
        x if x == VAProfileHEVCMain444_10 => "HEVCMain444_10",
        x if x == VAProfileHEVCMain444_12 => "HEVCMain444_12",
        x if x == VAProfileHEVCSccMain => "HEVCSccMain",
        x if x == VAProfileHEVCSccMain10 => "HEVCSccMain10",
        x if x == VAProfileHEVCSccMain444 => "HEVCSccMain444",
        x if x == VAProfileHEVCSccMain444_10 => "HEVCSccMain444_10",
        x if x == VAProfileVP9Profile0 => "VP9Profile0",
        x if x == VAProfileVP9Profile1 => "VP9Profile1",
        x if x == VAProfileVP9Profile2 => "VP9Profile2",
        x if x == VAProfileVP9Profile3 => "VP9Profile3",
        x if x == VAProfileAV1Profile0 => "AV1Profile0",
        x if x == VAProfileAV1Profile1 => "AV1Profile1",
        x if x == VAProfileAV1Profile2 => "AV1Profile2",
        x if x == VAProfileVVCMain10 => "VVCMain10",
        x if x == VAProfileProtected => "Protected",
        _ => "other",
    }
}

pub fn entrypoint_name(id: u32) -> &'static str {
    use VAEntrypoint::*;
    match id {
        x if x == VAEntrypointVLD => "decode",
        x if x == VAEntrypointEncSlice => "encode",
        x if x == VAEntrypointEncSliceLP => "encode-lp",
        x if x == VAEntrypointEncPicture => "encode-picture",
        x if x == VAEntrypointVideoProc => "vpp",
        x if x == VAEntrypointFEI => "fei",
        x if x == VAEntrypointStats => "stats",
        x if x == VAEntrypointProtectedContent => "protected",
        _ => "other",
    }
}
