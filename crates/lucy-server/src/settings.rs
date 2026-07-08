use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct ServerSettings {
    pub config_path: Option<Arc<str>>,
}
