//! Types the fixture's `[package.metadata.typedpg.types]` maps PG types to,
//! shared by every case under `src/bin/`.

use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mood {
    Happy,
    Sad,
}

impl fmt::Display for Mood {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mood::Happy => "happy",
            Mood::Sad => "sad",
        })
    }
}

impl FromStr for Mood {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "happy" => Ok(Mood::Happy),
            "sad" => Ok(Mood::Sad),
            other => Err(format!("unknown mood {other:?}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Prefs {
    pub theme: String,
}
