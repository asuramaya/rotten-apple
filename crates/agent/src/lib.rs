//! rotten-apple agent — per-node reconciler.
//!
//! An *agent* is the resident process on a rotten-apple node that:
//!   1. Holds the local "wanted state" (what manifests this node
//!      should be running, declared by the current controller)
//!   2. Polls "truth" from libxl (what's actually running)
//!   3. Reconciles wanted ↔ truth into an action plan
//!   4. Applies the plan via the backend (libxl on Xen hosts)
//!   5. Refuses controller events that violate manifest lease policy
//!
//! This crate is the agent's *logic*. It is pure and side-effect-free
//! — every function maps inputs (state + events) to outputs (new
//! state + actions). The actuator that turns actions into libxl
//! calls lives in `orchestratord` so the agent stays unit-testable
//! on hosts without Xen running.
//!
//! Module map:
//!   - [`wanted`]    — desired-state types + event application
//!   - [`truth`]     — observed-state types (decoupled from libxl)
//!   - [`reconcile`] — wanted × truth → [`ReconcilePlan`]
//!   - [`refuse`]    — manifest lease-policy refusal layer

pub mod reconcile;
pub mod refuse;
pub mod truth;
pub mod wanted;

pub use reconcile::{reconcile, ReconcileAction, ReconcilePlan};
pub use refuse::{may_apply, RefusalReason};
pub use truth::{DomainState, Truth, TruthEntry};
pub use wanted::{ApplyError, WantedEntry, WantedState};
