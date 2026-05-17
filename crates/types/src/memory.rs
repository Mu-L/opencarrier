//! Knowledge graph types: entities, relations, and graph query primitives.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// An entity in the knowledge graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entity {
    /// Unique entity ID.
    pub id: String,
    /// Entity type (Person, Organization, Project, etc.).
    pub entity_type: EntityType,
    /// Display name.
    pub name: String,
    /// Arbitrary properties.
    pub properties: HashMap<String, serde_json::Value>,
    /// When this entity was created.
    pub created_at: DateTime<Utc>,
    /// When this entity was last updated.
    pub updated_at: DateTime<Utc>,
}

/// Types of entities in the knowledge graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    /// A person.
    Person,
    /// An organization.
    Organization,
    /// A project.
    Project,
    /// A concept or idea.
    Concept,
    /// An event.
    Event,
    /// A location.
    Location,
    /// A document.
    Document,
    /// A tool.
    Tool,
    /// A custom type.
    Custom(String),
}

/// A relation between two entities in the knowledge graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relation {
    /// Source entity ID.
    pub source: String,
    /// Relation type.
    pub relation: RelationType,
    /// Target entity ID.
    pub target: String,
    /// Arbitrary properties on the relation.
    pub properties: HashMap<String, serde_json::Value>,
    /// Confidence score (0.0 - 1.0).
    pub confidence: f32,
    /// When this relation was created.
    pub created_at: DateTime<Utc>,
}

/// Types of relations in the knowledge graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationType {
    /// Entity works at an organization.
    WorksAt,
    /// Entity knows about a concept.
    KnowsAbout,
    /// Entities are related.
    RelatedTo,
    /// Entity depends on another.
    DependsOn,
    /// Entity is owned by another.
    OwnedBy,
    /// Entity was created by another.
    CreatedBy,
    /// Entity is located in another.
    LocatedIn,
    /// Entity is part of another.
    PartOf,
    /// Entity uses another.
    Uses,
    /// Entity produces another.
    Produces,
    /// A custom relation type.
    Custom(String),
}

/// A pattern for querying the knowledge graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphPattern {
    /// Optional source entity filter.
    pub source: Option<String>,
    /// Optional relation type filter.
    pub relation: Option<RelationType>,
    /// Optional target entity filter.
    pub target: Option<String>,
    /// Maximum traversal depth.
    pub max_depth: u32,
}

/// A result from a graph query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphMatch {
    /// The source entity.
    pub source: Entity,
    /// The relation.
    pub relation: Relation,
    /// The target entity.
    pub target: Entity,
}
