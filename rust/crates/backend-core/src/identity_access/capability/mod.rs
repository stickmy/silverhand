//! Who may see or act on a resource — an agent, or the computer it sits on.
//!
//! Every module here is a pure decision over facts the caller has ALREADY read:
//! nothing in this namespace holds a connection, issues a read, or owns a
//! repository. That is what lets the same rule serve a face that reads its facts
//! before an independent write and a face that must read them under the write
//! lock it is about to commit in — the decision is wrapped by a unit of work,
//! never cut by one.
//!
//! Acquiring the facts can fail (a missing driver, an unreadable row). That
//! failure is the caller's `Err`, upstream of every function here, so folding a
//! read failure into a refusal is not expressible rather than merely discouraged.
//!
//! ## The rule for what a verdict returns
//!
//! A rule whose verdict is only RENDERED returns [`decision::Decision`]. A rule
//! that GATES A WRITE does not: its verdict is its own type whose admitted arm
//! carries a witness with a private constructor and no public way to assemble one.
//! `Decision`'s `Allow` is a publicly constructible unit variant, so it can seal
//! nothing — an implementation could produce it from a comparison it re-stated
//! itself. The write boundary must consume a witness instead, so an implementation
//! that did not run the rule has nothing to return. That is the whole difference,
//! and it is why the two shapes are not one.
//!
//! A witness therefore names the subject it judged, not only the answer. Composing
//! several of them is where that matters: two individually well-formed witnesses can
//! be about two different rows, so every seal that composes them checks that they
//! name one subject. A mismatch there is an internal contract failure — a defect in
//! the code that read the facts, which no user can cause or act on — so it must
//! surface as its own failure class and never as an arm of a product refusal.
//! Folding it into "not yours" or "it moved" would invite a caller to retry a bug.

pub mod agent_audit;
pub mod agent_authority;
pub mod agent_view;
pub mod computer_access;
pub mod decision;
pub mod read_tier;
