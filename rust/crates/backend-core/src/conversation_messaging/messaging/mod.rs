//! Messaging application boundary: the real create-message contract.
//!
//! Driver-free and transport-free. Persistence and wire projection live in
//! sibling crates; both project from the facts defined here, and neither's
//! vocabulary — statement text, status codes, wire codes, operator instructions —
//! appears in this module.
//!
//! One send reads left to right through the modules below:
//!
//! - `principal` — who a principal is: the two kinds and the stored name grammar,
//!   one vocabulary for every consumer that asks.
//! - `identity` — the identities and ordinals this system mints and publishes.
//! - `addressing` — how a body produces references, and who it therefore addressed.
//! - `command` — a request with every invariant satisfied, prepared before any
//!   transaction opens.
//! - `authority` — what the write is authorized against, read inside the atomic
//!   unit.
//! - `fact` — the single business fact of a committed send, and the typed views
//!   each delivery path reads it through.
//! - `author` — who a message reports as its author, as one discriminated value
//!   (active / deleted / unresolved) every reader shares.
//!
//! **Those seven are PRIVATE, and that is the point.** Where a type happens to live
//! is this module's business to change, so nothing outside can import
//! `messaging::command::…` and become coupled to a layout decision. Everything they
//! define is reachable at its stable `messaging::X` name through the re-exports at
//! the bottom of this file — the same names these types have always had.
//!
//! The gate on that, one case per module, because a private module is only a
//! boundary while it stays unreachable:
//!
//! ```compile_fail
//! use silverhand_backend_core::conversation_messaging::messaging::principal::PrincipalKind;
//! ```
//! ```compile_fail
//! use silverhand_backend_core::conversation_messaging::messaging::identity::MessageId;
//! ```
//! ```compile_fail
//! use silverhand_backend_core::conversation_messaging::messaging::addressing::AddressedPrincipals;
//! ```
//! ```compile_fail
//! use silverhand_backend_core::conversation_messaging::messaging::command::SendMessage;
//! ```
//! ```compile_fail
//! use silverhand_backend_core::conversation_messaging::messaging::authority::assert_post_authority;
//! ```
//! ```compile_fail
//! use silverhand_backend_core::conversation_messaging::messaging::fact::MessageCreatedFact;
//! ```
//! ```compile_fail
//! use silverhand_backend_core::conversation_messaging::messaging::author::MessageAuthor;
//! ```
//!
//! Public alongside them sit the concerns that own a rule end to end and are
//! consumed by name from outside: [`attachments`], [`environment`], [`freshness`],
//! [`mention_cc`] and [`post_commit`]. A refusal lives with the authority that
//! answers it, which is why `AttachmentRefusal` is in `attachments` and
//! `StaleRefusal` is in `freshness` rather than here.

mod addressing;
mod author;
mod authority;
mod command;
mod direct_pair;
mod fact;
mod identity;
mod pre_send;
mod principal;

pub(crate) use crate::foundation::identity::{AgentId, ConversationId, UserId, WorkspaceId};

pub mod agent_onboarding;
pub mod alert;
pub mod attachments;
pub mod collaboration;
pub mod conversation_title;
pub mod environment;
pub mod freshness;
pub mod markdown;
pub mod mention_cc;
pub mod post_commit;
pub mod schedule;
pub mod task;
pub mod unread;

pub use addressing::{AddressedPrincipals, MessageReference, ReferencePositions};
pub use attachments::AttachmentRefusal;
pub use author::{
    ActiveAgent, ActiveAuthor, ActiveHuman, AvatarRevision, DeletedAuthor, MessageAuthor,
    PrincipalIdentity, UnresolvedAuthor,
};
pub use authority::{
    ActorRefusal, ConversationEndpoint, ConversationFacts, ConversationRefusal,
    DirectPairEndpoints, EndpointPrincipalReads, PostAuthorityReads, ReplyTargetRefusal,
    ThreadRootRefusal, assert_post_authority, direct_pair_is_read_only_for_survivor,
};
pub use command::{
    ClientMessageId, ContentTooLong, DirectLineAuthority, DirectLineGrants,
    MAX_MESSAGE_CONTENT_BYTES, MessageContent, SendActor, SendDestination, SendMessage,
    SendMessageDraft, SendMessageError, SendMessageUnitOfWork,
};
pub use direct_pair::{
    DirectPairMint, DirectPairMintIdentityMismatch, DirectPairMinted, DirectPairOpenedFact,
    DirectPairOpenedPort, DirectPairParticipants, OpenDirectPairError, SameDirectPairParticipant,
    open_direct_pair,
};
pub use environment::{EnvironmentUnavailable, SendClock, SendEntropy};
pub use fact::{
    CommittedMessageView, CreatedMessage, FromKind, MainCommittedSend, MessageCreatedFact,
    ReplyCommittedSend, SendMessageOutcome, THREAD_PREVIEW_MAX_UTF16, ThreadFollowerSource,
    ThreadListEntry, ThreadReplyFact, thread_root_preview,
};
pub use freshness::StaleRefusal;
pub use identity::{
    AttachmentId, EmptyIdentifier, InvalidSeq, MessageId, MessageSeq, NotificationId, ThreadRootId,
    mint_agent_id, mint_conversation_id, mint_workspace_id,
};
pub use pre_send::{
    AddressRefusal, CapabilityDenied, ChannelAccess, ChannelAccessRefusal, DeliveryIntent,
    DirectPairAllocation, ResolvedStream, SendCapability, TargetAddress,
    TargetAddressOperatorReason, TargetAddressRefusalPort, TargetAddressRefusalReport,
};
pub use principal::{InvalidHandle, ParticipantHandle, PrincipalKind};
