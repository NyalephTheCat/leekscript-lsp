//! Open document text for semantic token requests.

use std::collections::HashMap;

use parking_lot::RwLock;
use tower_lsp::Client;

pub struct Backend {
    /// Held for `tower_lsp` wiring; reserved for future requests.
    #[allow(dead_code)]
    pub client: Client,
    pub documents: RwLock<HashMap<String, String>>,
}
