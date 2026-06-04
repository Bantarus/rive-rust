//! View-model **data binding** — read/write named properties on an artboard's
//! bound default view-model instance. This is how a host (game) drives a face's
//! data (and reads values a script/state-machine wrote). Mirrors the C++ runtime
//! contract in `docs/cpp/data-binding.mdx`.
//!
//! The native binding is set up in the shim when the artboard is created (the
//! same instance is bound to the artboard + state machine, so editor-authored
//! bindings and scripts resolve). These methods reach that instance by `path`:
//! a property name, with `/` separating nested view models (`"group/child/x"`).
//!
//! Slice 1 exposes number / bool / trigger + schema introspection; color,
//! string and enum follow (tracked in `docs/feature-support.md`).

use std::ffi::CString;
use std::marker::PhantomData;
use std::os::raw::c_char;

use crate::{last_error, sys, Artboard, Error, Result};

/// The data type of a view-model property (mirrors rive's `DataType`). Returned
/// by [`Artboard::vm_property_at`] so a caller can pick the right typed accessor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiveValueKind {
    /// 32-bit float ([`Artboard::vm_get_number`] / [`Artboard::vm_set_number`]).
    Number,
    /// Boolean ([`Artboard::vm_get_bool`] / [`Artboard::vm_set_bool`]).
    Bool,
    /// ARGB color (slice 2).
    Color,
    /// UTF-8 string (slice 2).
    String,
    /// Enumeration (slice 2).
    Enum,
    /// Trigger ([`Artboard::vm_fire_trigger`]).
    Trigger,
    /// A **list** of view-model instances. Reach its items via the handle API
    /// ([`RiveViewModelInstance::list_size`] / [`RiveViewModelInstance::list_item`]).
    List,
    /// A **nested view model**. Reach it via [`RiveViewModelInstance::view_model`]
    /// (or address its scalars directly with a `/`-separated path).
    ViewModel,
    /// An image reference (set-only; not wired yet — needs out-of-band assets).
    Image,
    /// An artboard reference (set-only; not wired yet — needs `BindableArtboard`).
    Artboard,
    /// A type not modeled yet (integer, symbol-list-index, …).
    Other,
}

impl RiveValueKind {
    /// Maps a rive `DataType` ordinal (from the shim) to a kind.
    fn from_raw(v: i32) -> Self {
        match v {
            1 => RiveValueKind::String,
            2 => RiveValueKind::Number,
            3 => RiveValueKind::Bool,
            4 => RiveValueKind::Color,
            5 => RiveValueKind::List,
            6 => RiveValueKind::Enum,
            7 => RiveValueKind::Trigger,
            8 => RiveValueKind::ViewModel,
            11 => RiveValueKind::Image,
            12 => RiveValueKind::Artboard,
            _ => RiveValueKind::Other,
        }
    }
}

/// View-model data-binding accessors. All take `&self`: the property write
/// mutates native state through the artboard handle (interior mutability), so
/// these never conflict with a `&mut StateMachine` borrow of the same instance.
impl Artboard {
    /// NUL-checked C string for a property path.
    fn vm_path(path: &str) -> Result<CString> {
        CString::new(path).map_err(|_| Error::InvalidPath)
    }

    /// Sets a view-model **number** property.
    pub fn vm_set_number(&self, path: &str, value: f32) -> Result<()> {
        let c = Self::vm_path(path)?;
        // SAFETY: `self.inner.ptr` is a live artboard handle; `c` is a valid,
        // NUL-terminated C string that outlives the call.
        let st = unsafe { sys::rive_artboard_vm_set_number(self.inner.ptr, c.as_ptr(), value) };
        vm_status(st)
    }

    /// Reads a view-model **number** property.
    pub fn vm_get_number(&self, path: &str) -> Result<f32> {
        let c = Self::vm_path(path)?;
        let mut out = 0.0_f32;
        // SAFETY: live handle + valid C string; `out` is a valid f32 slot.
        let st = unsafe { sys::rive_artboard_vm_get_number(self.inner.ptr, c.as_ptr(), &mut out) };
        vm_status(st).map(|()| out)
    }

    /// Sets a view-model **bool** property.
    pub fn vm_set_bool(&self, path: &str, value: bool) -> Result<()> {
        let c = Self::vm_path(path)?;
        // SAFETY: live handle + valid C string.
        let st = unsafe {
            sys::rive_artboard_vm_set_bool(self.inner.ptr, c.as_ptr(), u8::from(value))
        };
        vm_status(st)
    }

    /// Reads a view-model **bool** property.
    pub fn vm_get_bool(&self, path: &str) -> Result<bool> {
        let c = Self::vm_path(path)?;
        let mut out = 0_u8;
        // SAFETY: live handle + valid C string; `out` is a valid u8 slot.
        let st = unsafe { sys::rive_artboard_vm_get_bool(self.inner.ptr, c.as_ptr(), &mut out) };
        vm_status(st).map(|()| out != 0)
    }

    /// Fires a view-model **trigger** property (one-shot pulse).
    pub fn vm_fire_trigger(&self, path: &str) -> Result<()> {
        let c = Self::vm_path(path)?;
        // SAFETY: live handle + valid C string.
        let st = unsafe { sys::rive_artboard_vm_fire_trigger(self.inner.ptr, c.as_ptr()) };
        vm_status(st)
    }

    /// Number of top-level view-model properties (0 if the artboard has none).
    pub fn vm_property_count(&self) -> usize {
        // SAFETY: `self.inner.ptr` is a live artboard handle.
        unsafe { sys::rive_artboard_vm_property_count(self.inner.ptr) as usize }
    }

    /// The `(name, kind)` of the view-model property at `index`, or `None` if the
    /// index is out of range / the artboard has no view model. Uses the shim's
    /// two-call buffer protocol (size, then fill).
    pub fn vm_property_at(&self, index: usize) -> Option<(String, RiveValueKind)> {
        // SAFETY: live artboard handle; the helper runs the two-call protocol.
        read_property_at(|buf, cap, out_len, out_type| unsafe {
            sys::rive_artboard_vm_property_at(self.inner.ptr, index as u32, buf, cap, out_len, out_type)
        })
    }

    /// All top-level view-model properties as `(name, kind)`. Handy at setup to
    /// discover a face's schema (the type lets a caller pick the right accessor).
    /// For a nested view model's or list item's schema, use the handle API
    /// ([`Artboard::vm_root`] → [`RiveViewModelInstance::properties`]).
    pub fn vm_properties(&self) -> Vec<(String, RiveValueKind)> {
        (0..self.vm_property_count())
            .filter_map(|i| self.vm_property_at(i))
            .collect()
    }

    /// The artboard's bound **root view-model instance** as a borrowed handle, or
    /// `None` if the artboard has no view model. Unlike the flat `vm_*` accessors
    /// above (which address the root VM by `/`-path), the handle can introspect a
    /// nested VM's schema and index into list properties — see
    /// [`RiveViewModelInstance`].
    pub fn vm_root(&self) -> Option<RiveViewModelInstance<'_>> {
        // SAFETY: live artboard handle; the shim returns null if there is no VM.
        let p = unsafe { sys::rive_artboard_vm_root(self.inner.ptr) };
        RiveViewModelInstance::from_ptr(p)
    }

    // ---- slice 2: color / string / enum ----

    /// Sets a view-model **color** property (ARGB, e.g. `0xFF_33_AA_FF`).
    pub fn vm_set_color(&self, path: &str, argb: u32) -> Result<()> {
        let c = Self::vm_path(path)?;
        // SAFETY: live handle + valid C string.
        let st = unsafe { sys::rive_artboard_vm_set_color(self.inner.ptr, c.as_ptr(), argb) };
        vm_status(st)
    }

    /// Reads a view-model **color** property (ARGB).
    pub fn vm_get_color(&self, path: &str) -> Result<u32> {
        let c = Self::vm_path(path)?;
        let mut out = 0_u32;
        // SAFETY: live handle + valid C string; `out` is a valid u32 slot.
        let st = unsafe { sys::rive_artboard_vm_get_color(self.inner.ptr, c.as_ptr(), &mut out) };
        vm_status(st).map(|()| out)
    }

    /// Sets a view-model **string** property.
    pub fn vm_set_string(&self, path: &str, value: &str) -> Result<()> {
        let c = Self::vm_path(path)?;
        let v = CString::new(value).map_err(|_| Error::InvalidPath)?;
        // SAFETY: live handle + valid C strings.
        let st = unsafe { sys::rive_artboard_vm_set_string(self.inner.ptr, c.as_ptr(), v.as_ptr()) };
        vm_status(st)
    }

    /// Reads a view-model **string** property.
    pub fn vm_get_string(&self, path: &str) -> Result<String> {
        let c = Self::vm_path(path)?;
        // SAFETY: live handle + valid C string; the shim's two-call protocol.
        read_string_via(|buf, cap, out_len| unsafe {
            sys::rive_artboard_vm_get_string(self.inner.ptr, c.as_ptr(), buf, cap, out_len)
        })
    }

    /// Sets a view-model **enum** property by 0-based value index.
    pub fn vm_set_enum_index(&self, path: &str, index: u32) -> Result<()> {
        let c = Self::vm_path(path)?;
        // SAFETY: live handle + valid C string.
        let st = unsafe { sys::rive_artboard_vm_set_enum_index(self.inner.ptr, c.as_ptr(), index) };
        vm_status(st)
    }

    /// Reads a view-model **enum** property's current value index.
    pub fn vm_get_enum_index(&self, path: &str) -> Result<u32> {
        let c = Self::vm_path(path)?;
        let mut out = 0_u32;
        // SAFETY: live handle + valid C string; `out` is a valid u32 slot.
        let st = unsafe { sys::rive_artboard_vm_get_enum_index(self.inner.ptr, c.as_ptr(), &mut out) };
        vm_status(st).map(|()| out)
    }

    /// Sets a view-model **enum** property by value label (name).
    pub fn vm_set_enum_name(&self, path: &str, name: &str) -> Result<()> {
        let c = Self::vm_path(path)?;
        let n = CString::new(name).map_err(|_| Error::InvalidPath)?;
        // SAFETY: live handle + valid C strings.
        let st = unsafe { sys::rive_artboard_vm_set_enum_name(self.inner.ptr, c.as_ptr(), n.as_ptr()) };
        vm_status(st)
    }

    /// The ordered value labels of a view-model **enum** property (index ↔ name).
    pub fn vm_enum_values(&self, path: &str) -> Result<Vec<String>> {
        let c = Self::vm_path(path)?;
        let mut count = 0_u32;
        // SAFETY: live handle + valid C string; `count` is a valid u32 slot.
        let st =
            unsafe { sys::rive_artboard_vm_enum_value_count(self.inner.ptr, c.as_ptr(), &mut count) };
        vm_status(st)?;
        (0..count)
            .map(|i| {
                // SAFETY: live handle + valid C string; the shim's two-call protocol.
                read_string_via(|buf, cap, out_len| unsafe {
                    sys::rive_artboard_vm_enum_value_at(self.inner.ptr, c.as_ptr(), i, buf, cap, out_len)
                })
            })
            .collect()
    }
}

/// Maps a shim `RiveStatus` to `Result<()>`, attaching the shim error on failure.
fn vm_status(st: sys::RiveStatus) -> Result<()> {
    if st == sys::RIVE_OK {
        Ok(())
    } else {
        Err(Error::ViewModel(last_error()))
    }
}

/// Runs the shim's two-call string protocol (size with a null buffer, then fill)
/// via `call`, returning the bytes as a `String`. `call(buf, cap, out_len)`.
fn read_string_via<F>(call: F) -> Result<String>
where
    F: Fn(*mut c_char, usize, *mut usize) -> sys::RiveStatus,
{
    let mut len = 0_usize;
    vm_status(call(std::ptr::null_mut(), 0, &mut len))?;
    let mut buf = vec![0_u8; len];
    let mut written = 0_usize;
    vm_status(call(buf.as_mut_ptr().cast::<c_char>(), buf.len(), &mut written))?;
    Ok(String::from_utf8_lossy(&buf[..written.min(buf.len())]).into_owned())
}

/// Runs the shim's two-call property-introspection protocol (size with a null
/// buffer, then fill) via `call`, returning `(name, kind)` or `None` on error.
/// `call(name_buf, cap, out_len, out_type)`. Shared by the artboard-rooted and
/// handle introspection accessors.
fn read_property_at<F>(call: F) -> Option<(String, RiveValueKind)>
where
    F: Fn(*mut c_char, usize, *mut usize, *mut i32) -> sys::RiveStatus,
{
    let mut len = 0_usize;
    let mut ty = 0_i32;
    if call(std::ptr::null_mut(), 0, &mut len, &mut ty) != sys::RIVE_OK {
        return None;
    }
    let mut buf = vec![0_u8; len];
    let mut written = 0_usize;
    let mut ty2 = 0_i32;
    if call(buf.as_mut_ptr().cast::<c_char>(), buf.len(), &mut written, &mut ty2) != sys::RIVE_OK {
        return None;
    }
    let name = String::from_utf8_lossy(&buf[..written.min(buf.len())]).into_owned();
    Some((name, RiveValueKind::from_raw(ty2)))
}

/// A borrowed **view-model instance** handle — the artboard's root view model, a
/// nested view model, or a list item. Obtained from [`Artboard::vm_root`] and
/// navigated via [`Self::view_model`] / [`Self::list_item`].
///
/// **Borrowed:** it aliases an instance owned by rive's caches under the root view
/// model, so the borrow checker ties it to the source [`Artboard`]. This slice is
/// **read-only** — scalar reads + introspection + navigation. Writes go through
/// the artboard-rooted `vm_set_*` methods (which already accept `/`-nested paths);
/// list mutation and image/artboard refs are deferred (see `docs/feature-support.md`).
/// `!Send`/`!Sync` (rive handles are not thread-safe), as desired.
#[derive(Debug)]
pub struct RiveViewModelInstance<'a> {
    ptr: *mut sys::RiveViewModelInstance,
    _marker: PhantomData<&'a Artboard>,
}

impl<'a> RiveViewModelInstance<'a> {
    /// Wraps a shim handle, returning `None` for a null pointer (no such instance).
    fn from_ptr(ptr: *mut sys::RiveViewModelInstance) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Self { ptr, _marker: PhantomData })
        }
    }

    /// NUL-checked C string for a property path.
    fn path(p: &str) -> Result<CString> {
        CString::new(p).map_err(|_| Error::InvalidPath)
    }

    /// The nested **view model** at `path` (relative to this instance; `/`
    /// descends), or `None` if `path` is not a view-model property.
    pub fn view_model(&self, path: &str) -> Option<RiveViewModelInstance<'a>> {
        let c = Self::path(path).ok()?;
        // SAFETY: live handle (lifetime 'a) + valid C string; null = not found.
        let p = unsafe { sys::rive_vmi_property_view_model(self.ptr, c.as_ptr()) };
        RiveViewModelInstance::from_ptr(p)
    }

    /// Number of elements in the **list** property at `path`.
    pub fn list_size(&self, path: &str) -> Result<usize> {
        let c = Self::path(path)?;
        let mut out = 0_u32;
        // SAFETY: live handle + valid C string; `out` is a valid u32 slot.
        let st = unsafe { sys::rive_vmi_list_size(self.ptr, c.as_ptr(), &mut out) };
        vm_status(st).map(|()| out as usize)
    }

    /// The **list** item at `index` as a nested instance handle, or `None` if
    /// `path` is not a list, `index` is out of range, or the item is empty.
    pub fn list_item(&self, path: &str, index: usize) -> Option<RiveViewModelInstance<'a>> {
        let c = Self::path(path).ok()?;
        // SAFETY: live handle (lifetime 'a) + valid C string; null on miss/oob.
        let p = unsafe { sys::rive_vmi_list_instance_at(self.ptr, c.as_ptr(), index as u32) };
        RiveViewModelInstance::from_ptr(p)
    }

    /// Number of properties on this instance.
    pub fn property_count(&self) -> usize {
        // SAFETY: live handle.
        unsafe { sys::rive_vmi_property_count(self.ptr) as usize }
    }

    /// The `(name, kind)` of the property at `index`, or `None` if out of range.
    pub fn property_at(&self, index: usize) -> Option<(String, RiveValueKind)> {
        // SAFETY: live handle; the helper runs the two-call protocol.
        read_property_at(|buf, cap, out_len, out_type| unsafe {
            sys::rive_vmi_property_at(self.ptr, index as u32, buf, cap, out_len, out_type)
        })
    }

    /// All properties on this instance as `(name, kind)` — the schema of a nested
    /// view model or list item (the recursion the flat `vm_*` accessors can't do).
    ///
    /// PERF: O(n²) — each `property_at` makes rive rebuild the whole property vector
    /// (inherited from the pre-existing `Artboard::vm_property_at` pattern). Fine for
    /// the setup/introspection path it serves; a batched `rive_vmi_properties_*` shim
    /// call would collapse it to O(n) if it ever moves onto a hot path.
    pub fn properties(&self) -> Vec<(String, RiveValueKind)> {
        (0..self.property_count())
            .filter_map(|i| self.property_at(i))
            .collect()
    }

    /// Reads a **number** property at `path` (relative to this instance).
    pub fn get_number(&self, path: &str) -> Result<f32> {
        let c = Self::path(path)?;
        let mut out = 0.0_f32;
        // SAFETY: live handle + valid C string; `out` valid.
        let st = unsafe { sys::rive_vmi_get_number(self.ptr, c.as_ptr(), &mut out) };
        vm_status(st).map(|()| out)
    }

    /// Reads a **bool** property at `path`.
    pub fn get_bool(&self, path: &str) -> Result<bool> {
        let c = Self::path(path)?;
        let mut out = 0_u8;
        // SAFETY: live handle + valid C string; `out` valid.
        let st = unsafe { sys::rive_vmi_get_bool(self.ptr, c.as_ptr(), &mut out) };
        vm_status(st).map(|()| out != 0)
    }

    /// Reads a **color** property at `path` (ARGB).
    pub fn get_color(&self, path: &str) -> Result<u32> {
        let c = Self::path(path)?;
        let mut out = 0_u32;
        // SAFETY: live handle + valid C string; `out` valid.
        let st = unsafe { sys::rive_vmi_get_color(self.ptr, c.as_ptr(), &mut out) };
        vm_status(st).map(|()| out)
    }

    /// Reads a **string** property at `path`.
    pub fn get_string(&self, path: &str) -> Result<String> {
        let c = Self::path(path)?;
        // SAFETY: live handle + valid C string; the shim's two-call protocol.
        read_string_via(|buf, cap, out_len| unsafe {
            sys::rive_vmi_get_string(self.ptr, c.as_ptr(), buf, cap, out_len)
        })
    }

    /// Reads an **enum** property's current value index at `path`.
    pub fn get_enum_index(&self, path: &str) -> Result<u32> {
        let c = Self::path(path)?;
        let mut out = 0_u32;
        // SAFETY: live handle + valid C string; `out` valid.
        let st = unsafe { sys::rive_vmi_get_enum_index(self.ptr, c.as_ptr(), &mut out) };
        vm_status(st).map(|()| out)
    }
}
