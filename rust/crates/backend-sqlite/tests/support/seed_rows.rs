//! Test seed rows for `agents` and `conversations`, rendered as `&'static str` SQL.
//!
//! A test names only the columns it cares about; required columns it omits get the
//! defaults below, so a schema change edits this file instead of every test. Values
//! are quoted SQL literals except `NULL`, `?` and `["raw sql"]`; integer columns
//! rely on SQLite's column affinity to store a quoted number as an integer.
//! A default copied from `id` repeats its value, so with `id = ?` name those columns
//! explicitly or the statement gains an extra bind. Mount with `#[macro_use]` before
//! any module that uses it.
#![allow(unused_macros)]

/// `INSERT INTO agents`. Defaults: `name` = id, `runtime` = claude, timestamps 1.
macro_rules! seed_agent {
    (id = $id:tt $(, $col:ident = $val:tt)* $(,)?) => {
        seed_row!(
            "agents",
            [id = $id $(, $col = $val)*],
            [name = $id, runtime = "claude", created_at = 1, updated_at = 1],
            []
        )
    };
}

/// `INSERT INTO conversations` for a workspace channel. Defaults: open roster,
/// `title` and `title_key` = id, timestamps 1.
macro_rules! seed_channel {
    (id = $id:tt $(, $col:ident = $val:tt)* $(,)?) => {
        seed_row!(
            "conversations",
            [id = $id $(, $col = $val)*],
            [roster_policy = "open", title = $id, title_key = $id, created_at = 1, updated_at = 1],
            [placement_kind = "workspace"]
        )
    };
}

/// `INSERT INTO conversations` for a direct pair. Defaults: fixed roster, empty
/// title, timestamps 1.
macro_rules! seed_direct_pair {
    (id = $id:tt $(, $col:ident = $val:tt)* $(,)?) => {
        seed_row!(
            "conversations",
            [id = $id $(, $col = $val)*],
            [roster_policy = "fixed", title = "", created_at = 1, updated_at = 1],
            [placement_kind = "direct_pair"]
        )
    };
}

/// Given columns in order, then each default the test did not name, then fixed columns.
macro_rules! seed_row {
    ($table:literal, $given:tt, [$($dk:ident = $dv:tt),*], [$($fk:ident = $fv:tt),*]) => {
        concat!(
            "INSERT INTO ", $table, " (", seed_names!($given),
            $(seed_missing!($dk $given (",", stringify!($dk))),)*
            $(",", stringify!($fk),)*
            ") VALUES (", seed_vals!($given),
            $(seed_missing!($dk $given (",", seed_val!($dv))),)*
            $(",", seed_val!($fv),)*
            ")"
        )
    };
}

macro_rules! seed_names {
    ([$c0:ident = $v0:tt $(, $c:ident = $v:tt)*]) => {
        concat!(stringify!($c0) $(, ",", stringify!($c))*)
    };
}

macro_rules! seed_vals {
    ([$c0:ident = $v0:tt $(, $c:ident = $v:tt)*]) => {
        concat!(seed_val!($v0) $(, ",", seed_val!($v))*)
    };
}

/// Expands to `concat!(then...)` unless the column list names `key`.
macro_rules! seed_missing {
    (name [name = $v:tt $(, $($r:tt)*)?] $then:tt) => { "" };
    (runtime [runtime = $v:tt $(, $($r:tt)*)?] $then:tt) => { "" };
    (roster_policy [roster_policy = $v:tt $(, $($r:tt)*)?] $then:tt) => { "" };
    (title [title = $v:tt $(, $($r:tt)*)?] $then:tt) => { "" };
    (title_key [title_key = $v:tt $(, $($r:tt)*)?] $then:tt) => { "" };
    (created_at [created_at = $v:tt $(, $($r:tt)*)?] $then:tt) => { "" };
    (updated_at [updated_at = $v:tt $(, $($r:tt)*)?] $then:tt) => { "" };
    ($key:ident [$c:ident = $v:tt $(, $($r:tt)*)?] $then:tt) => {
        seed_missing!($key [$($($r)*)?] $then)
    };
    ($key:ident [] ($($then:tt)*)) => { concat!($($then)*) };
}

macro_rules! seed_val {
    (NULL) => {
        "NULL"
    };
    (?) => {
        "?"
    };
    ([$raw:literal]) => {
        $raw
    };
    ($v:literal) => {
        concat!("'", $v, "'")
    };
}
