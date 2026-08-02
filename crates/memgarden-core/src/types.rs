use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// Kind of a memory node's underlying fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FactType {
    World,
    Observation,
    Experience,
}

impl FactType {
    pub fn as_str(&self) -> &'static str {
        match self {
            FactType::World => "world",
            FactType::Observation => "observation",
            FactType::Experience => "experience",
        }
    }
}

impl fmt::Display for FactType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for FactType {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "world" => Ok(FactType::World),
            "observation" => Ok(FactType::Observation),
            "experience" => Ok(FactType::Experience),
            other => Err(Error::Invalid(format!("unknown fact_type: {other}"))),
        }
    }
}

impl Serialize for FactType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for FactType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        FactType::from_str(&s).map_err(serde::de::Error::custom)
    }
}

/// Kind of relationship between two memory nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkType {
    Semantic,
    Temporal,
    Entity,
    CausedBy,
    Causes,
    Enables,
    Prevents,
}

impl LinkType {
    pub fn as_str(&self) -> &'static str {
        match self {
            LinkType::Semantic => "semantic",
            LinkType::Temporal => "temporal",
            LinkType::Entity => "entity",
            LinkType::CausedBy => "caused_by",
            LinkType::Causes => "causes",
            LinkType::Enables => "enables",
            LinkType::Prevents => "prevents",
        }
    }
}

impl fmt::Display for LinkType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LinkType {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "semantic" => Ok(LinkType::Semantic),
            "temporal" => Ok(LinkType::Temporal),
            "entity" => Ok(LinkType::Entity),
            "caused_by" => Ok(LinkType::CausedBy),
            "causes" => Ok(LinkType::Causes),
            "enables" => Ok(LinkType::Enables),
            "prevents" => Ok(LinkType::Prevents),
            other => Err(Error::Invalid(format!("unknown link_type: {other}"))),
        }
    }
}

impl Serialize for LinkType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for LinkType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        LinkType::from_str(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_FACT_TYPES: [FactType; 3] =
        [FactType::World, FactType::Observation, FactType::Experience];

    const ALL_LINK_TYPES: [LinkType; 7] = [
        LinkType::Semantic,
        LinkType::Temporal,
        LinkType::Entity,
        LinkType::CausedBy,
        LinkType::Causes,
        LinkType::Enables,
        LinkType::Prevents,
    ];

    #[test]
    fn fact_type_round_trips_through_str() {
        for ft in ALL_FACT_TYPES {
            let s = ft.as_str();
            assert_eq!(FactType::from_str(s).unwrap(), ft);
        }
    }

    #[test]
    fn fact_type_round_trips_through_json() {
        for ft in ALL_FACT_TYPES {
            let json = serde_json::to_string(&ft).unwrap();
            let back: FactType = serde_json::from_str(&json).unwrap();
            assert_eq!(back, ft);
        }
    }

    #[test]
    fn fact_type_unknown_is_err() {
        assert!(FactType::from_str("bogus").is_err());
        assert!(serde_json::from_str::<FactType>("\"bogus\"").is_err());
    }

    #[test]
    fn link_type_has_seven_variants_round_tripping_through_str() {
        assert_eq!(ALL_LINK_TYPES.len(), 7);
        for lt in ALL_LINK_TYPES {
            let s = lt.as_str();
            assert_eq!(LinkType::from_str(s).unwrap(), lt);
        }
    }

    #[test]
    fn link_type_snake_case_json_values() {
        assert_eq!(
            serde_json::to_string(&LinkType::CausedBy).unwrap(),
            "\"caused_by\""
        );
        assert_eq!(
            serde_json::to_string(&LinkType::Semantic).unwrap(),
            "\"semantic\""
        );
    }

    #[test]
    fn link_type_round_trips_through_json() {
        for lt in ALL_LINK_TYPES {
            let json = serde_json::to_string(&lt).unwrap();
            let back: LinkType = serde_json::from_str(&json).unwrap();
            assert_eq!(back, lt);
        }
    }

    #[test]
    fn link_type_unknown_is_err() {
        assert!(LinkType::from_str("bogus").is_err());
        assert!(serde_json::from_str::<LinkType>("\"bogus\"").is_err());
    }
}
