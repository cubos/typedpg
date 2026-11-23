//! Compiles cleanly although the calling module shadows every prelude name
//! the generated code could refer to unqualified.

#![allow(dead_code, non_camel_case_types, unused_imports, unused_macros)]

struct Option;
struct Some;
struct None;
struct Result;
struct Ok;
struct Err;
struct Vec;
struct String;
struct Box;
struct Into;
struct From;
struct Iterator;
struct IntoIterator;
struct ToString;
struct Default;
struct Clone;
struct Copy;
struct Send;
struct Sync;
struct Sized;
struct Drop;
struct Fn;
struct FnOnce;
struct FnMut;
struct Debug;
struct ToOwned;
struct AsRef;
struct PartialEq;
struct Eq;
struct Extend;
struct Unpin;
struct std;
struct core;
// A module of the same name as the crate.
mod typedpg {}

// The std macros, shadowed.
macro_rules! format {
    ($($t:tt)*) => {
        ::core::compile_error!("a std macro the caller shadows")
    };
}
macro_rules! include_str {
    ($($t:tt)*) => {
        ::core::compile_error!("a std macro the caller shadows")
    };
}
macro_rules! unreachable {
    ($($t:tt)*) => {
        ::core::compile_error!("a std macro the caller shadows")
    };
}
macro_rules! panic {
    ($($t:tt)*) => {
        ::core::compile_error!("a std macro the caller shadows")
    };
}
macro_rules! vec {
    ($($t:tt)*) => {
        ::core::compile_error!("a std macro the caller shadows")
    };
}
macro_rules! file {
    ($($t:tt)*) => {
        ::core::compile_error!("a std macro the caller shadows")
    };
}
macro_rules! line {
    ($($t:tt)*) => {
        ::core::compile_error!("a std macro the caller shadows")
    };
}
macro_rules! column {
    ($($t:tt)*) => {
        ::core::compile_error!("a std macro the caller shadows")
    };
}
macro_rules! assert {
    ($($t:tt)*) => {
        ::core::compile_error!("a std macro the caller shadows")
    };
}
macro_rules! matches {
    ($($t:tt)*) => {
        ::core::compile_error!("a std macro the caller shadows")
    };
}

struct NewUser {
    name: ::std::string::String,
    email: ::std::string::String,
}

async fn case(db: &tokio_postgres::Client) -> ::std::result::Result<(), ::typedpg::Error> {
    let id = 1_i64;
    let ids = [1_i64, 2];
    let user = ::typedpg::sql!(
        db,
        "SELECT name, age, mood, prefs, addr, ARRAY[name] AS names, \
         ARRAY[age] AS ages, ARRAY[mood] AS moods, ARRAY[prefs] AS prefss, \
         ARRAY[addr] AS addrs, ROW(1, age) AS r, '\\x00'::bytea AS b \
         FROM users WHERE id = $id AND id IN $..ids"
    )
    .fetch_one()
    .await?;
    let _ = (
        user.name, user.age, user.mood, user.prefs, user.addr, user.names,
    );

    let name = "x";
    let mood = ::std::option::Option::Some(typedpg_compile_fail_fixture::Mood::Happy);
    let names = ["a", "b"];
    ::typedpg::sql!(
        db,
        "UPDATE users SET name = $name, mood = $mood WHERE name = ANY($names)"
    )
    .execute()
    .await?;

    let users = [NewUser {
        name: "a".into(),
        email: "a@example.com".into(),
    }];
    ::typedpg::sql!(
        db,
        "INSERT INTO users (name, email) VALUES $..users { name, email } RETURNING id"
    )
    .fetch_all()
    .await?;
    ::typedpg::sql!(db, "SELECT id FROM users")
        .fetch_optional()
        .await?;
    ::typedpg::sql!(db, "SELECT id FROM users")
        .fetch_stream()
        .await?;
    ::typedpg::copy_in!(db, "users (name, email)", users { name, email }).await?;

    let prefs = ::std::option::Option::Some(typedpg_compile_fail_fixture::Prefs {
        theme: "dark".into(),
    });
    let moods = [typedpg_compile_fail_fixture::Mood::Sad];
    let prefss = [typedpg_compile_fail_fixture::Prefs {
        theme: "light".into(),
    }];
    ::typedpg::sql!(
        db,
        "UPDATE users SET prefs = $prefs WHERE mood = ANY($moods) OR prefs = ANY($prefss::prefs[])"
    )
    .execute()
    .await?;
    let _ = ::typedpg::sql!(db, "SELECT age FROM users WHERE id = $id")
        .fetch_value_optional()
        .await?;
    let _: ::std::vec::Vec<Named> = ::typedpg::sql!(db, "SELECT name FROM users")
        .fetch_all_as()
        .await?;
    let _ = ::typedpg::embed_migrations!("./migrations");
    ::std::result::Result::Ok(())
}

#[derive(::typedpg::FromRow)]
struct Named {
    name: ::std::string::String,
}

fn main() {
    let _ = case;
}
