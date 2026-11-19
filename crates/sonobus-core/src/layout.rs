//! The `ChannelLayouts` tree a SonoBus peer sends to describe its outgoing channel groups, both in
//! `/sb/clayinfo` and as the AOO source user format.
//!
//! Upstream: built by `getSendUserFormatLayoutTree` (Source/SonobusPluginProcessor.cpp:6359-6403),
//! one child per group from `ChannelGroupParams::getChannelLayoutValueTree`
//! (Source/ChannelGroup.cpp:730-749), applied by `applyLayoutFormatToPeer` (P.cpp:6493-6512) via
//! `setFromChannelLayoutValueTree` (ChannelGroup.cpp:751-769).
//!
//! ```text
//! ChannelLayouts
//!   Layout { chanstart: int, numchan: int, name: string, span1: double, span2: double }
//!     StereoPanners
//!       Panner { pan: double }   x min(numchan, 64)
//! ```
//!
//! Stock receivers check neither the root's nor the children's type names, and every property is
//! optional (missing keeps the previous value); we do the same.

use crate::valuetree::{ValueTree, Var};

/// `MAX_CHANNELS` (Source/ChannelGroup.h:18): panners per group.
pub const MAX_CHANNELS: usize = 64;
/// `MAX_CHANGROUPS` (Source/SonobusPluginProcessor.h:33): groups a receiver applies.
pub const MAX_CHANGROUPS: usize = 64;
/// Stock 1.7.2 receivers read the AOO user format into a buffer of this size
/// (P.cpp:3771-3773); a larger blob is ignored there, leaving only `/sb/clayinfo`.
pub const USER_FORMAT_MAX_LEN: usize = 1024;

const ROOT: &str = "ChannelLayouts";
const LAYOUT: &str = "Layout";
const PANNERS: &str = "StereoPanners";
const PANNER: &str = "Panner";

/// Layout part of upstream `ChannelGroupParams`; `Default` matches its field initialisers
/// (Source/ChannelGroup.h:38-52).
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelLayout {
    /// `chanstart`: first channel of this group in the sent stream.
    pub chan_start: i32,
    /// `numchan`.
    pub num_channels: i32,
    pub name: String,
    /// `span1`, `span2`: pan of each side when the group is stereo.
    pub pan_stereo: [f32; 2],
    /// `Panner/pan` per channel, used when the group is not stereo. Only the first
    /// `num_channels` are sent.
    pub pan: [f32; MAX_CHANNELS],
}

impl Default for ChannelLayout {
    fn default() -> Self {
        ChannelLayout {
            chan_start: 0,
            num_channels: 1,
            name: String::new(),
            pan_stereo: [-1.0, 1.0],
            pan: [0.0; MAX_CHANNELS],
        }
    }
}

impl ChannelLayout {
    pub fn to_tree(&self) -> ValueTree {
        let mut t = ValueTree::new(LAYOUT);
        t.set("chanstart", Var::Int(self.chan_start));
        t.set("numchan", Var::Int(self.num_channels));
        t.set("name", Var::String(self.name.clone()));
        // C++ floats are promoted to double in a var.
        t.set("span1", Var::Double(self.pan_stereo[0].into()));
        t.set("span2", Var::Double(self.pan_stereo[1].into()));
        let mut panners = ValueTree::new(PANNERS);
        let n = self.num_channels.clamp(0, MAX_CHANNELS as i32) as usize;
        for &p in &self.pan[..n] {
            let mut panner = ValueTree::new(PANNER);
            panner.set("pan", Var::Double(p.into()));
            panners.children.push(panner);
        }
        t.children.push(panners);
        t
    }

    /// Overrides fields present in `t`, keeping the rest (upstream `setFromChannelLayoutValueTree`).
    pub fn update_from_tree(&mut self, t: &ValueTree) {
        if let Some(v) = t.get("chanstart") {
            self.chan_start = v.to_i32();
        }
        if let Some(v) = t.get("numchan") {
            self.num_channels = v.to_i32();
        }
        if let Some(v) = t.get("name") {
            self.name = v.to_string_lossy();
        }
        if let Some(v) = t.get("span1") {
            self.pan_stereo[0] = v.to_f64() as f32;
        }
        if let Some(v) = t.get("span2") {
            self.pan_stereo[1] = v.to_f64() as f32;
        }
        if let Some(panners) = t.child_with_name(PANNERS) {
            for (pan, panner) in self.pan.iter_mut().zip(&panners.children) {
                if let Some(v) = panner.get("pan") {
                    *pan = v.to_f64() as f32;
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChannelLayouts {
    pub groups: Vec<ChannelLayout>,
}

impl ChannelLayouts {
    pub fn to_tree(&self) -> ValueTree {
        let mut t = ValueTree::new(ROOT);
        t.children = self.groups.iter().map(ChannelLayout::to_tree).collect();
        t
    }

    /// `None` for an invalid tree (stock receivers drop those). Groups start from
    /// [`ChannelLayout::default`]; beyond [`MAX_CHANGROUPS`] they are ignored, as upstream.
    /// Upstream starts each group from the receiver's current params instead; use
    /// [`ChannelLayout::update_from_tree`] on the children for that.
    pub fn from_tree(t: &ValueTree) -> Option<Self> {
        t.is_valid().then(|| ChannelLayouts {
            groups: t
                .children
                .iter()
                .take(MAX_CHANGROUPS)
                .map(|c| {
                    let mut g = ChannelLayout::default();
                    g.update_from_tree(c);
                    g
                })
                .collect(),
        })
    }
}
