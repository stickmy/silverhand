//! The task lifecycle SYSTEM-EVENT vocabulary — the one place that turns a task
//! mutation into the `task_*` system message a conversation carries.
//!
//! Every task write (create/claim/unclaim/status/edit) that changes durable state
//! appends ONE system message so the conversation, and every member's board, learns
//! the task moved. This module owns BOTH halves of that message as a unit — the
//! human-readable `content` line and the structured `metadata` JSON a renderer draws
//! an actor pill / task chip from without parsing the content — so the two never
//! drift and no adapter re-hand-rolls the JSON. It is the parity of the TS
//! `taskSystemEventMetadata` + the per-kind content templates.
//!
//! The names inside (`actor.name`, `assignee.name`) are DB-resolved facts the caller
//! reads before/inside its write and hands in as pure values; this module performs
//! no lookup — it is a serializer, not a reader.

use serde_json::{Map, Value, json};

use super::spec::{TaskId, TaskNumber, TaskStatus, TaskTitle};
use crate::foundation::identity::{AgentId, UserId};

/// The `subtype` a task mutation stamps: `task_${kind}`. It is the discriminant a
/// consumer switches on, distinct from the free-form content line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskEventKind {
    Create,
    Claim,
    Unclaim,
    Status,
    Edit,
}

/// The two deliberately different agent-name projections used by task events.
/// Human-readable content uses the stable agent handle (`agents.name`), while
/// structured metadata uses `display_name ?? name ?? id`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentEventNames {
    content_name: String,
    display_name: String,
}

impl AgentEventNames {
    pub fn new(content_name: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            content_name: content_name.into(),
            display_name: display_name.into(),
        }
    }

    pub fn content_name(&self) -> &str {
        &self.content_name
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }
}

impl TaskEventKind {
    /// The task lifecycle subtype carried by persisted and wire metadata.
    pub fn subtype(self) -> &'static str {
        match self {
            TaskEventKind::Create => "task_create",
            TaskEventKind::Claim => "task_claim",
            TaskEventKind::Unclaim => "task_unclaim",
            TaskEventKind::Status => "task_status",
            TaskEventKind::Edit => "task_edit",
        }
    }
}

/// The structured actor a task event carries so a renderer can draw the "by X" pill
/// without parsing the content. The id is a TYPED identity (`AgentId`/`UserId`), so a
/// `UserId` can never be placed in the agent arm; only at the JSON boundary is it
/// projected to a string. `name` is the DB-resolved display name the caller holds (an
/// agent's `display_name`/`name`, or the literal `"user"` — the TS `taskSystemActorFor`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskSystemActor {
    Agent { id: AgentId, names: AgentEventNames },
    User { id: UserId, name: String },
}

impl TaskSystemActor {
    /// The `actorLabel` the content line interpolates: `@<name>` for an agent, the
    /// literal `user` for a human (the TS `actorLabel`).
    fn label(&self) -> String {
        match self {
            TaskSystemActor::Agent { names, .. } => format!("@{}", names.content_name()),
            TaskSystemActor::User { .. } => "user".to_owned(),
        }
    }

    /// The `{kind,id,name}` JSON the metadata's `actor` field carries. Built by
    /// hand — the domain layer holds no serde derives; the wire shape is owned here.
    /// The typed id is projected to its string only here, at the boundary.
    pub fn to_json(&self) -> Value {
        match self {
            TaskSystemActor::Agent { id, names } => {
                json!({ "kind": "agent", "id": id.as_str(), "name": names.display_name() })
            }
            TaskSystemActor::User { id, name } => {
                json!({ "kind": "user", "id": id.as_str(), "name": name })
            }
        }
    }
}

/// The task's assignee as the event records it — the same `{type,id,name}` shape the
/// list/detail `TaskAssigneeDto` uses, or `null` for an unassigned task. The id is a
/// TYPED identity, projected to a string only at the JSON boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskEventAssignee {
    Agent { id: AgentId, names: AgentEventNames },
    User { id: UserId, name: String },
}

impl TaskEventAssignee {
    /// The `@name` an assignee carries, for the human-claim content line.
    fn label(&self) -> &str {
        match self {
            TaskEventAssignee::Agent { names, .. } => names.content_name(),
            TaskEventAssignee::User { name, .. } => name,
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            TaskEventAssignee::Agent { id, names } => {
                json!({ "type": "agent", "id": id.as_str(), "name": names.display_name() })
            }
            TaskEventAssignee::User { id, name } => {
                json!({ "type": "user", "id": id.as_str(), "name": name })
            }
        }
    }
}

/// The task facts an event serializes over — the post-write snapshot the caller read
/// under its own lock, plus the acting actor. The identity/number/title are TYPED
/// value objects (so a negative task_number or an un-normalized title cannot be
/// constructed in core); only at the JSON boundary are they projected to
/// string/integer. `status` is already the `TaskStatus` value object.
#[derive(Clone, Debug)]
pub struct TaskEventFacts {
    pub task_id: TaskId,
    pub task_number: TaskNumber,
    pub title: TaskTitle,
    pub status: TaskStatus,
    pub assignee: Option<TaskEventAssignee>,
    pub actor: TaskSystemActor,
}

/// A composed task system event: the two halves a `task_*` message carries, bound
/// together so a caller appends them as one and never re-derives either.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskEventMessage {
    pub content: String,
    /// The metadata JSON, serialized (the `messages.metadata` column is TEXT).
    pub metadata: String,
}

/// Serialize the uniform metadata object: `{ subtype, task_number, task_status,
/// task_id, task_title, task_assignee, actor, ...extra }`. `extra` merges last (e.g.
/// create's `cross_conversation`), matching the TS spread order. The typed value
/// objects are projected to string/integer here, at the boundary.
fn metadata_json(kind: TaskEventKind, facts: &TaskEventFacts, extra: &Map<String, Value>) -> Value {
    let mut object = Map::new();
    object.insert("subtype".to_owned(), json!(kind.subtype()));
    object.insert("task_number".to_owned(), json!(facts.task_number.get()));
    object.insert("task_status".to_owned(), json!(facts.status.as_str()));
    object.insert("task_id".to_owned(), json!(facts.task_id.as_str()));
    object.insert("task_title".to_owned(), json!(facts.title.as_str()));
    object.insert(
        "task_assignee".to_owned(),
        match &facts.assignee {
            Some(assignee) => assignee.to_json(),
            None => Value::Null,
        },
    );
    object.insert("actor".to_owned(), facts.actor.to_json());
    for (key, value) in extra {
        object.insert(key.clone(), value.clone());
    }
    Value::Object(object)
}

/// Compose an `edit` event. Content = `<actorLabel> edited task #N "<title>"`
/// (the post-change title), metadata the uniform object with the `task_edit`
/// subtype. Edit carries no `extra`.
pub fn compose_edit_event(facts: &TaskEventFacts) -> TaskEventMessage {
    let content = format!(
        "{} edited task #{} \"{}\"",
        facts.actor.label(),
        facts.task_number.get(),
        facts.title.as_str()
    );
    TaskEventMessage {
        content,
        metadata: metadata_json(TaskEventKind::Edit, facts, &Map::new()).to_string(),
    }
}

/// How a `create` was placed relative to the acting agent's own conversation,
/// selecting the content template (the TS `crossConversation ? … : claimByAgentId ?
/// … : …` fork). A human create is always [`CreatePlacement::Filed`] (no origin
/// anchor, no self-claim).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CreatePlacement {
    /// Filed into the acting conversation, unclaimed.
    Filed,
    /// Filed into the acting conversation AND self-claimed (`--claim`).
    Claimed,
    /// Filed from another conversation than the agent's own (a cross-conversation
    /// dispatch); records the `cross_conversation` metadata flag.
    CrossConversation,
}

/// Compose a `create` event. The content is one of three templates and the metadata
/// carries the `cross_conversation` flag only on the cross arm — the parity of the
/// TS create's `systemEvents.insert` content fork + the `crossConversation ?
/// {cross_conversation:true} : undefined` extra.
pub fn compose_create_event(
    facts: &TaskEventFacts,
    placement: CreatePlacement,
) -> TaskEventMessage {
    let label = facts.actor.label();
    let content = match placement {
        CreatePlacement::Filed => {
            format!(
                "{} created task #{} \"{}\"",
                label,
                facts.task_number.get(),
                facts.title.as_str()
            )
        }
        CreatePlacement::Claimed => format!(
            "{} created task #{} \"{}\" and claimed it",
            label,
            facts.task_number.get(),
            facts.title.as_str()
        ),
        CreatePlacement::CrossConversation => format!(
            "{} filed task #{} \"{}\" here from another conversation",
            label,
            facts.task_number.get(),
            facts.title.as_str()
        ),
    };
    let mut extra = Map::new();
    if placement == CreatePlacement::CrossConversation {
        extra.insert("cross_conversation".to_owned(), Value::Bool(true));
    }
    TaskEventMessage {
        content,
        metadata: metadata_json(TaskEventKind::Create, facts, &extra).to_string(),
    }
}

/// Compose a `status` event. Content is `task #N → <status> (by <who>)` — `(by
/// @<name>)` for an agent actor, `(by user)` for a human (the TS `updateStatusAgent`
/// / `updateStatusUser` templates). Metadata is the uniform object.
pub fn compose_status_event(facts: &TaskEventFacts) -> TaskEventMessage {
    let by = match &facts.actor {
        TaskSystemActor::Agent { names, .. } => format!("by @{}", names.content_name()),
        TaskSystemActor::User { .. } => "by user".to_owned(),
    };
    let content = format!(
        "task #{} → {} ({})",
        facts.task_number.get(),
        facts.status.as_str(),
        by
    );
    TaskEventMessage {
        content,
        metadata: metadata_json(TaskEventKind::Status, facts, &Map::new()).to_string(),
    }
}

/// Compose a `claim` event. An AGENT self-claim is `@<actor> claimed task #N`; a
/// HUMAN claim ASSIGNS a chosen agent, `task #N assigned to @<assignee> (by user)`
/// (the assignee's name is on the event's `task_assignee`, the fresh post-claim
/// owner). The actor's kind selects the template (the TS `claimTaskAgent` /
/// `claimTaskUser`).
pub fn compose_claim_event(facts: &TaskEventFacts) -> TaskEventMessage {
    let content = match &facts.actor {
        TaskSystemActor::Agent { names, .. } => {
            format!(
                "@{} claimed task #{}",
                names.content_name(),
                facts.task_number.get()
            )
        }
        TaskSystemActor::User { .. } => {
            let assignee = assignee_label(&facts.assignee);
            format!(
                "task #{} assigned to @{} (by user)",
                facts.task_number.get(),
                assignee
            )
        }
    };
    TaskEventMessage {
        content,
        metadata: metadata_json(TaskEventKind::Claim, facts, &Map::new()).to_string(),
    }
}

/// Compose the `task_claim` event of an assignment, which names the assignee. A
/// self-claim uses [`compose_claim_event`].
pub fn compose_assign_event(facts: &TaskEventFacts) -> TaskEventMessage {
    let assignee = assignee_label(&facts.assignee);
    let content = match &facts.actor {
        TaskSystemActor::Agent { names, .. } => format!(
            "@{} assigned task #{} to @{}",
            names.content_name(),
            facts.task_number.get(),
            assignee
        ),
        TaskSystemActor::User { .. } => format!(
            "task #{} assigned to @{} (by user)",
            facts.task_number.get(),
            assignee
        ),
    };
    TaskEventMessage {
        content,
        metadata: metadata_json(TaskEventKind::Claim, facts, &Map::new()).to_string(),
    }
}

/// Compose an `unclaim` event. An AGENT unclaim is `task #N unclaimed (was
/// @<prev>)` where `prev` is the assignee the clear removed; a HUMAN unclaim is
/// `task #N unassigned (by user)`. `prior_assignee_name` carries the removed agent's
/// name for the agent arm (the post-clear `task_assignee` is null, so the name
/// cannot be read off `facts`); it is unused on the human arm.
pub fn compose_unclaim_event(
    facts: &TaskEventFacts,
    prior_assignee_name: &str,
) -> TaskEventMessage {
    let content = match &facts.actor {
        TaskSystemActor::Agent { .. } => {
            format!(
                "task #{} unclaimed (was @{})",
                facts.task_number.get(),
                prior_assignee_name
            )
        }
        TaskSystemActor::User { .. } => {
            format!("task #{} unassigned (by user)", facts.task_number.get())
        }
    };
    TaskEventMessage {
        content,
        metadata: metadata_json(TaskEventKind::Unclaim, facts, &Map::new()).to_string(),
    }
}

/// The `@name` an assignee carries, for the human-claim content line.
fn assignee_label(assignee: &Option<TaskEventAssignee>) -> String {
    match assignee {
        Some(a) => a.label().to_owned(),
        None => "agent".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_facts() -> TaskEventFacts {
        TaskEventFacts {
            task_id: TaskId::parse("task-1").unwrap(),
            task_number: TaskNumber::new(7).unwrap(),
            title: TaskTitle::parse("Ship it").unwrap(),
            status: TaskStatus::InProgress,
            assignee: Some(TaskEventAssignee::Agent {
                id: AgentId::parse("agent-9").unwrap(),
                names: AgentEventNames::new("norman", "Norman"),
            }),
            actor: TaskSystemActor::Agent {
                id: AgentId::parse("agent-1").unwrap(),
                names: AgentEventNames::new("alice", "Alice"),
            },
        }
    }

    #[test]
    fn edit_metadata_carries_the_uniform_object_with_the_task_edit_subtype() {
        let event = compose_edit_event(&agent_facts());
        assert_eq!(event.content, "@alice edited task #7 \"Ship it\"");
        let value: Value = serde_json::from_str(&event.metadata).unwrap();
        assert_eq!(value["subtype"], "task_edit");
        assert_eq!(value["task_number"], 7);
        assert_eq!(value["task_status"], "in_progress");
        assert_eq!(value["task_id"], "task-1");
        assert_eq!(value["task_title"], "Ship it");
        assert_eq!(value["task_assignee"]["type"], "agent");
        assert_eq!(value["task_assignee"]["id"], "agent-9");
        assert_eq!(value["task_assignee"]["name"], "Norman");
        assert_eq!(value["actor"]["kind"], "agent");
        assert_eq!(value["actor"]["id"], "agent-1");
        assert_eq!(value["actor"]["name"], "Alice");
    }

    #[test]
    fn claimed_create_names_actor_even_when_assignee_differs() {
        let facts = agent_facts();
        assert_eq!(
            compose_create_event(&facts, CreatePlacement::Claimed).content,
            "@alice created task #7 \"Ship it\" and claimed it"
        );
    }

    fn user_actor(mut facts: TaskEventFacts) -> TaskEventFacts {
        facts.actor = TaskSystemActor::User {
            id: UserId::parse("user-1").unwrap(),
            name: "user".to_owned(),
        };
        facts
    }

    #[test]
    fn status_content_names_actor_instead_of_assignee() {
        let facts = agent_facts();
        assert_eq!(
            compose_status_event(&facts).content,
            "task #7 → in_progress (by @alice)"
        );
    }

    #[test]
    fn assign_names_the_assignee_for_either_actor() {
        assert_eq!(
            compose_assign_event(&agent_facts()).content,
            "@alice assigned task #7 to @norman"
        );
        assert_eq!(
            compose_assign_event(&user_actor(agent_facts())).content,
            "task #7 assigned to @norman (by user)"
        );
    }

    #[test]
    fn agent_claim_names_the_actor_human_claim_names_the_assignee() {
        let facts = agent_facts();
        assert_eq!(
            compose_claim_event(&facts).content,
            "@alice claimed task #7"
        );
        // Human claim: the content names the ASSIGNEE (task_assignee), not the user.
        assert_eq!(
            compose_claim_event(&user_actor(agent_facts())).content,
            "task #7 assigned to @norman (by user)"
        );
    }

    #[test]
    fn agent_unclaim_names_the_prior_assignee_human_unclaim_says_unassigned() {
        let facts = agent_facts();
        assert_eq!(
            compose_unclaim_event(&facts, "Alice").content,
            "task #7 unclaimed (was @Alice)"
        );
        assert_eq!(
            compose_unclaim_event(&user_actor(agent_facts()), "Alice").content,
            "task #7 unassigned (by user)"
        );
    }

    // ---- Production-signed parity fixture replay (drift-prone signed boundary) ----

    enum Kind {
        Create,
        Claim,
        Unclaim,
        Status,
        Edit,
    }
    struct Case {
        name: &'static str,
        kind: Kind,
        content: &'static str,
        metadata: &'static str,
    }

    const CASES: &[Case] = include!("event_ts_fixture.rs");

    fn fixture_agent_content_name(id: &str) -> &str {
        match id {
            "agent-alice" => "alice",
            "agent-bob" => "bob",
            other => panic!("unknown fixture agent id {other}"),
        }
    }

    /// Replays the PRODUCTION-SIGNED event fixture: each case carries the exact
    /// `content` + `metadata` the real `TaskService` mutation path emitted. This
    /// reconstructs the composer's input FACTS from that signed metadata (task
    /// id/number/title/status/assignee/actor are all in it), runs the SAME composer
    /// the writers use, and asserts it reproduces byte-identical content +
    /// canonical-JSON-equal metadata. Because the fixture is generated from the real
    /// service (not a re-hand-written template), a production template/metadata change
    /// with no matching composer change fails HERE.
    #[test]
    fn matches_ts_signed_event_fixture() {
        for case in CASES {
            let md: Value = serde_json::from_str(case.metadata).unwrap();

            // Reconstruct the actor from the signed `actor` object.
            let actor_obj = &md["actor"];
            let actor = match actor_obj["kind"].as_str().unwrap() {
                "agent" => TaskSystemActor::Agent {
                    id: AgentId::parse(actor_obj["id"].as_str().unwrap()).unwrap(),
                    names: AgentEventNames::new(
                        fixture_agent_content_name(actor_obj["id"].as_str().unwrap()),
                        actor_obj["name"].as_str().unwrap(),
                    ),
                },
                "user" => TaskSystemActor::User {
                    id: UserId::parse(actor_obj["id"].as_str().unwrap()).unwrap(),
                    name: actor_obj["name"].as_str().unwrap().to_owned(),
                },
                other => panic!("unknown actor kind {other} on case {}", case.name),
            };
            // Reconstruct the assignee (or None) from the signed `task_assignee`.
            let assignee = match &md["task_assignee"] {
                Value::Null => None,
                assignee => Some(match assignee["type"].as_str().unwrap() {
                    "agent" => TaskEventAssignee::Agent {
                        id: AgentId::parse(assignee["id"].as_str().unwrap()).unwrap(),
                        names: AgentEventNames::new(
                            fixture_agent_content_name(assignee["id"].as_str().unwrap()),
                            assignee["name"].as_str().unwrap(),
                        ),
                    },
                    "user" => TaskEventAssignee::User {
                        id: UserId::parse(assignee["id"].as_str().unwrap()).unwrap(),
                        name: assignee["name"].as_str().unwrap().to_owned(),
                    },
                    other => panic!("unknown assignee type {other} on case {}", case.name),
                }),
            };
            let facts = TaskEventFacts {
                task_id: TaskId::parse(md["task_id"].as_str().unwrap().to_owned()).unwrap(),
                task_number: TaskNumber::new(md["task_number"].as_i64().unwrap()).unwrap(),
                title: TaskTitle::parse(md["task_title"].as_str().unwrap()).unwrap(),
                status: TaskStatus::parse(md["task_status"].as_str().unwrap()).unwrap(),
                assignee,
                actor,
            };
            let event = match &case.kind {
                Kind::Create => {
                    // Placement is discriminated by the signed content/metadata: the
                    // cross-conversation arm carries the `cross_conversation` flag; the
                    // claimed arm's content says "and claimed it".
                    let placement =
                        if md.get("cross_conversation").and_then(Value::as_bool) == Some(true) {
                            CreatePlacement::CrossConversation
                        } else if case.content.ends_with("and claimed it") {
                            CreatePlacement::Claimed
                        } else {
                            CreatePlacement::Filed
                        };
                    compose_create_event(&facts, placement)
                }
                Kind::Claim => compose_claim_event(&facts),
                Kind::Unclaim => {
                    // The agent arm names the prior assignee (`was @<name>`), which for a
                    // self-unclaim is the acting agent's name; the human arm ignores it.
                    let prior = match &facts.actor {
                        TaskSystemActor::Agent { names, .. } => names.content_name().to_owned(),
                        TaskSystemActor::User { .. } => String::new(),
                    };
                    compose_unclaim_event(&facts, &prior)
                }
                Kind::Status => compose_status_event(&facts),
                Kind::Edit => compose_edit_event(&facts),
            };
            assert_eq!(
                event.content, case.content,
                "content drift on case {}",
                case.name
            );
            // Canonical-JSON compare (field-order-independent): the composer's metadata
            // must equal the production-emitted metadata value-for-value.
            let got: Value = serde_json::from_str(&event.metadata).unwrap();
            assert_eq!(got, md, "metadata drift on case {}", case.name);
        }
        // Coverage denominator, asserted so the signed fixture cannot silently shrink.
        assert_eq!(
            CASES.len(),
            12,
            "expected 12 production-signed parity cases"
        );
    }
}
