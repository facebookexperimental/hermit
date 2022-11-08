// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use core::fmt;
use core::str::FromStr;

use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

/// A recording ID.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Id(Uuid);

impl Id {
    /// Generates a new, random ID.
    pub fn unique() -> Self {
        Self(Uuid::new_v4())
    }
}

impl FromStr for Id {
    type Err = <Uuid as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::from_str(s).map(Self)
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&self.0.to_simple_ref(), f)
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&self.0.to_simple_ref(), f)
    }
}
