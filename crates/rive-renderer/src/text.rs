//! Runtime **text-run** get/set — read or write a `TextValueRun`'s string by
//! authored name on an [`Artboard`]. The run may live on the top-level artboard
//! (the common case) or in a nested artboard (addressed by a `/`-style path);
//! setting a run's text re-shapes it on the next [`advance`](crate::StateMachine::advance)
//! / draw. Introspection ([`text_run_names`](Artboard::text_run_names)) lists the
//! top-level runs so a game can discover what it can set.
//!
//! Mirrors the Rive text runtime API (https://rive.app/docs). The methods extend
//! [`Artboard`] (defined in `scene.rs`), alongside the view-model `vm_*` accessors.

use std::ffi::CString;
use std::os::raw::c_char;

use crate::{last_error, sys, Artboard, Error, Result};

/// Maps a NUL-terminating failure on a text name/path/value to [`Error::Text`].
fn text_cstring(s: &str, what: &str) -> Result<CString> {
    CString::new(s).map_err(|_| Error::Text(format!("{what} contained an interior NUL byte")))
}

/// `RIVE_OK` → `Ok(())`, otherwise the shim's last error as [`Error::Text`].
fn text_status(st: sys::RiveStatus) -> Result<()> {
    if st == sys::RIVE_OK {
        Ok(())
    } else {
        Err(Error::Text(last_error()))
    }
}

/// Runs the shim's two-call string protocol (size with a null buffer, then fill)
/// via `call`, returning the bytes as a `String`. `call(buf, cap, out_len)`.
fn read_text_via<F>(call: F) -> Result<String>
where
    F: Fn(*mut c_char, usize, *mut usize) -> sys::RiveStatus,
{
    let mut len = 0_usize;
    text_status(call(std::ptr::null_mut(), 0, &mut len))?;
    let mut buf = vec![0_u8; len];
    let mut written = 0_usize;
    text_status(call(buf.as_mut_ptr().cast::<c_char>(), buf.len(), &mut written))?;
    Ok(String::from_utf8_lossy(&buf[..written.min(buf.len())]).into_owned())
}

impl Artboard {
    /// Sets the string of the text run named `name` on the **top-level** artboard.
    /// Re-shapes the run on the next advance/draw.
    ///
    /// # Errors
    ///
    /// [`Error::Text`] if no run with that name exists, or an argument contained
    /// an interior NUL byte.
    pub fn text_set(&self, name: &str, value: &str) -> Result<()> {
        self.text_set_in(name, "", value)
    }

    /// Reads the current string of the text run named `name` on the **top-level**
    /// artboard.
    ///
    /// # Errors
    ///
    /// [`Error::Text`] if no run with that name exists, or `name` contained an
    /// interior NUL byte.
    pub fn text_get(&self, name: &str) -> Result<String> {
        self.text_get_in(name, "")
    }

    /// Sets the string of the text run named `name` inside the nested artboard at
    /// `path` (a `/`-style path; empty selects the top-level artboard).
    ///
    /// # Errors
    ///
    /// [`Error::Text`] if no such run exists, or an argument contained an interior
    /// NUL byte.
    pub fn text_set_in(&self, name: &str, path: &str, value: &str) -> Result<()> {
        let name_c = text_cstring(name, "text run name")?;
        let path_c = text_cstring(path, "text run path")?;
        let value_c = text_cstring(value, "text run value")?;
        // SAFETY: live artboard handle; all three are valid C strings.
        let st = unsafe {
            sys::rive_artboard_text_set(
                self.inner.ptr,
                name_c.as_ptr(),
                path_c.as_ptr(),
                value_c.as_ptr(),
            )
        };
        text_status(st)
    }

    /// Reads the current string of the text run named `name` inside the nested
    /// artboard at `path` (empty selects the top-level artboard).
    ///
    /// # Errors
    ///
    /// [`Error::Text`] if no such run exists, or an argument contained an interior
    /// NUL byte.
    pub fn text_get_in(&self, name: &str, path: &str) -> Result<String> {
        let name_c = text_cstring(name, "text run name")?;
        let path_c = text_cstring(path, "text run path")?;
        // SAFETY: live artboard handle; both are valid C strings; two-call protocol.
        read_text_via(|buf, cap, out_len| unsafe {
            sys::rive_artboard_text_get(
                self.inner.ptr,
                name_c.as_ptr(),
                path_c.as_ptr(),
                buf,
                cap,
                out_len,
            )
        })
    }

    /// The number of text runs on the top-level artboard.
    pub fn text_run_count(&self) -> usize {
        // SAFETY: `self.inner.ptr` is a live artboard handle.
        unsafe { sys::rive_artboard_text_run_count(self.inner.ptr) as usize }
    }

    /// The authored names of the text runs on the top-level artboard — for
    /// discovering what [`text_set`](Self::text_set) / [`text_get`](Self::text_get)
    /// can address. (Runs inside nested artboards are reached via the `_in`
    /// methods with a path and are not listed here.)
    pub fn text_run_names(&self) -> Vec<String> {
        (0..self.text_run_count())
            .map(|i| {
                // SAFETY: live handle; `i` < count; the shim's two-call protocol.
                read_text_via(|buf, cap, out_len| unsafe {
                    sys::rive_artboard_text_run_name_at(self.inner.ptr, i as u32, buf, cap, out_len)
                })
                .unwrap_or_default()
            })
            .collect()
    }
}
