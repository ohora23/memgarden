use memgarden_core::types::FactType;

#[derive(Debug, Clone, PartialEq)]
pub struct Bank {
    pub bank_id: String,
    pub mission: Option<String>,
    pub disposition: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Fields required to insert a new memory node. `uuid`, `created_at`, and
/// `updated_at` are assigned by `nodes::insert`.
#[derive(Debug, Clone, Copy)]
pub struct NewNode<'a> {
    pub bank_id: &'a str,
    pub document_id: Option<i64>,
    pub fact_type: FactType,
    pub text: &'a str,
    pub context: Option<&'a str>,
    pub event_date: Option<i64>,
    pub occurred_start: Option<i64>,
    pub occurred_end: Option<i64>,
    pub mentioned_at: Option<i64>,
    pub metadata: Option<&'a str>,
}

impl<'a> NewNode<'a> {
    /// A node with only the required fields set; temporal/metadata fields
    /// default to `None`.
    pub fn new(bank_id: &'a str, fact_type: FactType, text: &'a str) -> Self {
        NewNode {
            bank_id,
            document_id: None,
            fact_type,
            text,
            context: None,
            event_date: None,
            occurred_start: None,
            occurred_end: None,
            mentioned_at: None,
            metadata: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MemoryNode {
    pub id: i64,
    pub uuid: String,
    pub bank_id: String,
    pub document_id: Option<i64>,
    pub fact_type: FactType,
    pub text: String,
    pub context: Option<String>,
    pub embedding: Option<Vec<u8>>,
    pub event_date: Option<i64>,
    pub occurred_start: Option<i64>,
    pub occurred_end: Option<i64>,
    pub mentioned_at: Option<i64>,
    pub metadata: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}
