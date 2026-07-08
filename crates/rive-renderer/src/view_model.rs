//! View-model **data binding** — read/write named properties on an artboard's
//! bound default view-model instance. This is how a host (game) drives a face's
//! data (and reads values a script/state-machine wrote). Mirrors the C++ runtime
//! contract in the Rive data-binding docs (https://rive.app/docs).
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
use std::rc::Rc;

use crate::{last_error, sys, Artboard, Context, ContextInner, Error, File, Result};

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
    /// An image reference — bind a decoded [`RiveImage`] with
    /// [`Artboard::vm_set_image`] / [`RiveViewModelInstance::set_image`] (set-only).
    Image,
    /// An artboard reference — bind a [`BindableArtboard`] with
    /// [`Artboard::vm_set_artboard`] / [`RiveViewModelInstance::set_artboard`] (set-only).
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

/// A decoded **render image** — the value source for binding an image to a
/// view-model image property. Decode encoded bytes (PNG / JPEG / WEBP) with
/// [`Context::decode_image`], then bind with [`Artboard::vm_set_image`] or
/// [`RiveViewModelInstance::set_image`]. Binding takes its own reference, so a
/// `RiveImage` may be dropped afterwards without unbinding it.
///
/// It owns GPU resources on the [`Context`]'s device, so it keeps that context
/// alive and may only be bound into artboards on the **same** context (a mismatch
/// is [`Error::ContextMismatch`], not undefined behavior). `!Send + !Sync`.
pub struct RiveImage {
    ptr: *mut sys::RiveImage,
    /// Owning context: keeps the device alive *and* identifies which context this
    /// image belongs to (checked on bind, like [`Artboard`]/`RenderTarget`).
    ctx: Rc<ContextInner>,
}

impl Drop for RiveImage {
    fn drop(&mut self) {
        // SAFETY: created by the shim, destroyed exactly once; the `ctx` `Rc`
        // (dropped after this body) keeps the device alive until after this destroy.
        unsafe { sys::rive_image_destroy(self.ptr) };
    }
}

impl std::fmt::Debug for RiveImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RiveImage").finish_non_exhaustive()
    }
}

/// A file-sourced **artboard value** — the value source for binding an artboard to
/// a view-model **artboard-reference** (`propertyArtboard`) property, the artboard
/// analogue of [`RiveImage`]. Create one with [`File::bindable_artboard_named`] /
/// [`File::bindable_artboard_default`], then bind with [`Artboard::vm_set_artboard`]
/// or [`RiveViewModelInstance::set_artboard`]. Binding takes its own reference, so a
/// `BindableArtboard` may be dropped afterwards without unbinding it.
///
/// It keeps its source [`File`]'s data alive natively. Bind it only into artboards
/// on the **same** [`Context`] it was loaded under (a mismatch is
/// [`Error::ContextMismatch`], to avoid driving one device's render resources
/// through another's renderer). `!Send + !Sync`.
pub struct BindableArtboard {
    ptr: *mut sys::RiveBindableArtboard,
    /// Owning context: keeps the device alive *and* identifies which context this
    /// value belongs to (checked on bind, like [`RiveImage`]).
    ctx: Rc<ContextInner>,
}

impl Drop for BindableArtboard {
    fn drop(&mut self) {
        // SAFETY: created by the shim, destroyed exactly once; the binding (if any)
        // took its own ref, so destroying this handle does not unbind it.
        unsafe { sys::rive_bindable_artboard_destroy(self.ptr) };
    }
}

impl std::fmt::Debug for BindableArtboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BindableArtboard").finish_non_exhaustive()
    }
}

impl File {
    /// Creates a [`BindableArtboard`] from the artboard named `name` — the value
    /// source for an artboard-reference data binding (bind with
    /// [`Artboard::vm_set_artboard`]).
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoArtboard`] if the file has no artboard with that name, or
    /// `name` contained an interior NUL byte.
    pub fn bindable_artboard_named(&self, name: &str) -> Result<BindableArtboard> {
        let c = CString::new(name).map_err(|_| {
            Error::NoArtboard("bindable artboard name contained an interior NUL byte".into())
        })?;
        // SAFETY: `self.ptr` is a live file handle; `c` is a valid C string.
        let ptr = unsafe { sys::rive_file_bindable_artboard_named(self.ptr, c.as_ptr()) };
        self.wrap_bindable(ptr)
    }

    /// Creates a [`BindableArtboard`] from the file's default artboard.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoArtboard`] if the file contains no artboards.
    pub fn bindable_artboard_default(&self) -> Result<BindableArtboard> {
        // SAFETY: `self.ptr` is a live file handle.
        let ptr = unsafe { sys::rive_file_bindable_artboard_default(self.ptr) };
        self.wrap_bindable(ptr)
    }

    /// Wraps a shim bindable-artboard pointer, tying it to this file's context, or
    /// maps null to [`Error::NoArtboard`].
    fn wrap_bindable(&self, ptr: *mut sys::RiveBindableArtboard) -> Result<BindableArtboard> {
        if ptr.is_null() {
            return Err(Error::NoArtboard(last_error()));
        }
        Ok(BindableArtboard {
            ptr,
            ctx: Rc::clone(&self._ctx),
        })
    }
}

impl Context {
    /// Decodes encoded image bytes (PNG / JPEG / WEBP) into a [`RiveImage`] using
    /// this context's factory — the value source for binding an image to a
    /// view-model image property ([`Artboard::vm_set_image`]).
    ///
    /// The bytes are only borrowed for the call. The returned image is tied to this
    /// context's device; bind it only into artboards built on the same context.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Image`] if the bytes can't be decoded (unsupported / corrupt
    /// format, or no matching decoder compiled in).
    pub fn decode_image(&self, bytes: &[u8]) -> Result<RiveImage> {
        // SAFETY: `bytes` is borrowed only for this call (the shim copies what it
        // needs); a null return signals a decode failure.
        let ptr = unsafe { sys::rive_image_decode(self.inner.ptr, bytes.as_ptr(), bytes.len()) };
        if ptr.is_null() {
            return Err(Error::Image(last_error()));
        }
        Ok(RiveImage {
            ptr,
            ctx: Rc::clone(&self.inner),
        })
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

    /// Binds a decoded `image` to a view-model **image** property (a `/`-path
    /// reaches a nested view model). Get `image` from [`Context::decode_image`];
    /// binding takes its own reference, so the [`RiveImage`] may be dropped after.
    /// The change is observed on the next advance.
    ///
    /// # Errors
    ///
    /// [`Error::ContextMismatch`] if `image` was decoded by a different [`Context`]
    /// than this artboard's (binding it would drive one device's image through
    /// another's renderer); [`Error::ViewModel`] if `path` is not an image property;
    /// [`Error::InvalidPath`] for an interior NUL byte.
    pub fn vm_set_image(&self, path: &str, image: &RiveImage) -> Result<()> {
        if !Rc::ptr_eq(&self.inner.ctx, &image.ctx) {
            return Err(Error::ContextMismatch);
        }
        let c = Self::vm_path(path)?;
        // SAFETY: live artboard handle + valid C string; `image.ptr` is a live image
        // decoded by the same context (checked above).
        let st = unsafe { sys::rive_artboard_vm_set_image(self.inner.ptr, c.as_ptr(), image.ptr) };
        vm_status(st)
    }

    /// Clears a view-model **image** property — unbinds any bound image, leaving it
    /// empty. The counterpart to [`Self::vm_set_image`] (no [`RiveImage`] needed, so
    /// no context check).
    ///
    /// # Errors
    ///
    /// [`Error::ViewModel`] if `path` is not an image property; [`Error::InvalidPath`]
    /// for an interior NUL byte.
    pub fn vm_clear_image(&self, path: &str) -> Result<()> {
        let c = Self::vm_path(path)?;
        // SAFETY: live artboard handle + valid C string; a null image clears the property.
        let st =
            unsafe { sys::rive_artboard_vm_set_image(self.inner.ptr, c.as_ptr(), std::ptr::null_mut()) };
        vm_status(st)
    }

    /// Binds a file-sourced `artboard` to a view-model **artboard-reference**
    /// (`propertyArtboard`) property (a `/`-path reaches a nested view model). Get
    /// `artboard` from [`File::bindable_artboard_named`]; binding takes its own
    /// reference, so the [`BindableArtboard`] may be dropped after. The change is
    /// observed on the next advance (a `NestedArtboard` bound to this property then
    /// instances the referenced artboard).
    ///
    /// # Errors
    ///
    /// [`Error::ContextMismatch`] if `artboard` was created from a [`File`] loaded on
    /// a different [`Context`] than this artboard's; [`Error::ViewModel`] if `path`
    /// is not an artboard property; [`Error::InvalidPath`] for an interior NUL byte.
    pub fn vm_set_artboard(&self, path: &str, artboard: &BindableArtboard) -> Result<()> {
        if !Rc::ptr_eq(&self.inner.ctx, &artboard.ctx) {
            return Err(Error::ContextMismatch);
        }
        let c = Self::vm_path(path)?;
        // SAFETY: live artboard handle + valid C string; `artboard.ptr` is a live
        // bindable from the same context (checked above).
        let st =
            unsafe { sys::rive_artboard_vm_set_artboard(self.inner.ptr, c.as_ptr(), artboard.ptr) };
        vm_status(st)
    }

    /// Clears a view-model **artboard-reference** property — unbinds any bound
    /// artboard. Counterpart to [`Self::vm_set_artboard`] (no [`BindableArtboard`],
    /// so no context check).
    ///
    /// # Errors
    ///
    /// [`Error::ViewModel`] if `path` is not an artboard property; [`Error::InvalidPath`]
    /// for an interior NUL byte.
    pub fn vm_clear_artboard(&self, path: &str) -> Result<()> {
        let c = Self::vm_path(path)?;
        // SAFETY: live artboard handle + valid C string; a null bindable clears the property.
        let st = unsafe {
            sys::rive_artboard_vm_set_artboard(self.inner.ptr, c.as_ptr(), std::ptr::null_mut())
        };
        vm_status(st)
    }

    /// Observes whether the property at `path` **changed** — or, for a trigger,
    /// **fired** — on the last [`StateMachine::advance`](crate::StateMachine::advance),
    /// consuming the flag (a later call returns `false` until it changes again).
    ///
    /// This is the modern, non-deprecated replacement for events read-back: the rig
    /// signals gameplay by driving a view-model trigger/property, and the game
    /// observes it here (Rive deprecated runtime *event* listening — see
    /// `docs/feature-support.md`). Type-agnostic — works for triggers and any
    /// scalar property.
    ///
    /// **Subscribe before the first advance:** the first call subscribes the
    /// property and returns `false`; call it once at setup so the very first
    /// fire/change isn't missed, then poll each frame *after* advancing.
    pub fn vm_flush_changed(&self, path: &str) -> Result<bool> {
        let c = Self::vm_path(path)?;
        let mut out = 0_u8;
        // SAFETY: live handle + valid C string; `out` is a valid u8 slot.
        let st = unsafe { sys::rive_artboard_vm_flush_changed(self.inner.ptr, c.as_ptr(), &mut out) };
        vm_status(st).map(|()| out != 0)
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
        RiveViewModelInstance::from_ptr(p, self.inner.ctx.ptr)
    }

    /// Resolves an **indexed view-model path** to the instance that owns its leaf
    /// property, plus the leaf name — so any typed `set_*` / `get_*` on the returned
    /// [`RiveViewModelInstance`] reaches a nested view model or a **list item**.
    ///
    /// Path grammar: `/`-separated segments descend nested view models, and a
    /// `name[i]` segment indexes element `i` of the list property `name` (which the
    /// flat `vm_*` accessors can't do — rive's resolver can't index lists). The final
    /// segment names the leaf property and must not itself be indexed.
    ///
    /// ```no_run
    /// # fn demo(artboard: &rive_renderer::Artboard) -> rive_renderer::Result<()> {
    /// // Drive element 2 of the `wheels` list, then any depth of nesting:
    /// let (item, leaf) = artboard.vm_resolve("wheels[2]/value")?;
    /// item.set_number(&leaf, 5.0)?;
    /// let (tint, leaf) = artboard.vm_resolve("groups[1]/wheels[2]/tint")?;
    /// tint.set_color(&leaf, 0xFF_00_FF_00)?;
    /// # Ok(()) }
    /// ```
    ///
    /// Errors if the artboard has no view model, a segment doesn't resolve, the path
    /// is malformed (a bad `[index]`, or an interior NUL byte → `Error::InvalidPath`),
    /// or the leaf segment is itself indexed.
    pub fn vm_resolve(&self, path: &str) -> Result<(RiveViewModelInstance<'_>, String)> {
        let mut handle = self
            .vm_root()
            .ok_or_else(|| Error::ViewModel("artboard has no view model".to_string()))?;
        let mut segments: Vec<&str> = path.split('/').collect();
        // `str::split` always yields ≥1 element, so `pop` gives the leaf.
        let leaf = segments.pop().expect("split('/') always yields at least one segment");
        for seg in segments {
            handle = match parse_index_segment(seg)? {
                Some((name, index)) => handle
                    .list_item(name, index)
                    .ok_or_else(|| Error::ViewModel(format!("list item {seg:?} not found")))?,
                None => handle
                    .view_model(seg)
                    .ok_or_else(|| Error::ViewModel(format!("path segment {seg:?} not found")))?,
            };
        }
        // The leaf must name a scalar/trigger property — a list item is not writable.
        if parse_index_segment(leaf)?.is_some() {
            return Err(Error::ViewModel(format!(
                "path {path:?} ends in a list item, not a writable property"
            )));
        }
        Ok((handle, leaf.to_string()))
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

/// Parses one path segment for [`Artboard::vm_resolve`]: `"wheels[2]"` →
/// `Some(("wheels", 2))`, a plain `"breath"` → `None`. The `[i]` must close with
/// `]` as the segment's last char and hold a non-empty base-10 index; malformed
/// bracket syntax is an `Error::ViewModel` naming the offending segment.
fn parse_index_segment(seg: &str) -> Result<Option<(&str, usize)>> {
    let Some(open) = seg.find('[') else {
        return Ok(None);
    };
    let bad = || Error::ViewModel(format!("malformed list index in path segment {seg:?}"));
    let close = seg.find(']').ok_or_else(bad)?;
    // `]` must be the last char and the index non-empty (`name[3]`, not `name[]x`).
    if close != seg.len() - 1 || close <= open + 1 {
        return Err(bad());
    }
    let index = seg[open + 1..close].parse::<usize>().map_err(|_| bad())?;
    Ok(Some((&seg[..open], index)))
}

/// A borrowed **view-model instance** handle — the artboard's root view model, a
/// nested view model, or a list item. Obtained from [`Artboard::vm_root`] and
/// navigated via [`Self::view_model`] / [`Self::list_item`]; a freshly constructed
/// instance also yields one via [`RiveOwnedViewModel::borrow`].
///
/// **Borrowed:** it aliases an instance owned by rive's caches under the root view
/// model (or, from [`RiveOwnedViewModel::borrow`], the owned instance), so the borrow
/// checker ties it to the source [`Artboard`] (or owned handle). Supports scalar reads
/// **and writes** (`set_*` / [`Self::fire_trigger`]) + introspection + navigation — so
/// a caller can drive a nested view model or a **list item**, which the flat
/// artboard-rooted `vm_set_*` path can't address (rive's resolver can't index lists;
/// use [`Artboard::vm_resolve`] for the `name[i]/leaf` shorthand). It can also bind an
/// **image** property ([`Self::set_image`]) or an **artboard reference**
/// ([`Self::set_artboard`]), and **structurally mutate a list** ([`Self::list_add`] /
/// [`Self::list_remove_at`] / [`Self::list_swap`] / [`Self::list_clear`] …) or replace
/// a VM reference ([`Self::replace_view_model`]). `!Send`/`!Sync` (rive handles are not
/// thread-safe).
#[derive(Debug)]
pub struct RiveViewModelInstance<'a> {
    ptr: *mut sys::RiveViewModelInstance,
    /// The render context the owning artboard belongs to — identity only, used to
    /// reject binding an image decoded by a different context ([`Self::set_image`]).
    /// The `'a` borrow keeps the real context alive (via the artboard's `Rc`); this
    /// raw pointer is never dereferenced, only compared.
    ctx: *mut sys::RiveRenderContext,
    _marker: PhantomData<&'a Artboard>,
}

impl<'a> RiveViewModelInstance<'a> {
    /// Wraps a shim handle, returning `None` for a null pointer (no such instance).
    /// `ctx` is the owning artboard's render context, propagated to child handles
    /// for the image-bind context check.
    fn from_ptr(
        ptr: *mut sys::RiveViewModelInstance,
        ctx: *mut sys::RiveRenderContext,
    ) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Self { ptr, ctx, _marker: PhantomData })
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
        RiveViewModelInstance::from_ptr(p, self.ctx)
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
        RiveViewModelInstance::from_ptr(p, self.ctx)
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

    /// Observes whether the property at `path` **changed** — or, for a trigger,
    /// **fired** — on the last advance, consuming the flag. The handle-based
    /// counterpart of [`Artboard::vm_flush_changed`] (same prime-then-poll contract).
    pub fn flush_changed(&self, path: &str) -> Result<bool> {
        let c = Self::path(path)?;
        let mut out = 0_u8;
        // SAFETY: live handle + valid C string; `out` valid.
        let st = unsafe { sys::rive_vmi_flush_changed(self.ptr, c.as_ptr(), &mut out) };
        vm_status(st).map(|()| out != 0)
    }

    // ---- writes (number / bool / color / string / enum / trigger) ----
    // Mutate native state through the borrowed instance (interior mutability);
    // the change is observed on the next advance. These reach into a nested view
    // model or a **list item** — what the flat `Artboard::vm_set_*` path can't do.

    /// Sets a **number** property at `path` (relative to this instance).
    pub fn set_number(&self, path: &str, value: f32) -> Result<()> {
        let c = Self::path(path)?;
        // SAFETY: live handle + valid C string.
        let st = unsafe { sys::rive_vmi_set_number(self.ptr, c.as_ptr(), value) };
        vm_status(st)
    }

    /// Sets a **bool** property at `path`.
    pub fn set_bool(&self, path: &str, value: bool) -> Result<()> {
        let c = Self::path(path)?;
        // SAFETY: live handle + valid C string.
        let st = unsafe { sys::rive_vmi_set_bool(self.ptr, c.as_ptr(), u8::from(value)) };
        vm_status(st)
    }

    /// Sets a **color** property at `path` (ARGB).
    pub fn set_color(&self, path: &str, argb: u32) -> Result<()> {
        let c = Self::path(path)?;
        // SAFETY: live handle + valid C string.
        let st = unsafe { sys::rive_vmi_set_color(self.ptr, c.as_ptr(), argb) };
        vm_status(st)
    }

    /// Sets a **string** property at `path`.
    pub fn set_string(&self, path: &str, value: &str) -> Result<()> {
        let c = Self::path(path)?;
        let v = CString::new(value).map_err(|_| Error::InvalidPath)?;
        // SAFETY: live handle + valid C strings.
        let st = unsafe { sys::rive_vmi_set_string(self.ptr, c.as_ptr(), v.as_ptr()) };
        vm_status(st)
    }

    /// Sets an **enum** property at `path` by 0-based value index.
    pub fn set_enum_index(&self, path: &str, index: u32) -> Result<()> {
        let c = Self::path(path)?;
        // SAFETY: live handle + valid C string.
        let st = unsafe { sys::rive_vmi_set_enum_index(self.ptr, c.as_ptr(), index) };
        vm_status(st)
    }

    /// Sets an **enum** property at `path` by value label (name).
    pub fn set_enum_name(&self, path: &str, name: &str) -> Result<()> {
        let c = Self::path(path)?;
        let n = CString::new(name).map_err(|_| Error::InvalidPath)?;
        // SAFETY: live handle + valid C strings.
        let st = unsafe { sys::rive_vmi_set_enum_name(self.ptr, c.as_ptr(), n.as_ptr()) };
        vm_status(st)
    }

    /// Fires a **trigger** property at `path` (one-shot pulse).
    pub fn fire_trigger(&self, path: &str) -> Result<()> {
        let c = Self::path(path)?;
        // SAFETY: live handle + valid C string.
        let st = unsafe { sys::rive_vmi_fire_trigger(self.ptr, c.as_ptr()) };
        vm_status(st)
    }

    /// Binds a decoded `image` to an **image** property at `path` — reaching a
    /// nested view model or a **list item** (what the flat [`Artboard::vm_set_image`]
    /// can't). Get `image` from [`Context::decode_image`]; binding takes its own
    /// reference, so the [`RiveImage`] may be dropped after.
    ///
    /// # Errors
    ///
    /// [`Error::ContextMismatch`] if `image` came from a different [`Context`] than
    /// the artboard this handle belongs to; [`Error::ViewModel`] if `path` is not an
    /// image property; [`Error::InvalidPath`] for an interior NUL byte.
    pub fn set_image(&self, path: &str, image: &RiveImage) -> Result<()> {
        if self.ctx != image.ctx.ptr {
            return Err(Error::ContextMismatch);
        }
        let c = Self::path(path)?;
        // SAFETY: live handle + valid C string; `image.ptr` is a live image decoded
        // by the same context (checked above).
        let st = unsafe { sys::rive_vmi_set_image(self.ptr, c.as_ptr(), image.ptr) };
        vm_status(st)
    }

    /// Clears an **image** property — unbinds any bound image. Counterpart to
    /// [`Self::set_image`] (no [`RiveImage`], so no context check).
    ///
    /// # Errors
    ///
    /// [`Error::ViewModel`] if `path` is not an image property; [`Error::InvalidPath`]
    /// for an interior NUL byte.
    pub fn clear_image(&self, path: &str) -> Result<()> {
        let c = Self::path(path)?;
        // SAFETY: live handle + valid C string; a null image clears the property.
        let st = unsafe { sys::rive_vmi_set_image(self.ptr, c.as_ptr(), std::ptr::null_mut()) };
        vm_status(st)
    }

    /// Binds a file-sourced `artboard` to an **artboard-reference** property on a
    /// nested view model or list item (which the flat artboard-rooted path can't
    /// address). See [`Artboard::vm_set_artboard`] for the binding semantics.
    ///
    /// # Errors
    ///
    /// [`Error::ContextMismatch`] if `artboard` came from a different [`Context`] than
    /// the artboard this handle belongs to; [`Error::ViewModel`] if `path` is not an
    /// artboard property; [`Error::InvalidPath`] for an interior NUL byte.
    pub fn set_artboard(&self, path: &str, artboard: &BindableArtboard) -> Result<()> {
        if self.ctx != artboard.ctx.ptr {
            return Err(Error::ContextMismatch);
        }
        let c = Self::path(path)?;
        // SAFETY: live handle + valid C string; `artboard.ptr` is a live bindable
        // from the same context (checked above).
        let st = unsafe { sys::rive_vmi_set_artboard(self.ptr, c.as_ptr(), artboard.ptr) };
        vm_status(st)
    }

    /// Clears an **artboard-reference** property — unbinds any bound artboard.
    /// Counterpart to [`Self::set_artboard`] (no [`BindableArtboard`], so no context check).
    ///
    /// # Errors
    ///
    /// [`Error::ViewModel`] if `path` is not an artboard property; [`Error::InvalidPath`]
    /// for an interior NUL byte.
    pub fn clear_artboard(&self, path: &str) -> Result<()> {
        let c = Self::path(path)?;
        // SAFETY: live handle + valid C string; a null bindable clears the property.
        let st = unsafe { sys::rive_vmi_set_artboard(self.ptr, c.as_ptr(), std::ptr::null_mut()) };
        vm_status(st)
    }

    // ---- LIST structural mutation (add / remove / swap / clear) ----
    // Operate on the **list** property at `path` on this instance. A structural edit
    // marks data bindings dirty, so it takes effect on the next advance. Indices are
    // positional and shift on add/remove/swap: after a mutation, re-fetch list items
    // via [`Self::list_item`] (a handle to a *removed* item is invalid; handles to
    // surviving items stay valid but their index may have changed). The `item` to add
    // is typically a freshly built [`RiveOwnedViewModel`] borrowed with
    // [`RiveOwnedViewModel::borrow`]; once added, the list co-owns it.

    /// Appends `item` to the end of the **list** property at `path`.
    ///
    /// # Errors
    ///
    /// [`Error::ViewModel`] if `path` is not a list property; [`Error::InvalidPath`]
    /// for an interior NUL byte.
    pub fn list_add(&self, path: &str, item: &RiveViewModelInstance<'_>) -> Result<()> {
        let c = Self::path(path)?;
        // SAFETY: live handle + valid C string; `item.ptr` is a live instance handle.
        let st = unsafe { sys::rive_vmi_list_add(self.ptr, c.as_ptr(), item.ptr) };
        vm_status(st)
    }

    /// Inserts `item` at `index` in the **list** at `path` (valid range `0..=len`;
    /// `index == len` appends).
    ///
    /// # Errors
    ///
    /// [`Error::ViewModel`] if `path` is not a list or `index` is out of range;
    /// [`Error::InvalidPath`] for an interior NUL byte.
    pub fn list_add_at(
        &self,
        path: &str,
        item: &RiveViewModelInstance<'_>,
        index: usize,
    ) -> Result<()> {
        let c = Self::path(path)?;
        // SAFETY: live handle + valid C string; `item.ptr` is a live instance handle.
        let st = unsafe { sys::rive_vmi_list_add_at(self.ptr, c.as_ptr(), item.ptr, index as u32) };
        vm_status(st)
    }

    /// Removes **every** occurrence of `item` (matched by underlying instance) from
    /// the **list** at `path`. Get `item` from [`Self::list_item`].
    ///
    /// # Errors
    ///
    /// [`Error::ViewModel`] if `path` is not a list property; [`Error::InvalidPath`]
    /// for an interior NUL byte.
    pub fn list_remove(&self, path: &str, item: &RiveViewModelInstance<'_>) -> Result<()> {
        let c = Self::path(path)?;
        // SAFETY: live handle + valid C string; `item.ptr` is a live instance handle.
        let st = unsafe { sys::rive_vmi_list_remove(self.ptr, c.as_ptr(), item.ptr) };
        vm_status(st)
    }

    /// Removes the item at `index` from the **list** at `path`.
    ///
    /// # Errors
    ///
    /// [`Error::ViewModel`] if `path` is not a list or `index` is out of range;
    /// [`Error::InvalidPath`] for an interior NUL byte.
    pub fn list_remove_at(&self, path: &str, index: usize) -> Result<()> {
        let c = Self::path(path)?;
        // SAFETY: live handle + valid C string.
        let st = unsafe { sys::rive_vmi_list_remove_at(self.ptr, c.as_ptr(), index as u32) };
        vm_status(st)
    }

    /// Swaps the items at `a` and `b` in the **list** at `path`.
    ///
    /// # Errors
    ///
    /// [`Error::ViewModel`] if `path` is not a list or either index is out of range;
    /// [`Error::InvalidPath`] for an interior NUL byte.
    pub fn list_swap(&self, path: &str, a: usize, b: usize) -> Result<()> {
        let c = Self::path(path)?;
        // SAFETY: live handle + valid C string.
        let st = unsafe { sys::rive_vmi_list_swap(self.ptr, c.as_ptr(), a as u32, b as u32) };
        vm_status(st)
    }

    /// Removes all items from the **list** at `path`, leaving it empty.
    ///
    /// # Errors
    ///
    /// [`Error::ViewModel`] if `path` is not a list property; [`Error::InvalidPath`]
    /// for an interior NUL byte.
    pub fn list_clear(&self, path: &str) -> Result<()> {
        let c = Self::path(path)?;
        // SAFETY: live handle + valid C string.
        let st = unsafe { sys::rive_vmi_list_clear(self.ptr, c.as_ptr()) };
        vm_status(st)
    }

    /// Assigns `value` to the **view-model-reference** property at `path` (`/`
    /// descends). `value` is typically a freshly built [`RiveOwnedViewModel`] borrowed
    /// with [`RiveOwnedViewModel::borrow`]; once assigned, the parent co-owns it.
    ///
    /// # Errors
    ///
    /// [`Error::ViewModel`] if `path` is not a view-model-reference property, or
    /// `value`'s view-model **type** doesn't match the property's referenced type
    /// (rive enforces the match); [`Error::InvalidPath`] for an interior NUL byte.
    pub fn replace_view_model(
        &self,
        path: &str,
        value: &RiveViewModelInstance<'_>,
    ) -> Result<()> {
        let c = Self::path(path)?;
        // SAFETY: live handle + valid C string; `value.ptr` is a live instance handle.
        let st = unsafe { sys::rive_vmi_replace_view_model(self.ptr, c.as_ptr(), value.ptr) };
        vm_status(st)
    }
}

/// View-model **construction** accessors — reach a view-model *definition*
/// ([`RiveViewModelRuntime`]) to mint fresh instances for lists / VM-references, plus
/// artboard-sourced [`BindableArtboard`] values (so a caller holding only an
/// [`Artboard`] can bind a `propertyArtboard` without its [`File`]).
impl Artboard {
    /// Number of view-model definitions in this artboard's file.
    pub fn view_model_count(&self) -> usize {
        // SAFETY: live artboard handle (0 if it has no file).
        unsafe { sys::rive_artboard_view_model_count(self.inner.ptr) as usize }
    }

    /// The names of the file's view-model **definitions** (index ↔ name, for
    /// [`Self::view_model_by_index`] / [`Self::view_model_by_name`]) — mirrors
    /// [`Self::artboard_names`](crate::Artboard) / `state_machine_names` for discovery.
    pub fn view_model_names(&self) -> Vec<String> {
        (0..self.view_model_count())
            .filter_map(|i| self.view_model_by_index(i).map(|d| d.name()))
            .collect()
    }

    /// The view-model **definition** named `name` (used to construct instances), or
    /// `None` if the artboard has no file or no such definition.
    ///
    /// PERF: rive allocates a fresh runtime each call and caches it for the file's
    /// lifetime (it is never released early), so resolve a definition **once** and
    /// reuse the returned handle across many `create_*` calls rather than re-fetching
    /// it per construction — a hot construct-and-resolve loop grows that cache.
    pub fn view_model_by_name(&self, name: &str) -> Option<RiveViewModelRuntime<'_>> {
        let c = Self::vm_path(name).ok()?;
        // SAFETY: live artboard handle + valid C string; null if not found.
        let p = unsafe { sys::rive_artboard_view_model_by_name(self.inner.ptr, c.as_ptr()) };
        RiveViewModelRuntime::from_ptr(p, self.inner.ctx.ptr)
    }

    /// The view-model **definition** at `index`, or `None` if out of range.
    pub fn view_model_by_index(&self, index: usize) -> Option<RiveViewModelRuntime<'_>> {
        // SAFETY: live artboard handle; null if out of range.
        let p = unsafe { sys::rive_artboard_view_model_by_index(self.inner.ptr, index as u32) };
        RiveViewModelRuntime::from_ptr(p, self.inner.ctx.ptr)
    }

    /// The view-model **definition** bound to this artboard (the type of its own root
    /// view model), or `None` if the artboard has no linked view model. Handy for
    /// minting another instance of the same type (e.g. to add to a list of it).
    pub fn default_view_model(&self) -> Option<RiveViewModelRuntime<'_>> {
        // SAFETY: live artboard handle; null if no linked view model.
        let p = unsafe { sys::rive_artboard_default_view_model(self.inner.ptr) };
        RiveViewModelRuntime::from_ptr(p, self.inner.ctx.ptr)
    }

    /// Creates a [`BindableArtboard`] from the artboard named `name` in **this
    /// artboard's own file** — the artboard analogue of [`File::bindable_artboard_named`]
    /// for a caller that holds only an [`Artboard`] (e.g. after the [`File`] is
    /// dropped). Bind it with [`Artboard::vm_set_artboard`].
    ///
    /// # Errors
    ///
    /// [`Error::NoArtboard`] if the file has no artboard with that name, the artboard
    /// has no file, or `name` contained an interior NUL byte.
    pub fn bindable_artboard_named(&self, name: &str) -> Result<BindableArtboard> {
        let c = CString::new(name).map_err(|_| {
            Error::NoArtboard("bindable artboard name contained an interior NUL byte".into())
        })?;
        // SAFETY: live artboard handle + valid C string.
        let ptr = unsafe { sys::rive_artboard_bindable_artboard_named(self.inner.ptr, c.as_ptr()) };
        self.wrap_artboard_bindable(ptr)
    }

    /// Creates a [`BindableArtboard`] from the **default** artboard of this artboard's
    /// own file. See [`Self::bindable_artboard_named`].
    ///
    /// # Errors
    ///
    /// [`Error::NoArtboard`] if the file contains no artboards (or the artboard has no file).
    pub fn bindable_artboard_default(&self) -> Result<BindableArtboard> {
        // SAFETY: live artboard handle.
        let ptr = unsafe { sys::rive_artboard_bindable_artboard_default(self.inner.ptr) };
        self.wrap_artboard_bindable(ptr)
    }

    /// Wraps a shim bindable-artboard pointer, tying it to this artboard's context, or
    /// maps null to [`Error::NoArtboard`].
    fn wrap_artboard_bindable(
        &self,
        ptr: *mut sys::RiveBindableArtboard,
    ) -> Result<BindableArtboard> {
        if ptr.is_null() {
            return Err(Error::NoArtboard(last_error()));
        }
        Ok(BindableArtboard {
            ptr,
            ctx: Rc::clone(&self.inner.ctx),
        })
    }
}

/// A view-model **definition** (`rive::ViewModelRuntime`) — the schema/type a
/// view-model instance is an instance *of*. Obtained from [`Artboard::view_model_by_name`]
/// / [`Artboard::view_model_by_index`] / [`Artboard::default_view_model`]; used to mint
/// fresh [`RiveOwnedViewModel`] instances to add to a list or assign to a VM-reference
/// property.
///
/// **Borrowed:** owned by the file, so the borrow checker ties it to the source
/// [`Artboard`] (the file lives as long as the artboard). `!Send`/`!Sync`.
///
/// ```no_run
/// # fn demo(artboard: &rive_renderer::Artboard) -> rive_renderer::Result<()> {
/// // Build a fresh list item, populate it, and append it to a list property.
/// let def = artboard.view_model_by_name("WheelItem").expect("view model exists");
/// let item = def.create_instance()?;      // a caller-owned instance
/// item.borrow().set_number("value", 7.0)?;
/// let root = artboard.vm_root().expect("artboard has a view model");
/// root.list_add("wheels", &item.borrow())?; // the list now co-owns it
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct RiveViewModelRuntime<'a> {
    ptr: *mut sys::RiveViewModelRuntime,
    /// The owning artboard's render context — identity only, propagated to created
    /// instances so a later image bind can reject a cross-context image (never deref'd;
    /// the `'a` borrow keeps the real context alive).
    ctx: *mut sys::RiveRenderContext,
    _marker: PhantomData<&'a Artboard>,
}

impl<'a> RiveViewModelRuntime<'a> {
    /// Wraps a shim runtime pointer, returning `None` for null (no such definition).
    fn from_ptr(
        ptr: *mut sys::RiveViewModelRuntime,
        ctx: *mut sys::RiveRenderContext,
    ) -> Option<Self> {
        if ptr.is_null() {
            None
        } else {
            Some(Self { ptr, ctx, _marker: PhantomData })
        }
    }

    /// Wraps a freshly-minted owned-instance pointer, mapping null to the shim error.
    fn wrap_owned(&self, ptr: *mut sys::RiveOwnedVmInstance) -> Result<RiveOwnedViewModel<'a>> {
        if ptr.is_null() {
            Err(Error::ViewModel(last_error()))
        } else {
            Ok(RiveOwnedViewModel { ptr, ctx: self.ctx, _marker: PhantomData })
        }
    }

    /// Mints a **blank** instance (all default property values).
    ///
    /// # Errors
    ///
    /// [`Error::ViewModel`] if construction fails (e.g. allocation failure).
    pub fn create_instance(&self) -> Result<RiveOwnedViewModel<'a>> {
        // SAFETY: live runtime handle.
        let p = unsafe { sys::rive_view_model_create_instance(self.ptr) };
        self.wrap_owned(p)
    }

    /// Mints the editor's **default** instance (falls back to a blank instance if the
    /// definition authors no default).
    ///
    /// # Errors
    ///
    /// [`Error::ViewModel`] if construction fails.
    pub fn create_default_instance(&self) -> Result<RiveOwnedViewModel<'a>> {
        // SAFETY: live runtime handle.
        let p = unsafe { sys::rive_view_model_create_default_instance(self.ptr) };
        self.wrap_owned(p)
    }

    /// Mints a clone of the editor instance named `name`.
    ///
    /// # Errors
    ///
    /// [`Error::ViewModel`] if no editor instance has that name; [`Error::InvalidPath`]
    /// for an interior NUL byte.
    pub fn create_instance_from_name(&self, name: &str) -> Result<RiveOwnedViewModel<'a>> {
        let c = CString::new(name).map_err(|_| Error::InvalidPath)?;
        // SAFETY: live runtime handle + valid C string.
        let p = unsafe { sys::rive_view_model_create_instance_from_name(self.ptr, c.as_ptr()) };
        self.wrap_owned(p)
    }

    /// Mints a clone of the editor instance at `index`.
    ///
    /// # Errors
    ///
    /// [`Error::ViewModel`] if `index` is out of range.
    pub fn create_instance_from_index(&self, index: usize) -> Result<RiveOwnedViewModel<'a>> {
        // SAFETY: live runtime handle.
        let p = unsafe { sys::rive_view_model_create_instance_from_index(self.ptr, index as u32) };
        self.wrap_owned(p)
    }

    /// This definition's name.
    pub fn name(&self) -> String {
        // SAFETY: live runtime handle; the shim's two-call protocol.
        read_string_via(|buf, cap, out_len| unsafe {
            sys::rive_view_model_name(self.ptr, buf, cap, out_len)
        })
        .unwrap_or_default()
    }

    /// Number of editor-authored named instances (the names
    /// [`Self::create_instance_from_name`] can clone).
    pub fn instance_count(&self) -> usize {
        // SAFETY: live runtime handle.
        unsafe { sys::rive_view_model_instance_count(self.ptr) as usize }
    }

    /// The names of the editor-authored instances (index ↔ name, for
    /// [`Self::create_instance_from_index`] / [`Self::create_instance_from_name`]).
    pub fn instance_names(&self) -> Vec<String> {
        (0..self.instance_count())
            .filter_map(|i| {
                // SAFETY: live runtime handle; the shim's two-call protocol.
                read_string_via(|buf, cap, out_len| unsafe {
                    sys::rive_view_model_instance_name_at(self.ptr, i as u32, buf, cap, out_len)
                })
                .ok()
            })
            .collect()
    }
}

/// An **owned**, freshly-constructed view-model instance — minted by
/// [`RiveViewModelRuntime::create_instance`] (and friends). Populate it via
/// [`Self::borrow`] (which yields the same read/write [`RiveViewModelInstance`]
/// surface), then add it to a list ([`RiveViewModelInstance::list_add`]) or assign it
/// to a VM-reference property ([`RiveViewModelInstance::replace_view_model`]) — after
/// which the list/parent co-owns it and this handle may be dropped.
///
/// The Rust lifetime `'a` ties it to the source [`Artboard`], whose native file keeps
/// the instance's backing data alive. `!Send`/`!Sync`.
#[derive(Debug)]
pub struct RiveOwnedViewModel<'a> {
    ptr: *mut sys::RiveOwnedVmInstance,
    /// Owning artboard's render context — identity only (see [`RiveViewModelRuntime`]).
    ctx: *mut sys::RiveRenderContext,
    _marker: PhantomData<&'a Artboard>,
}

impl Drop for RiveOwnedViewModel<'_> {
    fn drop(&mut self) {
        // SAFETY: created by the shim, destroyed exactly once. Dropping releases only
        // *our* ref: if the instance was added to a list / assigned, that co-owner
        // keeps it alive; otherwise this frees it.
        unsafe { sys::rive_owned_vmi_destroy(self.ptr) };
    }
}

impl RiveOwnedViewModel<'_> {
    /// Borrows this owned instance as a [`RiveViewModelInstance`] to read/write its
    /// properties (before or after adding it to a list). The borrow is tied to `self`.
    pub fn borrow(&self) -> RiveViewModelInstance<'_> {
        // SAFETY: live owned handle; the shim returns the inner instance pointer,
        // non-null for any handle we hold (we never construct a null owned handle).
        let p = unsafe { sys::rive_owned_vmi_borrow(self.ptr) };
        RiveViewModelInstance::from_ptr(p, self.ctx)
            .expect("owned view-model instance borrow returned null")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_indexed_segments() {
        assert_eq!(parse_index_segment("breath").unwrap(), None);
        assert_eq!(parse_index_segment("wheels[2]").unwrap(), Some(("wheels", 2)));
        assert_eq!(parse_index_segment("a[0]").unwrap(), Some(("a", 0)));
        assert_eq!(parse_index_segment("list[10]").unwrap(), Some(("list", 10)));
    }

    #[test]
    fn rejects_malformed_index_segments() {
        // Missing close, empty index, trailing chars, non-numeric, negative.
        for bad in ["wheels[2", "wheels[]", "wheels[2]x", "wheels[a]", "wheels[-1]"] {
            assert!(
                matches!(parse_index_segment(bad), Err(Error::ViewModel(_))),
                "{bad:?} should be a malformed-index error"
            );
        }
    }
}
