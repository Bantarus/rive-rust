//! Out-of-band asset loading: supplying the images / fonts / audio a `.riv`
//! references externally (assets exported as **Referenced** rather than
//! **Embedded**).
//!
//! [`Context::load_file_with_assets`] imports a file while routing each
//! referenced asset through a host closure. The closure receives an
//! [`AssetRequest`] (name, kind, extension, …) and returns the **encoded** bytes
//! to supply — a PNG / JPEG / WEBP image, or a font / audio file, which rive
//! decodes via this context's factory — or `None` to fall back to the file's
//! in-band content (if any). The closure runs synchronously, once per asset,
//! during the load call, and is not retained afterwards.

use std::os::raw::{c_char, c_void};
use std::rc::Rc;

use crate::{last_error, sys, Context, Error, File, Result};

/// The kind of asset rive is asking the loader to resolve — mirrors rive's
/// `FileAsset` subtype.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetType {
    /// A bitmap image (`ImageAsset`) — supply PNG / JPEG / WEBP bytes.
    Image,
    /// A font (`FontAsset`) — supply a font file's bytes.
    Font,
    /// An audio clip (`AudioAsset`) — supply an audio file's bytes.
    Audio,
    /// Any other asset kind.
    Other,
}

impl AssetType {
    fn from_raw(v: u16) -> Self {
        match v {
            sys::RIVE_ASSET_IMAGE => AssetType::Image,
            sys::RIVE_ASSET_FONT => AssetType::Font,
            sys::RIVE_ASSET_AUDIO => AssetType::Audio,
            _ => AssetType::Other,
        }
    }
}

/// A borrowed description of one asset the file references, handed to the loader
/// closure. Every field is valid only for the duration of that closure call.
#[derive(Debug, Clone, Copy)]
pub struct AssetRequest<'a> {
    /// Authored asset name, e.g. `"logo.png"`.
    pub name: &'a str,
    /// Lowercase file extension without a dot, e.g. `"png"`, `"ttf"`.
    pub file_extension: &'a str,
    /// The asset's CDN UUID, or `""` if none.
    pub cdn_uuid: &'a str,
    /// The file-unique asset id.
    pub asset_id: u32,
    /// Which kind of asset this is.
    pub asset_type: AssetType,
    /// The asset's embedded (in-band) bytes if the file carries them; `None`
    /// when the asset is referenced out-of-band.
    pub in_band: Option<&'a [u8]>,
}

/// State handed to the C trampoline as the opaque `user` pointer: the host
/// closure plus a holding cell that keeps the most recently returned buffer
/// alive across the FFI boundary (the shim copies `*out_bytes` synchronously
/// right after the trampoline returns, so the `Vec` must outlive that return;
/// it is overwritten on the next call, after the shim has copied the previous).
struct LoaderCtx<F> {
    closure: F,
    held: Option<Vec<u8>>,
}

/// The C callback the shim invokes per referenced asset. Generic over the
/// closure type so it monomorphizes to a plain `extern "C" fn` (no `dyn`, no
/// caller-lifetime to name) and coerces to [`sys::RiveAssetLoadFn`].
extern "C" fn trampoline<F>(
    user: *mut c_void,
    req: *const sys::RiveAssetRequest,
    out_bytes: *mut *const u8,
    out_len: *mut usize,
) -> i32
where
    F: FnMut(AssetRequest) -> Option<Vec<u8>>,
{
    // SAFETY: `user` is the `&mut LoaderCtx<F>` passed to the load call, alive for
    // the whole synchronous import.
    let ctx = unsafe { &mut *(user as *mut LoaderCtx<F>) };
    // SAFETY: `req` is a valid request the shim built for this call.
    let req = unsafe { &*req };

    // The shim guarantees NUL-terminated, call-lifetime strings (never null for
    // name/extension/cdn_uuid), but treat null defensively as "".
    let to_str = |p: *const c_char| -> &str {
        if p.is_null() {
            ""
        } else {
            // SAFETY: shim contract — NUL-terminated, valid for this call.
            unsafe { std::ffi::CStr::from_ptr(p) }.to_str().unwrap_or("")
        }
    };

    let in_band = if req.in_band_bytes.is_null() || req.in_band_len == 0 {
        None
    } else {
        // SAFETY: shim guarantees `in_band_len` bytes at `in_band_bytes` for the call.
        Some(unsafe { std::slice::from_raw_parts(req.in_band_bytes, req.in_band_len) })
    };

    let request = AssetRequest {
        name: to_str(req.name),
        file_extension: to_str(req.file_extension),
        cdn_uuid: to_str(req.cdn_uuid),
        asset_id: req.asset_id,
        asset_type: AssetType::from_raw(req.asset_type),
        in_band,
    };

    match (ctx.closure)(request) {
        Some(bytes) if !bytes.is_empty() => {
            // Stash so the buffer outlives this return; the shim copies it next.
            let held = ctx.held.insert(bytes);
            // SAFETY: `out_bytes`/`out_len` are valid out-params from the shim.
            unsafe {
                *out_bytes = held.as_ptr();
                *out_len = held.len();
            }
            1
        }
        _ => {
            ctx.held = None;
            0
        }
    }
}

impl Context {
    /// Imports a `.riv` from memory like [`load_file`](Context::load_file), but
    /// routes each **referenced** (out-of-band) asset through `loader`.
    ///
    /// `loader` is called synchronously, once per asset, during this call. For
    /// each [`AssetRequest`] it returns the **encoded** bytes to supply (a PNG /
    /// JPEG / WEBP image, or a font / audio file — rive decodes them via this
    /// context's factory), or `None` to fall back to the file's in-band content.
    ///
    /// The bytes and `loader` are only borrowed for the duration of the call.
    ///
    /// # Errors
    ///
    /// Returns [`Error::FileLoad`] if the data is malformed or an unsupported
    /// version.
    pub fn load_file_with_assets<F>(&self, bytes: &[u8], loader: F) -> Result<File>
    where
        F: FnMut(AssetRequest) -> Option<Vec<u8>>,
    {
        let mut ctx = LoaderCtx {
            closure: loader,
            held: None,
        };
        // SAFETY: `bytes` is borrowed only for this call; `trampoline::<F>` and
        // `&mut ctx` are valid for the whole synchronous import (the loader is not
        // retained past it). The returned file keeps the context alive via its
        // `Rc` clone.
        let ptr = unsafe {
            sys::rive_file_load_with_assets(
                self.raw(),
                bytes.as_ptr(),
                bytes.len(),
                trampoline::<F>,
                (&mut ctx as *mut LoaderCtx<F>).cast::<c_void>(),
            )
        };
        if ptr.is_null() {
            return Err(Error::FileLoad(last_error()));
        }
        Ok(File {
            ptr,
            _ctx: Rc::clone(&self.inner),
        })
    }
}
