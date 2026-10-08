//! Stable highlight identifiers and protocol emission.

use std::collections::{BTreeMap, BTreeSet};

use ox_types::{Dict, Object, OxStr};
use thiserror::Error;

/// Maximum highlight group name length (`MAX_SYN_NAME`).
const MAX_GROUP_NAME_LEN: usize = 200;
/// Maximum highlight group id (`MAX_HL_ID`).
const MAX_GROUP_ID: u64 = 20_000;
/// Low 32 bits of an attribute id address the definition slot; higher
/// bits carry the slot's reuse generation. Reusing a vacated slot
/// yields a numerically distinct id, so cell diffs keyed on the
/// numeric id always observe the change and each emitted id is
/// defined exactly once — `hl_attr_define` never redefines a live id.
const SLOT_MASK: u64 = 0xFFFF_FFFF;

/// RGB or terminal highlight attributes.
#[expect(
    clippy::struct_excessive_bools,
    reason = "Neovim's highlight protocol defines these independent style flags as separate booleans"
)]
#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct HlAttrs {
    /// Foreground color (`0xRRGGBB`) or terminal color index.
    pub foreground: Option<u32>,
    /// Background color (`0xRRGGBB`) or terminal color index.
    pub background: Option<u32>,
    /// Special color (`0xRRGGBB`) or terminal color index.
    pub special: Option<u32>,
    /// Bold text.
    pub bold: bool,
    /// Italic text.
    pub italic: bool,
    /// Underlined text.
    pub underline: bool,
    /// Undercurled text.
    pub undercurl: bool,
    /// Double-underlined text.
    pub underdouble: bool,
    /// Dotted-underlined text.
    pub underdotted: bool,
    /// Dashed-underlined text.
    pub underdashed: bool,
    /// Struck-through text.
    pub strikethrough: bool,
    /// Reverse foreground and background.
    pub reverse: bool,
    /// Standout text (distinct from `reverse` in the API surface).
    pub standout: bool,
    /// Alternative font.
    pub altfont: bool,
    /// Faint text.
    pub dim: bool,
    /// Blinking text.
    pub blink: bool,
    /// Concealed text.
    pub conceal: bool,
    /// Overlined text.
    pub overline: bool,
    /// Attribute combination is suppressed.
    pub nocombine: bool,
    /// Blend percentage from zero through one hundred.
    pub blend: Option<u8>,
    /// Clickable hyperlink URL.
    pub url: Option<OxStr>,
    /// Foreground is a terminal color index, not an RGB value.
    pub fg_indexed: bool,
    /// Background is a terminal color index, not an RGB value.
    pub bg_indexed: bool,
}

impl HlAttrs {
    /// Converts attributes to the ordered dictionary used by `hl_attr_define`.
    #[must_use]
    pub fn to_object(&self) -> Object {
        let mut entries = Vec::new();
        push_color(&mut entries, "foreground", self.foreground);
        push_color(&mut entries, "background", self.background);
        push_color(&mut entries, "special", self.special);
        push_flag(&mut entries, "bold", self.bold);
        push_flag(&mut entries, "standout", self.standout);
        push_flag(&mut entries, "italic", self.italic);
        push_flag(&mut entries, "underline", self.underline);
        push_flag(&mut entries, "undercurl", self.undercurl);
        push_flag(&mut entries, "underdouble", self.underdouble);
        push_flag(&mut entries, "underdotted", self.underdotted);
        push_flag(&mut entries, "underdashed", self.underdashed);
        push_flag(&mut entries, "strikethrough", self.strikethrough);
        push_flag(&mut entries, "reverse", self.reverse);
        push_flag(&mut entries, "altfont", self.altfont);
        push_flag(&mut entries, "dim", self.dim);
        push_flag(&mut entries, "blink", self.blink);
        push_flag(&mut entries, "conceal", self.conceal);
        push_flag(&mut entries, "overline", self.overline);
        push_flag(&mut entries, "nocombine", self.nocombine);
        push_flag(&mut entries, "fg_indexed", self.fg_indexed);
        push_flag(&mut entries, "bg_indexed", self.bg_indexed);
        if let Some(blend) = self.blend {
            entries.push((
                OxStr::from("blend"),
                Object::Integer(i64::from(blend.min(100))),
            ));
        }
        if let Some(url) = &self.url {
            entries.push((OxStr::from("url"), Object::String(url.clone())));
        }
        Object::Dict(Dict(entries))
    }
}

/// Metadata identifying the source highlight group.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct HlInfo {
    /// Kind: `ui`, `syntax`, or `terminal`.
    pub kind: OxStr,
    /// Final highlight group defining the attributes.
    pub hi_name: Option<OxStr>,
    /// Built-in UI group name, for `ui` entries.
    pub ui_name: Option<OxStr>,
    /// Unique numeric source identifier.
    pub id: Option<i64>,
}

impl HlInfo {
    /// Converts metadata to the dictionary carried in the `info` array.
    #[must_use]
    pub fn to_object(&self) -> Object {
        let mut values = vec![(OxStr::from("kind"), Object::String(self.kind.clone()))];
        if let Some(name) = &self.hi_name {
            values.push((OxStr::from("hi_name"), Object::String(name.clone())));
        }
        if let Some(name) = &self.ui_name {
            values.push((OxStr::from("ui_name"), Object::String(name.clone())));
        }
        if let Some(id) = self.id {
            values.push((OxStr::from("id"), Object::Integer(id)));
        }
        Object::Dict(Dict(values))
    }
}

/// One highlight definition and its fallback data.
#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct Highlight {
    /// RGB attributes.
    pub rgb: HlAttrs,
    /// Cterm fallback attributes.
    pub cterm: HlAttrs,
    /// Whether cterm attributes were explicitly supplied (vs inherited from gui).
    pub cterm_explicit: bool,
    /// Whether `default=true` was set (don't override existing definition).
    pub default_flag: bool,
    /// Source metadata entries.
    pub info: Vec<HlInfo>,
}

/// Canonical API-level definition of a highlight group, mirroring upstream
/// `HlGroup` state: rgb and cterm attribute sets, cterm color indices, link
/// target, font, and the `default` flag.
#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct HlDef {
    /// Gui (RGB) attributes.
    pub rgb: HlAttrs,
    /// Cterm attribute flags (colors are carried by [`HlDef::cterm_fg`]
    /// and [`HlDef::cterm_bg`]).
    pub cterm: HlAttrs,
    /// Cterm foreground color index.
    pub cterm_fg: Option<u32>,
    /// Cterm background color index.
    pub cterm_bg: Option<u32>,
    /// Linked target group id.
    pub link: Option<u64>,
    /// The link resolves in the global (`ns 0`) namespace.
    pub link_global: bool,
    /// Gui font name.
    pub font: Option<OxStr>,
    /// Whether `default=true` was set (don't override existing definition).
    pub default_flag: bool,
}

/// Highlight protocol event.
#[derive(Clone, Debug, PartialEq)]
pub struct HlEvent {
    /// Event name.
    pub name: &'static str,
    /// Event arguments.
    pub args: Vec<Object>,
}

/// Highlight state failures.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum HlError {
    /// Attribute identifiers have exhausted the signed protocol range.
    #[error("highlight identifier space exhausted")]
    IdExhausted,
    /// A requested identifier does not exist.
    #[error("unknown highlight id {0}")]
    UnknownId(u64),
    /// A group name contains an invalid character (`E5248`).
    #[error("Vim:E5248: Invalid character in group name")]
    InvalidGroupName,
    /// A group name contains an unprintable character (`E669`).
    #[error("Vim:E669: Unprintable character in group name")]
    UnprintableGroupName,
    /// A group name exceeds the length limit (`E1249`).
    #[error("Vim:E1249: Highlight group name too long")]
    GroupNameTooLong,
    /// The group table has exhausted its identifier space (`E849`).
    #[error("Vim:E849: Too many highlight and syntax groups")]
    TooManyGroups,
}

/// Stable highlight table. Identifier zero is always the default group.
#[derive(Clone, Debug)]
pub struct HlState {
    definitions: Vec<Highlight>,
    /// Whether each definition slot holds live attributes; vacated
    /// slots keep their last entry until `intern` reuses them.
    occupied: Vec<bool>,
    /// Name bindings referencing each slot. A slot whose count returns
    /// to zero after having been bound is vacated into `free`.
    refcounts: Vec<u32>,
    /// Definition slots `intern` may reuse, in vacate order.
    free: Vec<u64>,
    /// Reuse generation per slot, forming each id's high bits.
    generations: Vec<u64>,
    ids: BTreeMap<Highlight, u64>,
    groups: BTreeMap<OxStr, u64>,
    /// Group name to stable group id (`syn_check_group` registry).
    group_ids: BTreeMap<OxStr, u64>,
    /// Group id to the last explicitly-set canonical definition.
    group_defs: BTreeMap<u64, HlDef>,
    /// Link target to the group names linking at it, split by
    /// resolution scope: `local_dependents` lists links resolved in
    /// this namespace, `global_dependents` lists `link_global` links
    /// resolved against namespace 0.
    local_dependents: BTreeMap<u64, BTreeSet<OxStr>>,
    /// `link_global` dependents of each namespace-0 group id.
    global_dependents: BTreeMap<u64, BTreeSet<OxStr>>,
    /// Next group id; group ids are one-based and stable per name.
    next_group_id: u64,
}

impl Default for HlState {
    fn default() -> Self {
        Self::new()
    }
}

impl HlState {
    /// Creates a table containing the default group at identifier zero.
    #[must_use]
    pub fn new() -> Self {
        let default = Highlight::default();
        let mut ids = BTreeMap::new();
        ids.insert(default.clone(), 0);
        Self {
            definitions: vec![default],
            occupied: vec![true],
            refcounts: vec![0],
            free: Vec::new(),
            generations: vec![0],
            ids,
            groups: BTreeMap::new(),
            group_ids: BTreeMap::new(),
            group_defs: BTreeMap::new(),
            local_dependents: BTreeMap::new(),
            global_dependents: BTreeMap::new(),
            next_group_id: 1,
        }
    }

    /// Creates the render table with the standard syntax groups the editor
    /// establishes while initializing highlighting (`Comment`, `String`).
    #[must_use]
    pub fn with_default_syntax_groups() -> Self {
        let mut state = Self::new();
        let comment = Highlight {
            rgb: HlAttrs {
                foreground: Some(0x0000_00ff),
                ..HlAttrs::default()
            },
            ..Highlight::default()
        };
        let string = Highlight {
            rgb: HlAttrs {
                foreground: Some(0x0000_00ff),
                bold: true,
                ..HlAttrs::default()
            },
            ..Highlight::default()
        };
        let _ = state.define_group("Comment", comment);
        let _ = state.define_group("String", string);
        state
    }

    /// Interns an attribute set, returning its stable id and an event only once.
    ///
    /// Reuses a vacated slot when one is available: the reuse bumps the
    /// slot's generation, so the minted id is numerically new and its
    /// `hl_attr_define` defines it for the first time — consumers can
    /// never confuse it with the vacated id a cell may still hold.
    ///
    /// # Errors
    ///
    /// Returns [`HlError::IdExhausted`] when no identifier remains in the
    /// signed range supported by the protocol.
    pub fn intern(&mut self, highlight: Highlight) -> Result<(u64, Option<HlEvent>), HlError> {
        if let Some(id) = self.ids.get(&highlight) {
            return Ok((*id, None));
        }
        let id = if let Some(index) = self.free.pop() {
            let index = usize::try_from(index).map_err(|_| HlError::IdExhausted)?;
            self.definitions[index] = highlight.clone();
            self.occupied[index] = true;
            self.refcounts[index] = 0;
            self.generations[index] = self.generations[index].saturating_add(1);
            self.slot_id(index)
        } else {
            let index = self.definitions.len();
            let id = u64::try_from(index).map_err(|_| HlError::IdExhausted)?;
            i64::try_from(id).map_err(|_| HlError::IdExhausted)?;
            self.definitions.push(highlight.clone());
            self.occupied.push(true);
            self.refcounts.push(0);
            self.generations.push(0);
            id
        };
        let event = define_event(id, &highlight);
        self.ids.insert(highlight, id);
        Ok((id, Some(event)))
    }

    /// Replaces a definition while retaining its identifier.
    ///
    /// # Errors
    ///
    /// Returns [`HlError::UnknownId`] when `id` does not identify an existing
    /// highlight.
    pub fn redefine(&mut self, id: u64, highlight: Highlight) -> Result<Option<HlEvent>, HlError> {
        let index = self.slot_index(id).ok_or(HlError::UnknownId(id))?;
        let existing = &mut self.definitions[index];
        if *existing == highlight {
            return Ok(None);
        }
        if self.ids.get(existing) == Some(&id) {
            self.ids.remove(existing);
        }
        let event = define_event(id, &highlight);
        self.ids.entry(highlight.clone()).or_insert(id);
        *existing = highlight;
        Ok(Some(event))
    }

    /// Associates a named highlight group with an id, emitting only on change.
    ///
    /// # Errors
    ///
    /// Returns [`HlError::UnknownId`] when `id` does not identify an existing
    /// highlight, or [`HlError::IdExhausted`] when it cannot be represented in
    /// the signed protocol range.
    pub fn set_group(
        &mut self,
        name: impl Into<OxStr>,
        id: u64,
    ) -> Result<Option<HlEvent>, HlError> {
        let index = self.slot_index(id).ok_or(HlError::UnknownId(id))?;
        let name = name.into();
        if self.groups.get(&name) == Some(&id) {
            return Ok(None);
        }
        if let Some(vacated) = self.groups.insert(name.clone(), id) {
            self.release_binding(vacated);
        }
        self.refcounts[index] = self.refcounts[index].saturating_add(1);
        Ok(Some(HlEvent {
            name: "hl_group_set",
            args: vec![
                Object::String(name),
                Object::Integer(i64::try_from(id).map_err(|_| HlError::IdExhausted)?),
            ],
        }))
    }

    /// Returns a definition by id.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<&Highlight> {
        self.slot_index(id).map(|index| &self.definitions[index])
    }

    /// Numeric id for `index`'s current generation (`SLOT_MASK` layout).
    fn slot_id(&self, index: usize) -> u64 {
        (self.generations[index] << 32) | index as u64
    }

    /// Definition slot a full id addresses, or `None` when the slot is
    /// vacant or the id carries an earlier generation.
    fn slot_index(&self, id: u64) -> Option<usize> {
        let index = usize::try_from(id & SLOT_MASK).ok()?;
        (index < self.definitions.len()
            && self.occupied[index]
            && self.generations[index] == (id >> 32))
        .then_some(index)
    }

    /// Iterates definitions in identifier order.
    pub fn iter(&self) -> impl Iterator<Item = (u64, &Highlight)> {
        self.definitions
            .iter()
            .enumerate()
            .filter(|(index, _)| self.occupied.get(*index).copied() == Some(true))
            .map(|(index, highlight)| (self.slot_id(index), highlight))
    }

    /// Iterates named group bindings in stable name order.
    pub fn groups(&self) -> impl Iterator<Item = (&OxStr, u64)> {
        self.groups.iter().map(|(name, id)| (name, *id))
    }

    /// Emits every current definition, including the default group.
    #[must_use]
    pub fn definitions(&self) -> Vec<HlEvent> {
        self.iter()
            .map(|(id, highlight)| define_event(id, highlight))
            .collect()
    }

    /// Drops one name binding's reference to `id`; when the last binding
    /// leaves a non-default slot, vacates it for `intern` to reuse. The
    /// reuse mints a fresh generation of the slot's id, so a grid cell
    /// still holding the vacated id can never alias another group's
    /// colors — it resolves to nothing until its next write, and the
    /// numeric id change makes the emit diff repaint the cell.
    fn release_binding(&mut self, id: u64) {
        if id == 0 {
            return;
        }
        let Ok(index) = usize::try_from(id & SLOT_MASK) else {
            return;
        };
        let Some(count) = self.refcounts.get_mut(index) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count > 0 {
            return;
        }
        if self.ids.get(&self.definitions[index]) == Some(&id) {
            self.ids.remove(&self.definitions[index]);
        }
        self.occupied[index] = false;
        self.free.push(index as u64);
    }

    /// Defines a named group, interning the highlight and binding the name.
    ///
    /// # Errors
    ///
    /// Returns [`HlError::IdExhausted`] when no identifier remains in the
    /// signed range supported by the protocol.
    pub fn define_group(
        &mut self,
        name: impl Into<OxStr>,
        highlight: Highlight,
    ) -> Result<u64, HlError> {
        let (id, _) = self.intern(highlight)?;
        self.set_group(name, id)?;
        Ok(id)
    }

    /// Looks up a group id by name.
    #[must_use]
    pub fn group_id(&self, name: &OxStr) -> Option<u64> {
        self.groups.get(name).copied()
    }

    /// Validates a highlight group name per upstream `syn_add_group` and
    /// returns its stable id, allocating one when the name is new.
    ///
    /// # Errors
    ///
    /// Returns [`HlError::GroupNameTooLong`], [`HlError::UnprintableGroupName`]
    /// or [`HlError::InvalidGroupName`] for invalid names, and
    /// [`HlError::TooManyGroups`] when the group table is exhausted.
    pub fn check_group(&mut self, name: &OxStr) -> Result<u64, HlError> {
        let bytes = name.as_bytes();
        if bytes.len() > MAX_GROUP_NAME_LEN {
            return Err(HlError::GroupNameTooLong);
        }
        if bytes.is_empty() {
            return Err(HlError::InvalidGroupName);
        }
        for &byte in bytes {
            if byte < 0x21 || byte == 0x7f {
                return Err(HlError::UnprintableGroupName);
            }
            if !byte.is_ascii_alphanumeric() && !matches!(byte, b'_' | b'.' | b'@' | b'-') {
                return Err(HlError::InvalidGroupName);
            }
        }
        if let Some(id) = self.group_ids.get(name) {
            return Ok(*id);
        }
        if self.next_group_id > MAX_GROUP_ID {
            return Err(HlError::TooManyGroups);
        }
        let id = self.next_group_id;
        self.next_group_id += 1;
        self.group_ids.insert(name.clone(), id);
        Ok(id)
    }

    /// Looks up a group id by name in the registry without allocating.
    #[must_use]
    pub fn group_by_name(&self, name: &OxStr) -> Option<u64> {
        self.group_ids.get(name).copied()
    }

    /// Looks up a group name by id in the registry.
    #[must_use]
    pub fn group_name(&self, id: u64) -> Option<&OxStr> {
        self.group_ids
            .iter()
            .find_map(|(name, candidate)| (*candidate == id).then_some(name))
    }

    /// Returns the number of allocated group ids.
    #[must_use]
    pub fn group_count(&self) -> u64 {
        self.next_group_id - 1
    }

    /// Returns the canonical definition explicitly set for a group id.
    #[must_use]
    pub fn group_def(&self, id: u64) -> Option<&HlDef> {
        self.group_defs.get(&id)
    }

    /// Stores the canonical definition for a group id, keeping the
    /// reverse link index in sync: the group's name leaves the index
    /// slot of its previous target and joins its new target's slot.
    pub fn set_group_def(&mut self, id: u64, name: &OxStr, definition: HlDef) {
        if let Some(old) = self.group_defs.get(&id)
            && let Some(target) = old.link
        {
            let map = if old.link_global {
                &mut self.global_dependents
            } else {
                &mut self.local_dependents
            };
            if let Some(linkers) = map.get_mut(&target) {
                linkers.remove(name);
                if linkers.is_empty() {
                    map.remove(&target);
                }
            }
        }
        if let Some(target) = definition.link {
            let map = if definition.link_global {
                &mut self.global_dependents
            } else {
                &mut self.local_dependents
            };
            map.entry(target).or_default().insert(name.clone());
        }
        self.group_defs.insert(id, definition);
    }

    /// Group names whose definitions link to `target`; `global` selects
    /// `link_global` links (resolved against namespace 0) from local
    /// links resolved in this namespace.
    pub fn link_dependents(&self, target: u64, global: bool) -> impl Iterator<Item = &OxStr> {
        let map = if global {
            &self.global_dependents
        } else {
            &self.local_dependents
        };
        map.get(&target).into_iter().flatten()
    }

    /// Iterates explicitly-set group definitions in name order.
    pub fn iter_group_defs(&self) -> impl Iterator<Item = (&OxStr, u64, &HlDef)> {
        self.group_ids.iter().filter_map(|(name, id)| {
            self.group_defs
                .get(id)
                .map(|definition| (name, *id, definition))
        })
    }

    /// Interns the result of stacking the overlay over the base highlight.
    ///
    /// Colors explicitly supplied by the later layer replace earlier colors;
    /// style flags accumulate, matching Neovim's range-highlight composition.
    ///
    /// # Errors
    ///
    /// Returns [`HlError::UnknownId`] when either identifier does not identify
    /// an existing highlight, or [`HlError::IdExhausted`] when the composite
    /// cannot be assigned a protocol identifier.
    pub fn combine(
        &mut self,
        base_id: u64,
        overlay_id: u64,
    ) -> Result<(u64, Option<HlEvent>), HlError> {
        let base = self
            .get(base_id)
            .ok_or(HlError::UnknownId(base_id))?
            .clone();
        let overlay = self
            .get(overlay_id)
            .ok_or(HlError::UnknownId(overlay_id))?
            .clone();
        let mut combined = base;
        combine_attrs(&mut combined.rgb, &overlay.rgb);
        combine_attrs(&mut combined.cterm, &overlay.cterm);
        combined.default_flag |= overlay.default_flag;
        if !overlay.info.is_empty() {
            combined.info = overlay.info;
        }
        self.intern(combined)
    }

    /// Interns the blend-mode composite of `overlay_id` layered over `base_id`.
    ///
    /// # Errors
    ///
    /// Returns [`HlError::UnknownId`] when either identifier does not identify
    /// an existing highlight, or [`HlError::IdExhausted`] when the composite
    /// cannot be assigned a protocol identifier.
    pub fn blend(
        &mut self,
        base_id: u64,
        overlay_id: u64,
    ) -> Result<(u64, Option<HlEvent>), HlError> {
        let base = self
            .get(base_id)
            .ok_or(HlError::UnknownId(base_id))?
            .clone();
        let overlay = self
            .get(overlay_id)
            .ok_or(HlError::UnknownId(overlay_id))?
            .clone();
        let amount = overlay.rgb.blend.unwrap_or(0).min(100);
        let (overlay_foreground, base_foreground) = (overlay.rgb.foreground, base.rgb.foreground);
        let (overlay_background, base_background) = (overlay.rgb.background, base.rgb.background);
        let (overlay_special, base_special) = (overlay.rgb.special, base.rgb.special);
        let mut mixed = base;
        combine_attrs(&mut mixed.rgb, &overlay.rgb);
        combine_attrs(&mut mixed.cterm, &overlay.cterm);
        if let (Some(over), Some(under)) = (overlay_foreground, base_foreground) {
            mixed.rgb.foreground = Some(premix_color(over, under, amount));
        }
        if let (Some(over), Some(under)) = (overlay_background, base_background) {
            mixed.rgb.background = Some(premix_color(over, under, amount));
        }
        if let (Some(over), Some(under)) = (overlay_special, base_special) {
            mixed.rgb.special = Some(premix_color(over, under, amount));
        }
        mixed.rgb.blend = None;
        mixed.default_flag |= overlay.default_flag;
        if !overlay.info.is_empty() {
            mixed.info = overlay.info;
        }
        self.intern(mixed)
    }

    /// Interns a winblend-premixed variant of `foreground_id` over `background_id`.
    ///
    /// # Errors
    ///
    /// Returns [`HlError::UnknownId`] when either identifier does not identify
    /// an existing highlight, or [`HlError::IdExhausted`] when the premixed
    /// highlight cannot be assigned a protocol identifier.
    pub fn premix(
        &mut self,
        foreground_id: u64,
        background_id: u64,
        blend: u8,
    ) -> Result<(u64, Option<HlEvent>), HlError> {
        let foreground = self
            .get(foreground_id)
            .ok_or(HlError::UnknownId(foreground_id))?
            .clone();
        let background = self
            .get(background_id)
            .ok_or(HlError::UnknownId(background_id))?
            .clone();
        let mut mixed = foreground;
        let amount = blend.min(100);
        mixed.rgb.foreground =
            mix_optional(mixed.rgb.foreground, background.rgb.foreground, amount);
        mixed.rgb.background =
            mix_optional(mixed.rgb.background, background.rgb.background, amount);
        mixed.rgb.special = mix_optional(mixed.rgb.special, background.rgb.special, amount);
        mixed.rgb.blend = None;
        self.intern(mixed)
    }
}

fn combine_attrs(base: &mut HlAttrs, overlay: &HlAttrs) {
    if overlay.foreground.is_some() {
        base.foreground = overlay.foreground;
    }
    if overlay.background.is_some() {
        base.background = overlay.background;
    }
    if overlay.special.is_some() {
        base.special = overlay.special;
    }
    base.bold |= overlay.bold;
    base.italic |= overlay.italic;
    base.underline |= overlay.underline;
    base.undercurl |= overlay.undercurl;
    base.underdouble |= overlay.underdouble;
    base.underdotted |= overlay.underdotted;
    base.underdashed |= overlay.underdashed;
    base.strikethrough |= overlay.strikethrough;
    base.reverse |= overlay.reverse;
    base.standout |= overlay.standout;
    base.altfont |= overlay.altfont;
    base.dim |= overlay.dim;
    base.blink |= overlay.blink;
    base.conceal |= overlay.conceal;
    base.overline |= overlay.overline;
    base.nocombine |= overlay.nocombine;
    base.fg_indexed |= overlay.fg_indexed;
    base.bg_indexed |= overlay.bg_indexed;
    if overlay.blend.is_some() {
        base.blend = overlay.blend;
    }
    if overlay.url.is_some() {
        base.url.clone_from(&overlay.url);
    }
}

/// Premixes a foreground RGB color over a background with Neovim-style percentage rounding.
#[must_use]
pub fn premix_color(foreground: u32, background: u32, blend: u8) -> u32 {
    let blend = u32::from(blend.min(100));
    let opaque = 100 - blend;
    let channel = |shift: u32| {
        let fg = (foreground >> shift) & 0xff;
        let bg = (background >> shift) & 0xff;
        ((fg * opaque + bg * blend + 50) / 100) << shift
    };
    channel(16) | channel(8) | channel(0)
}

fn mix_optional(foreground: Option<u32>, background: Option<u32>, blend: u8) -> Option<u32> {
    match (foreground, background) {
        (Some(foreground), Some(background)) => Some(premix_color(foreground, background, blend)),
        (foreground, _) => foreground,
    }
}

fn define_event(id: u64, highlight: &Highlight) -> HlEvent {
    HlEvent {
        name: "hl_attr_define",
        args: vec![
            Object::Integer(i64::try_from(id).unwrap_or(i64::MAX)),
            highlight.rgb.to_object(),
            highlight.cterm.to_object(),
            Object::Array(highlight.info.iter().map(HlInfo::to_object).collect()),
        ],
    }
}

fn push_color(entries: &mut Vec<(OxStr, Object)>, name: &'static str, color: Option<u32>) {
    if let Some(color) = color {
        entries.push((OxStr::from(name), Object::Integer(i64::from(color))));
    }
}

fn push_flag(entries: &mut Vec<(OxStr, Object)>, name: &'static str, enabled: bool) {
    if enabled {
        entries.push((OxStr::from(name), Object::Boolean(true)));
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn fg_highlight(fg: u32) -> Highlight {
        Highlight {
            rgb: HlAttrs {
                foreground: Some(fg),
                ..HlAttrs::default()
            },
            ..Highlight::default()
        }
    }

    #[test]
    fn group_boundary_rejects_past_max_hl_id() {
        let mut state = HlState::new();
        // Seed the counter so the next allocation would exceed MAX_HL_ID.
        state.next_group_id = MAX_GROUP_ID;
        assert_eq!(
            state.check_group(&OxStr::from("LastGroup")),
            Ok(MAX_GROUP_ID)
        );
        assert_eq!(
            state.check_group(&OxStr::from("Over")),
            Err(HlError::TooManyGroups)
        );
    }

    #[test]
    fn group_boundary_allows_exactly_max_hl_id() {
        let mut state = HlState::new();
        state.next_group_id = MAX_GROUP_ID - 1;
        assert_eq!(
            state.check_group(&OxStr::from("Penultimate")),
            Ok(MAX_GROUP_ID - 1)
        );
        assert_eq!(state.check_group(&OxStr::from("Last")), Ok(MAX_GROUP_ID));
        assert_eq!(
            state.check_group(&OxStr::from("Over")),
            Err(HlError::TooManyGroups)
        );
    }

    #[test]
    fn rebinding_a_group_vacates_its_old_attr_slot_for_reuse() {
        let mut state = HlState::new();
        let (first, _) = state.intern(fg_highlight(1)).unwrap();
        state.set_group("G", first).unwrap();
        let (second, _) = state.intern(fg_highlight(2)).unwrap();
        state.set_group("G", second).unwrap();
        // The slot `G` vacated reports nothing until interned again.
        assert_eq!(state.get(first), None);
        let (reused, event) = state.intern(fg_highlight(3)).unwrap();
        // Reuse mints a new generation of the same slot: numerically a
        // fresh id, so cells holding the vacated id diff as changed.
        assert_ne!(reused, first);
        assert_eq!(reused & SLOT_MASK, first & SLOT_MASK);
        assert!(event.is_some());
        assert_eq!(
            state.get(reused).map(|highlight| highlight.rgb.foreground),
            Some(Some(3))
        );
    }

    #[test]
    fn shared_attr_survives_partial_rebind() {
        let mut state = HlState::new();
        let (id, _) = state.intern(fg_highlight(1)).unwrap();
        state.set_group("A", id).unwrap();
        state.set_group("B", id).unwrap();
        let (other, _) = state.intern(fg_highlight(2)).unwrap();
        state.set_group("A", other).unwrap();
        // `B` still binds the slot, so it is not a reuse candidate.
        assert!(state.get(id).is_some());
        let (fresh, _) = state.intern(fg_highlight(3)).unwrap();
        assert_ne!(fresh, id);
    }

    #[test]
    fn default_slot_is_never_vacated() {
        let mut state = HlState::new();
        state.set_group("G", 0).unwrap();
        let (other, _) = state.intern(fg_highlight(2)).unwrap();
        state.set_group("G", other).unwrap();
        assert!(state.get(0).is_some());
    }

    #[test]
    fn vacated_slot_rejects_bind_and_redefine() {
        let mut state = HlState::new();
        let (id, _) = state.intern(fg_highlight(1)).unwrap();
        state.set_group("G", id).unwrap();
        let (other, _) = state.intern(fg_highlight(2)).unwrap();
        state.set_group("G", other).unwrap();
        assert_eq!(state.set_group("H", id), Err(HlError::UnknownId(id)));
        assert_eq!(
            state.redefine(id, fg_highlight(3)),
            Err(HlError::UnknownId(id))
        );
    }

    #[test]
    fn rebinding_to_the_same_id_keeps_the_slot_bound() {
        let mut state = HlState::new();
        let (id, _) = state.intern(fg_highlight(1)).unwrap();
        state.set_group("G", id).unwrap();
        state.set_group("G", id).unwrap();
        let (other, _) = state.intern(fg_highlight(2)).unwrap();
        assert_ne!(other, id);
        assert!(state.get(id).is_some());
    }

    #[test]
    fn vacating_a_slot_keeps_other_slots_dedup_entry() {
        // `redefine`'s `or_insert` can leave `ids[h]` pointing at a
        // different live slot; vacating must not delete that entry or
        // the next intern of `h` duplicates a live definition.
        let mut state = HlState::new();
        let (victim, _) = state.intern(fg_highlight(1)).unwrap();
        let (other, _) = state.intern(fg_highlight(2)).unwrap();
        // `other` now holds `victim`'s attributes; `ids[hl1]` still
        // maps to `victim` because `or_insert` keeps the first winner.
        state.redefine(other, fg_highlight(1)).unwrap();
        state.set_group("G", other).unwrap();
        let (fresh, _) = state.intern(fg_highlight(3)).unwrap();
        state.set_group("G", fresh).unwrap();
        // Vacating `other` must not remove `ids[hl1] -> victim`.
        let (deduped, _) = state.intern(fg_highlight(1)).unwrap();
        assert_eq!(deduped, victim);
    }

    #[test]
    fn stale_generation_ids_resolve_to_nothing() {
        let mut state = HlState::new();
        let (id, _) = state.intern(fg_highlight(1)).unwrap();
        state.set_group("G", id).unwrap();
        let (next, _) = state.intern(fg_highlight(2)).unwrap();
        state.set_group("G", next).unwrap();
        // The vacated id's generation is now stale: nothing may bind
        // it or read attributes through it, so a cell still holding it
        // can never alias the slot's later contents.
        assert_eq!(state.get(id), None);
        assert_eq!(state.set_group("H", id), Err(HlError::UnknownId(id)));
    }

    #[test]
    fn link_dependents_track_retargets_and_scope() {
        let mut state = HlState::new();
        let a = state.check_group(&OxStr::from("A")).unwrap();
        let t = state.check_group(&OxStr::from("T")).unwrap();
        let u = state.check_group(&OxStr::from("U")).unwrap();
        state.set_group_def(
            a,
            &OxStr::from("A"),
            HlDef {
                link: Some(t),
                ..HlDef::default()
            },
        );
        assert_eq!(state.link_dependents(t, false).count(), 1);
        // Retargeting removes the stale index entry.
        state.set_group_def(
            a,
            &OxStr::from("A"),
            HlDef {
                link: Some(u),
                ..HlDef::default()
            },
        );
        assert_eq!(state.link_dependents(t, false).count(), 0);
        assert_eq!(state.link_dependents(u, false).count(), 1);
        // `link_global` dependents index separately from local links.
        state.set_group_def(
            a,
            &OxStr::from("A"),
            HlDef {
                link: Some(t),
                link_global: true,
                ..HlDef::default()
            },
        );
        assert_eq!(state.link_dependents(t, false).count(), 0);
        assert_eq!(state.link_dependents(t, true).count(), 1);
        // Clearing the link drops the index entry entirely.
        state.set_group_def(a, &OxStr::from("A"), HlDef::default());
        assert_eq!(state.link_dependents(t, true).count(), 0);
    }
}
