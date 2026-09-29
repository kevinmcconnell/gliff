//! The VA display on a DRM render node, and the capability queries the
//! tiers are chosen from.

use std::ffi::{c_char, c_void, CStr};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Arc;

use crate::bindings as va;
use crate::{check, Error, Result};

/// One libva display. Not thread-safe: every codec object built on it lives
/// on the thread that created it.
pub struct Display {
    dpy: va::VADisplay,
    _node: File,
    pub vendor: String,
    pub version: (i32, i32),
}

// SAFETY: libva serialises the calls on one display behind its own lock,
// and gliff drives each display from the thread that opened it; the Arc is
// only for shared ownership by the surfaces.
unsafe impl Send for Display {}
// SAFETY: as above.
unsafe impl Sync for Display {}

impl Display {
    /// Open the VA driver for the GPU behind `render_node`.
    pub fn open(render_node: &Path) -> Result<Arc<Self>> {
        let node = File::options().read(true).write(true).open(render_node)?;
        // SAFETY: the fd stays open for the life of the display (held in
        // `_node`); the callbacks are plain functions with a null context.
        unsafe {
            let dpy = va::vaGetDisplayDRM(node.as_raw_fd());
            if dpy.is_null() {
                return Err(Error::NoDevice(format!(
                    "vaGetDisplayDRM failed on {}",
                    render_node.display()
                )));
            }
            va::vaSetErrorCallback(dpy, Some(log_error), std::ptr::null_mut());
            va::vaSetInfoCallback(dpy, Some(log_info), std::ptr::null_mut());
            let (mut major, mut minor) = (0, 0);
            if let Err(e) = check(
                va::vaInitialize(dpy, &mut major, &mut minor),
                "vaInitialize",
            ) {
                va::vaTerminate(dpy);
                return Err(Error::NoDevice(format!("{}: {e}", render_node.display())));
            }
            let vendor = CStr::from_ptr(va::vaQueryVendorString(dpy))
                .to_string_lossy()
                .into_owned();
            tracing::info!(%vendor, major, minor, node = %render_node.display(), "va-api display");
            Ok(Arc::new(Self {
                dpy,
                _node: node,
                vendor,
                version: (major, minor),
            }))
        }
    }

    pub(crate) fn raw(&self) -> va::VADisplay {
        self.dpy
    }

    pub fn profiles(&self) -> Result<Vec<va::VAProfile>> {
        // SAFETY: the list is sized by vaMaxNumProfiles as the API requires.
        unsafe {
            let mut list = vec![0; va::vaMaxNumProfiles(self.dpy).max(0) as usize];
            let mut n = 0;
            check(
                va::vaQueryConfigProfiles(self.dpy, list.as_mut_ptr(), &mut n),
                "vaQueryConfigProfiles",
            )?;
            list.truncate(n.max(0) as usize);
            Ok(list)
        }
    }

    pub fn entrypoints(&self, profile: va::VAProfile) -> Result<Vec<va::VAEntrypoint>> {
        // SAFETY: the list is sized by vaMaxNumEntrypoints as the API requires.
        unsafe {
            let mut list = vec![0; va::vaMaxNumEntrypoints(self.dpy).max(0) as usize];
            let mut n = 0;
            check(
                va::vaQueryConfigEntrypoints(self.dpy, profile, list.as_mut_ptr(), &mut n),
                "vaQueryConfigEntrypoints",
            )?;
            list.truncate(n.max(0) as usize);
            Ok(list)
        }
    }

    /// The value of each attribute for the profile and entrypoint, `None`
    /// where the driver reports it as not supported.
    pub fn config_attribs(
        &self,
        profile: va::VAProfile,
        entrypoint: va::VAEntrypoint,
        types: &[va::VAConfigAttribType],
    ) -> Result<Vec<Option<u32>>> {
        let mut attribs: Vec<va::VAConfigAttrib> = types
            .iter()
            .map(|&type_| va::VAConfigAttrib { type_, value: 0 })
            .collect();
        // SAFETY: the array is fully initialised and sized by its length.
        unsafe {
            check(
                va::vaGetConfigAttributes(
                    self.dpy,
                    profile,
                    entrypoint,
                    attribs.as_mut_ptr(),
                    attribs.len() as i32,
                ),
                "vaGetConfigAttributes",
            )?;
        }
        Ok(attribs
            .iter()
            .map(|a| (a.value != va::VA_ATTRIB_NOT_SUPPORTED).then_some(a.value))
            .collect())
    }

    /// What the driver offers for H.264 High profile.
    pub fn caps(&self) -> Result<Caps> {
        Caps::query(self)
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: the display was initialised in `open` and nothing built on
        // it outlives the Arc.
        unsafe {
            va::vaTerminate(self.dpy);
        }
    }
}

unsafe extern "C" fn log_error(_ctx: *mut c_void, message: *const c_char) {
    // SAFETY: libva passes a NUL-terminated string.
    let msg = unsafe { CStr::from_ptr(message) }.to_string_lossy();
    tracing::warn!(target: "libva", "{}", msg.trim_end());
}

unsafe extern "C" fn log_info(_ctx: *mut c_void, message: *const c_char) {
    // SAFETY: libva passes a NUL-terminated string.
    let msg = unsafe { CStr::from_ptr(message) }.to_string_lossy();
    tracing::debug!(target: "libva", "{}", msg.trim_end());
}

/// The H.264 High profile capabilities gliff chooses its tiers from.
#[derive(Debug, Clone)]
pub struct Caps {
    /// The encode entrypoint to use, when encode is possible at all:
    /// `VAEntrypointEncSlice` when offered, else the low-power one.
    pub encode_entrypoint: Option<va::VAEntrypoint>,
    pub decode: bool,
    /// `VA_RC_*` bits the encode entrypoint supports.
    pub rate_control: u32,
    /// `VA_ENC_PACKED_HEADER_*` bits the encode entrypoint accepts.
    pub packed_headers: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub decode_max_width: u32,
    pub decode_max_height: u32,
}

impl Caps {
    pub const PROFILE: va::VAProfile = va::VAProfileH264High;

    pub fn query(display: &Display) -> Result<Self> {
        let profiles = display.profiles()?;
        let mut caps = Self {
            encode_entrypoint: None,
            decode: false,
            rate_control: 0,
            packed_headers: 0,
            max_width: 0,
            max_height: 0,
            decode_max_width: 0,
            decode_max_height: 0,
        };
        if !profiles.contains(&Self::PROFILE) {
            return Ok(caps);
        }
        let entrypoints = display.entrypoints(Self::PROFILE)?;
        caps.decode = entrypoints.contains(&va::VAEntrypointVLD);
        if caps.decode {
            let values = display.config_attribs(
                Self::PROFILE,
                va::VAEntrypointVLD,
                &[
                    va::VAConfigAttribMaxPictureWidth,
                    va::VAConfigAttribMaxPictureHeight,
                ],
            )?;
            caps.decode_max_width = values[0].unwrap_or(0);
            caps.decode_max_height = values[1].unwrap_or(0);
        }
        // The first entrypoint that can run the way gliff drives it, else
        // the first one offered so the failure names its shortcoming.
        let mut first = None;
        for entrypoint in [va::VAEntrypointEncSlice, va::VAEntrypointEncSliceLP] {
            if !entrypoints.contains(&entrypoint) {
                continue;
            }
            let values = display.config_attribs(
                Self::PROFILE,
                entrypoint,
                &[
                    va::VAConfigAttribRateControl,
                    va::VAConfigAttribEncPackedHeaders,
                    va::VAConfigAttribMaxPictureWidth,
                    va::VAConfigAttribMaxPictureHeight,
                ],
            )?;
            let candidate = Self {
                encode_entrypoint: Some(entrypoint),
                rate_control: values[0].unwrap_or(0),
                packed_headers: values[1].unwrap_or(0),
                max_width: values[2].unwrap_or(0),
                max_height: values[3].unwrap_or(0),
                ..caps.clone()
            };
            if candidate.can_encode().is_ok() {
                return Ok(candidate);
            }
            first.get_or_insert(candidate);
        }
        Ok(first.unwrap_or(caps))
    }

    /// The packed headers gliff writes itself.
    pub const PACKED_HEADERS: u32 = va::VA_ENC_PACKED_HEADER_SEQUENCE
        | va::VA_ENC_PACKED_HEADER_PICTURE
        | va::VA_ENC_PACKED_HEADER_SLICE;

    /// Whether the encoder can run the way gliff drives it.
    pub fn can_encode(&self) -> Result<()> {
        let Some(entrypoint) = self.encode_entrypoint else {
            return Err(Error::Unsupported("no H.264 High encode entrypoint".into()));
        };
        if self.rate_control & va::VA_RC_CBR == 0 {
            return Err(Error::Unsupported(format!(
                "H.264 encode entrypoint {} has no CBR (rate control modes {:#x})",
                entrypoint_name(entrypoint),
                self.rate_control
            )));
        }
        if self.packed_headers & Self::PACKED_HEADERS != Self::PACKED_HEADERS {
            return Err(Error::Unsupported(format!(
                "H.264 encode entrypoint {} does not take packed SPS/PPS/slice headers ({:#x})",
                entrypoint_name(entrypoint),
                self.packed_headers
            )));
        }
        Ok(())
    }

    pub fn can_decode(&self) -> Result<()> {
        if self.decode {
            Ok(())
        } else {
            Err(Error::Unsupported("no H.264 High decode entrypoint".into()))
        }
    }
}

pub fn entrypoint_name(e: va::VAEntrypoint) -> &'static str {
    match e {
        va::VAEntrypointVLD => "VLD",
        va::VAEntrypointEncSlice => "EncSlice",
        va::VAEntrypointEncSliceLP => "EncSliceLP",
        _ => "other",
    }
}

/// `VA_RC_*` bits as words, for the probe.
pub fn rate_control_names(bits: u32) -> Vec<&'static str> {
    let mut v = Vec::new();
    for (bit, name) in [
        (va::VA_RC_CQP, "CQP"),
        (va::VA_RC_CBR, "CBR"),
        (va::VA_RC_VBR, "VBR"),
        (va::VA_RC_VCM, "VCM"),
        (va::VA_RC_ICQ, "ICQ"),
        (va::VA_RC_QVBR, "QVBR"),
        (va::VA_RC_AVBR, "AVBR"),
    ] {
        if bits & bit != 0 {
            v.push(name);
        }
    }
    v
}
