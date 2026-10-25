//! Authenticated public counts; source visibility never filters an admitted subject.
use crate::foundation::clock::ProcessClock;
use crate::foundation::http_error::json_error as error;
use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use silverhand_backend_core::{
    foundation::identity::{AgentId, Principal, UserId},
    identity_access::{
        agent_detail_read::AgentDetailReadModel,
        capability::agent_view::{VisibleStages, can_view_agent},
        profile_message_counts::{
            MessageCountYear, ProfileMessageCountRead, TIME_ZONE, shanghai_year,
        },
        user_identity::UserProfileRead,
    },
};

pub const PATH: &str = "/api/profiles/{kind}/{id}/message-counts";
#[derive(Clone)]
struct Ports<R, U, A, C> {
    counts: R,
    users: U,
    agents: A,
    clock: C,
}
pub fn router<R, U, A, C>(counts: R, users: U, agents: A, clock: C) -> Router
where
    R: ProfileMessageCountRead,
    U: UserProfileRead,
    A: AgentDetailReadModel + Clone + Send + Sync + 'static,
    C: ProcessClock,
{
    Router::new()
        .route(PATH, get(read::<R, U, A, C>))
        .with_state(Ports {
            counts,
            users,
            agents,
            clock,
        })
}
#[derive(Deserialize)]
struct YearQuery {
    year: Option<String>,
}
#[derive(Serialize)]
struct Day {
    date: String,
    count: i64,
}
#[derive(Serialize)]
struct Output {
    subject: Subject,
    year: i32,
    time_zone: &'static str,
    days: Vec<Day>,
    available_years: Vec<i32>,
}
#[derive(Serialize)]
struct Subject {
    kind: String,
    id: String,
}
async fn read<R, U, A, C>(
    Path((kind, id)): Path<(String, String)>,
    Query(query): Query<YearQuery>,
    viewer: Option<Extension<UserId>>,
    State(p): State<Ports<R, U, A, C>>,
) -> Response
where
    R: ProfileMessageCountRead,
    U: UserProfileRead,
    A: AgentDetailReadModel,
    C: ProcessClock,
{
    let Some(Extension(viewer)) = viewer else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
    };
    let Some(year) = query
        .year
        .filter(|s| s.len() == 4 && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse().ok())
        .and_then(MessageCountYear::new)
    else {
        return error(StatusCode::BAD_REQUEST, "invalid year");
    };
    let subject = match kind.as_str() {
        "user" => match UserId::parse(id.clone()) {
            Ok(v) => Principal::human(v),
            Err(_) => return error(StatusCode::NOT_FOUND, "profile not found"),
        },
        "agent" => match AgentId::parse(id.clone()) {
            Ok(v) => Principal::agent(v),
            Err(_) => return error(StatusCode::NOT_FOUND, "profile not found"),
        },
        _ => return error(StatusCode::BAD_REQUEST, "invalid subject kind"),
    };
    let allowed = match &subject {
        Principal::Human { user_id } => p.users.get(user_id).await.map(|v| v.is_some()),
        Principal::Agent { agent_id } => p.agents.read_agent_detail(agent_id).await.map(|v| {
            v.as_ref().is_some_and(|v| v.source() == agent_id)
                && can_view_agent(
                    &Principal::human(viewer),
                    agent_id,
                    v.as_ref().map(|v| v.visibility()),
                    &VisibleStages::active_only(),
                )
                .is_allowed()
        }),
        Principal::System => unreachable!(),
    };
    match allowed {
        Ok(true) => {}
        Ok(false) => return error(StatusCode::NOT_FOUND, "profile not found"),
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"),
    };
    let counts = match p.counts.read_counts(&subject, &year).await {
        Ok(v) => v,
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error"),
    };
    let mut days: Vec<Day> = year
        .dates
        .iter()
        .map(|date| Day {
            date: date.clone(),
            count: 0,
        })
        .collect();
    for (index, count) in counts.days {
        let Some(day) = usize::try_from(index).ok().and_then(|i| days.get_mut(i)) else {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
        };
        if count < 0 {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
        };
        day.count = count;
    }
    let Some(current) =
        shanghai_year(p.clock.now_unix_millis()).filter(|v| (1970..=9998).contains(v))
    else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
    };
    let first = counts
        .earliest_ms
        .and_then(shanghai_year)
        .unwrap_or(current)
        .clamp(1970, current);
    Json(Output {
        subject: Subject { kind, id },
        year: year.year,
        time_zone: TIME_ZONE,
        days,
        available_years: (first..=current).collect(),
    })
    .into_response()
}
