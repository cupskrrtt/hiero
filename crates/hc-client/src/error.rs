use serde::Deserialize;
use std::fmt;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot connect to herdr-core at {path}: {source} (is `hc server start` running?)")]
    Connect {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("bad json from core: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error("connection to herdr-core closed")]
    Disconnected,
    #[error("unexpected message from core: {0}")]
    Protocol(String),
}

impl Error {
    /// The stable error kind when the core answered with an error.
    pub fn kind(&self) -> Option<&ErrorKind> {
        match self {
            Error::Rpc(e) => Some(&e.kind),
            _ => None,
        }
    }

    pub fn is_kind(&self, kind: ErrorKind) -> bool {
        self.kind() == Some(&kind)
    }
}

/// A JSON-RPC error from the core, decoded.
#[derive(Debug, Clone, thiserror::Error)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub kind: ErrorKind,
    pub hint: Option<String>,
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(h) = &self.hint {
            write!(f, " ({h})")?;
        }
        Ok(())
    }
}

/// Stable error kinds the core reports in `error.data.kind`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    AlreadyAdded,
    Blocked,
    Config,
    Conflict,
    EditFailed,
    Finished,
    HcMissing,
    Incompatible,
    Internal,
    InvalidParams,
    InvalidTemplate,
    Io,
    Live,
    MachineUnknown,
    NameTaken,
    NeedsFramed,
    NeedsTarget,
    NeedsUpdate,
    NotFound,
    NotText,
    SpawnFailed,
    StatusMismatch,
    Unavailable,
    Unreachable,
    UnsupportedGeneration,
    /// A kind this client doesn't know yet (the API only grows).
    #[serde(untagged)]
    Other(String),
}

#[derive(Deserialize)]
pub(crate) struct WireError {
    code: i64,
    message: String,
    #[serde(default)]
    data: Option<WireErrorData>,
}

#[derive(Deserialize)]
struct WireErrorData {
    #[serde(default)]
    kind: Option<ErrorKind>,
    #[serde(default)]
    hint: Option<String>,
}

impl From<WireError> for RpcError {
    fn from(w: WireError) -> Self {
        let (kind, hint) = match w.data {
            Some(d) => (d.kind, d.hint),
            None => (None, None),
        };
        RpcError {
            code: w.code,
            message: w.message,
            kind: kind.unwrap_or_else(|| ErrorKind::Other(String::new())),
            hint,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_known_and_unknown_kinds() {
        let w: WireError = serde_json::from_str(
            r#"{"code":-32000,"message":"no terminal t9","data":{"kind":"not_found","hint":"hc ls"}}"#,
        )
        .unwrap();
        let e = RpcError::from(w);
        assert_eq!(e.kind, ErrorKind::NotFound);
        assert_eq!(e.to_string(), "no terminal t9 (hc ls)");

        let w: WireError =
            serde_json::from_str(r#"{"code":-32000,"message":"x","data":{"kind":"brand_new"}}"#)
                .unwrap();
        assert_eq!(RpcError::from(w).kind, ErrorKind::Other("brand_new".into()));
    }
}
