//! The SQL spelling of a conversation's lifecycle: `archived_at` is its only stamp.
//! `$p` is the table prefix (`""`, `"c."`, ...).

macro_rules! conversation_open {
    ($p:literal) => {
        concat!($p, "archived_at IS NULL")
    };
}

macro_rules! conversation_archived {
    ($p:literal) => {
        concat!($p, "archived_at IS NOT NULL")
    };
}

pub(crate) use {conversation_archived, conversation_open};
