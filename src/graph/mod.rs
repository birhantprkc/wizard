//! The graph explorer: the Obsidian-style view over the mesh, minus the
//! drawing.
//!
//! This is S1.1 of the v2 plan, and it is deliberately toolkit-independent.
//! Nothing here links a GUI crate, opens a window or knows what a pixel is,
//! because the two hard parts of an explorer are not the rendering:
//!
//! - [`model`] turns [`crate::mesh`]'s cached peer state into something
//!   drawable, at a clock the caller supplies. It decides liveness once,
//!   honestly, folding the recorded trust decision together with what the
//!   store last observed, so a renderer cannot draw a blocked peer green by
//!   forgetting a match arm. The plan's line is that "a graph that is
//!   beautiful and lies about who is online is worse than a plain one that
//!   does not", and that is a property of this layer, not of the canvas.
//! - [`layout`] is the force model as pure arithmetic: seeded, deterministic,
//!   snapshot-tested, with pinning and a measured per-step cost. A layout
//!   nobody can reproduce is a layout nobody can debug.
//!
//! ```text
//! PeerStore ──build──▶ MeshGraph ──seed/step──▶ Layout ──hit_test──▶ NodeKey
//!                          │                                           │
//!                          └──────────────── inspect ◀─────────────────┘
//! ```
//!
//! # Peer text is attacker-controlled
//!
//! A peer picks its own name, and that name ends up as a label on somebody
//! else's screen. [`crate::mesh::PeerText`] sanitises at the wire boundary;
//! this layer bounds again anyway ([`model::MAX_LABEL_COLUMNS`], in display
//! columns *and* characters, with invisible formatting characters neutralised
//! a second time) and gives every node a discriminator taken from its key
//! fingerprint, so two peers that call themselves the same thing are told
//! apart by something a peer cannot choose. Trust state is a plain field on
//! every node rather than something to derive, because the plan's acceptance
//! bar is that it is unambiguous from the model alone.
//!
//! # What is not here
//!
//! Animation and time scrubbing over delegation history: the plan puts both in
//! 2.1, behind a static graph with a good inspector and correct staleness. The
//! model is shaped to take them (a [`model::MeshGraph`] is a snapshot at an
//! instant, and building one for a past instant is the same call with a
//! different clock) and nothing here pretends they exist yet.

pub mod layout;
pub mod model;

pub use layout::{Layout, LayoutParams, Point, Rect, seed_position, step_positions};
pub use model::{
    CapabilityRef, DisplayName, GraphNode, Inspection, Link, Liveness, MeshGraph, NodeKey, NodeKind,
};
