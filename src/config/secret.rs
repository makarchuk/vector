use std::collections::HashMap;

use enum_dispatch::enum_dispatch;
use vector_config::NamedComponent;

use crate::signal;

/// Generalized interface to a secret backend.
#[enum_dispatch]
pub trait SecretBackend: NamedComponent + core::fmt::Debug + Send + Sync {
    fn retrieve(
        &mut self,
        secret_keys: Vec<String>,
        signal_rx: &mut signal::SignalRx,
    ) -> crate::Result<HashMap<String, String>>;
}
