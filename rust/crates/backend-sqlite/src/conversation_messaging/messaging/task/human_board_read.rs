//! SQLite implementation of the bounded human task-board read model.

use silverhand_backend_core::conversation_messaging::messaging::task::{
    HumanTaskBoardRead, TaskBoardAssigneeFilter, TaskBoardColumnPage, TaskBoardColumnRequest,
    TaskBoardContainer, TaskBoardCounts, TaskBoardCreatorFilter, TaskBoardFacets, TaskBoardFilters,
    TaskBoardPerson, TaskListCandidate, TaskStatus,
};
use silverhand_backend_core::foundation::persistence::PersistenceFailure;
use silverhand_backend_core::foundation::wire_number::JsSafeInteger;
use sqlx::{QueryBuilder, Row, Sqlite};

use super::row::{
    SELECT_TASK_JOINED, decode_row, decode_sidecars, seal_board_against_conversation,
};
use super::{SqliteTaskStore, corrupt, driver};

fn push_population(builder: &mut QueryBuilder<Sqlite>, filters: &TaskBoardFilters) {
    let viewer = filters.viewer.as_str();
    builder
        .push("EXISTS (SELECT 1 FROM conversations vc WHERE vc.id=t.conversation_id AND ((vc.placement_kind='workspace' AND EXISTS (SELECT 1 FROM conversation_users vcu WHERE vcu.conversation_id=vc.id AND vcu.user_id=")
        .push_bind(viewer.to_owned())
        .push(") AND EXISTS (SELECT 1 FROM workspace_users vwu WHERE vwu.workspace_id=vc.workspace_id AND vwu.user_id=")
        .push_bind(viewer.to_owned())
        .push(")) OR (vc.placement_kind='direct_pair' AND ((vc.endpoint_a_kind='human' AND vc.endpoint_a_id=")
        .push_bind(viewer.to_owned())
        .push(") OR (vc.endpoint_b_kind='human' AND vc.endpoint_b_id=")
        .push_bind(viewer.to_owned())
        .push(")))))");

    if let Some(workspace) = &filters.workspace {
        builder
            .push(" AND t.workspace_id=")
            .push_bind(workspace.as_str().to_owned());
    }
    if let Some(conversation) = &filters.conversation {
        builder
            .push(" AND t.conversation_id=")
            .push_bind(conversation.as_str().to_owned());
    }
    match filters.container {
        Some(TaskBoardContainer::Workspace) => builder.push(" AND EXISTS (SELECT 1 FROM conversations fc JOIN workspaces fw ON fw.id=fc.workspace_id WHERE fc.id=t.conversation_id AND fc.placement_kind='workspace' AND fw.owner_agent_id IS NULL)"),
        Some(TaskBoardContainer::AgentSpace) => builder.push(" AND EXISTS (SELECT 1 FROM conversations fc JOIN workspaces fw ON fw.id=fc.workspace_id WHERE fc.id=t.conversation_id AND fc.placement_kind='workspace' AND fw.owner_agent_id IS NOT NULL)"),
        Some(TaskBoardContainer::DirectMessages) => builder.push(" AND EXISTS (SELECT 1 FROM conversations fc WHERE fc.id=t.conversation_id AND fc.placement_kind='direct_pair')"),
        None => builder,
    };
    match &filters.assignee {
        Some(TaskBoardAssigneeFilter::Agent(id)) => {
            builder
                .push(" AND t.assignee_agent_id=")
                .push_bind(id.as_str().to_owned());
        }
        Some(TaskBoardAssigneeFilter::User(id)) => {
            builder
                .push(" AND t.assignee_user_id=")
                .push_bind(id.as_str().to_owned());
        }
        Some(TaskBoardAssigneeFilter::Unassigned) => {
            builder.push(" AND t.assignee_agent_id IS NULL AND t.assignee_user_id IS NULL");
        }
        None => {}
    }
    match &filters.creator {
        Some(TaskBoardCreatorFilter::Agent(id)) => {
            builder
                .push(" AND t.creator_kind='agent' AND t.creator_agent_id=")
                .push_bind(id.as_str().to_owned());
        }
        Some(TaskBoardCreatorFilter::User(id)) => {
            builder
                .push(" AND t.creator_kind='user' AND t.creator_user_id=")
                .push_bind(id.as_str().to_owned());
        }
        None => {}
    }
    if let Some(created_after) = filters.created_after {
        builder.push(" AND t.created_at>=").push_bind(created_after);
    }
}

fn facet_filters(
    viewer: &silverhand_backend_core::foundation::identity::UserId,
    container: Option<TaskBoardContainer>,
) -> TaskBoardFilters {
    TaskBoardFilters {
        viewer: viewer.clone(),
        workspace: None,
        conversation: None,
        container,
        assignee: None,
        creator: None,
        created_after: None,
    }
}

impl HumanTaskBoardRead for SqliteTaskStore {
    async fn column(
        &self,
        request: &TaskBoardColumnRequest,
    ) -> Result<TaskBoardColumnPage, PersistenceFailure> {
        let mut builder = QueryBuilder::<Sqlite>::new(SELECT_TASK_JOINED);
        builder.push(" WHERE ");
        push_population(&mut builder, &request.filters);
        builder
            .push(" AND t.status=")
            .push_bind(request.status.as_str().to_owned());
        if let Some(cursor) = &request.cursor {
            builder
                .push(" AND t.task_number<=")
                .push_bind(cursor.task_number.get())
                .push(" AND (t.task_number<")
                .push_bind(cursor.task_number.get())
                .push(" OR t.id<")
                .push_bind(cursor.task_id.as_str().to_owned())
                .push(")");
        }
        let probe = request
            .limit
            .checked_add(1)
            .ok_or_else(|| corrupt("task_board.limit"))?;
        let probe = i64::try_from(probe).map_err(|_| corrupt("task_board.limit"))?;
        builder
            .push(" ORDER BY t.task_number DESC,t.id DESC LIMIT ")
            .push_bind(probe);
        let rows = builder
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(driver)?;
        let has_more = rows.len() > request.limit;
        let mut decoded = Vec::with_capacity(rows.len().min(request.limit));
        for row in rows.into_iter().take(request.limit) {
            let task = decode_row(&row)?;
            seal_board_against_conversation(&row, &task)?;
            decoded.push(TaskListCandidate::new(task, decode_sidecars(&row)?));
        }
        Ok(TaskBoardColumnPage {
            rows: decoded,
            has_more,
        })
    }

    async fn counts(
        &self,
        filters: &TaskBoardFilters,
    ) -> Result<TaskBoardCounts, PersistenceFailure> {
        let mut builder =
            QueryBuilder::<Sqlite>::new("SELECT t.status,COUNT(*) AS n FROM tasks t WHERE ");
        push_population(&mut builder, filters);
        builder.push(" GROUP BY t.status");
        let rows = builder
            .build()
            .fetch_all(&self.pool)
            .await
            .map_err(driver)?;
        let mut values = [0_i64; 5];
        for row in rows {
            let status: String = row.try_get("status").map_err(driver)?;
            let n: i64 = row.try_get("n").map_err(driver)?;
            let index = match TaskStatus::parse(&status).map_err(|_| corrupt("tasks.status"))? {
                TaskStatus::Todo => 0,
                TaskStatus::InProgress => 1,
                TaskStatus::InReview => 2,
                TaskStatus::Done => 3,
                TaskStatus::Closed => 4,
            };
            values[index] = n;
        }
        let safe = |value| JsSafeInteger::new(value).map_err(|_| corrupt("task_board.count"));
        Ok(TaskBoardCounts {
            todo: safe(values[0])?,
            in_progress: safe(values[1])?,
            in_review: safe(values[2])?,
            done: safe(values[3])?,
            closed: safe(values[4])?,
        })
    }

    async fn facets(
        &self,
        viewer: &silverhand_backend_core::foundation::identity::UserId,
        container: Option<TaskBoardContainer>,
    ) -> Result<TaskBoardFacets, PersistenceFailure> {
        let filters = facet_filters(viewer, container);
        // One SQLite statement owns the complete response. This is stronger than
        // three awaits inside a read transaction: there is no seam at which a
        // writer can splice together assignee, creator, and unassigned populations.
        let mut facets = QueryBuilder::<Sqlite>::new(
            "SELECT DISTINCT 'assignee' AS category,'agent' AS kind,t.assignee_agent_id AS id,COALESCE(a.display_name,t.assignee_agent_id) AS name FROM tasks t LEFT JOIN agents a ON a.id=t.assignee_agent_id WHERE ",
        );
        push_population(&mut facets, &filters);
        facets.push(" AND t.assignee_agent_id IS NOT NULL UNION SELECT DISTINCT 'assignee','user',t.assignee_user_id,COALESCE(u.display_name,t.assignee_user_id) FROM tasks t LEFT JOIN users u ON u.id=t.assignee_user_id WHERE ");
        push_population(&mut facets, &filters);
        facets.push(" AND t.assignee_user_id IS NOT NULL UNION SELECT DISTINCT 'creator','agent',t.creator_agent_id,COALESCE(a.display_name,t.creator_agent_id) FROM tasks t LEFT JOIN agents a ON a.id=t.creator_agent_id WHERE ");
        push_population(&mut facets, &filters);
        facets.push(" AND t.creator_kind='agent' AND t.creator_agent_id IS NOT NULL UNION SELECT DISTINCT 'creator','user',t.creator_user_id,COALESCE(u.display_name,t.creator_user_id) FROM tasks t LEFT JOIN users u ON u.id=t.creator_user_id WHERE ");
        push_population(&mut facets, &filters);
        facets.push(" AND t.creator_kind='user' AND t.creator_user_id IS NOT NULL UNION SELECT 'unassigned','none',NULL,NULL WHERE EXISTS(SELECT 1 FROM tasks t WHERE ");
        push_population(&mut facets, &filters);
        facets.push(" AND t.assignee_agent_id IS NULL AND t.assignee_user_id IS NULL)");

        let rows = facets.build().fetch_all(&self.pool).await.map_err(driver)?;
        let mut assignees = Vec::new();
        let mut creators = Vec::new();
        let mut present = false;
        for row in rows {
            let category: String = row.try_get("category").map_err(driver)?;
            if category == "unassigned" {
                present = true;
                continue;
            }
            let kind: String = row.try_get("kind").map_err(driver)?;
            let id: String = row.try_get("id").map_err(driver)?;
            let name: String = row.try_get("name").map_err(driver)?;
            let person = match kind.as_str() {
                "agent" => TaskBoardPerson::Agent {
                    id: silverhand_backend_core::foundation::identity::AgentId::parse(id)
                        .map_err(|_| corrupt("task_board.facet_agent"))?,
                    name,
                },
                "user" => TaskBoardPerson::User {
                    id: silverhand_backend_core::foundation::identity::UserId::parse(id)
                        .map_err(|_| corrupt("task_board.facet_user"))?,
                    name,
                },
                _ => return Err(corrupt("task_board.facet_kind")),
            };
            match category.as_str() {
                "assignee" => assignees.push(person),
                "creator" => creators.push(person),
                _ => return Err(corrupt("task_board.facet_category")),
            }
        }
        sort_people(&mut assignees);
        sort_people(&mut creators);
        Ok(TaskBoardFacets {
            assignees,
            creators,
            has_unassigned: present,
        })
    }
}

fn person_sort_key(person: &TaskBoardPerson) -> (&str, &'static str, &str) {
    match person {
        TaskBoardPerson::Agent { id, name } => (name, "agent", id.as_str()),
        TaskBoardPerson::User { id, name } => (name, "user", id.as_str()),
    }
}

fn sort_people(people: &mut [TaskBoardPerson]) {
    people.sort_by(|left, right| {
        let left = person_sort_key(left);
        let right = person_sort_key(right);
        left.0
            .as_bytes()
            .cmp(right.0.as_bytes())
            .then_with(|| left.1.as_bytes().cmp(right.1.as_bytes()))
            .then_with(|| left.2.as_bytes().cmp(right.2.as_bytes()))
    });
}

#[cfg(test)]
mod ordering_tests {
    use serde::Deserialize;
    use silverhand_backend_core::foundation::identity::{AgentId, UserId};

    use super::*;

    #[derive(Deserialize)]
    struct Fixture {
        facet_order: FacetOrder,
    }

    #[derive(Deserialize)]
    struct FacetOrder {
        input: Vec<Person>,
        expected: Vec<Person>,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
    struct Person {
        name: String,
        #[serde(rename = "type")]
        kind: String,
        id: String,
    }

    fn domain(person: Person) -> TaskBoardPerson {
        match person.kind.as_str() {
            "agent" => TaskBoardPerson::Agent {
                id: AgentId::parse(person.id).unwrap(),
                name: person.name,
            },
            "user" => TaskBoardPerson::User {
                id: UserId::parse(person.id).unwrap(),
                name: person.name,
            },
            other => panic!("unknown fixture person type {other}"),
        }
    }

    fn wire(person: &TaskBoardPerson) -> Person {
        let (name, kind, id) = person_sort_key(person);
        Person {
            name: name.to_owned(),
            kind: kind.to_owned(),
            id: id.to_owned(),
        }
    }

    #[test]
    fn production_comparator_consumes_shared_utf8_total_order_fixture() {
        let fixture: Fixture = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../packages/shared/tests/fixtures/task-schedule-wire-canonicalization.json"
        )))
        .unwrap();
        let mut people = fixture
            .facet_order
            .input
            .into_iter()
            .map(domain)
            .collect::<Vec<_>>();
        sort_people(&mut people);
        assert_eq!(
            people.iter().map(wire).collect::<Vec<_>>(),
            fixture.facet_order.expected
        );
    }
}
