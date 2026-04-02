use std::collections::HashMap;

/// Verified caller identity produced by the auth pipeline.
///
/// Attached to the request as an extension after successful auth:
/// ```rust,ignore
/// req.extensions_mut().insert(auth_ctx);
/// ```
///
/// Downstream middleware reads it without re-doing any verification:
/// ```rust,ignore
/// let ctx = req.extensions().get::<AuthContext>().expect("auth ran");
/// let subject = &ctx.subject;
/// ```
#[derive(Debug, Clone)]
pub struct AuthContext {
    /// The canonical subject identifier.
    ///
    /// - JWT: `sub` claim
    /// - API key: the key's associated principal (stored in Redis alongside the key)
    /// - mTLS only: the certificate's Subject Common Name
    pub subject: String,

    /// Auth mechanism(s) that passed for this request.
    pub mechanisms: Vec<AuthMechanism>,

    /// JWT claims extracted verbatim, if JWT auth ran.
    /// Useful for scope/role checks in upstream services via forwarded headers.
    pub claims: Option<Claims>,

    /// mTLS peer identity, if mTLS auth ran.
    pub peer_identity: Option<PeerIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMechanism {
    Jwt,
    ApiKey,
    Mtls,
}

/// Decoded JWT claims.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Claims {
    /// Subject
    pub sub: String,
    /// Issuer
    pub iss: Option<String>,
    /// Audience (may be a single string or an array)
    pub aud: Option<serde_json::Value>,
    /// Expiry (Unix timestamp)
    pub exp: Option<u64>,
    /// Scopes (space-separated string, as per RFC 8693)
    pub scope: Option<String>,
    /// Any extra claims the token carries
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl Claims {
    /// Returns the list of scopes if a `scope` claim is present.
    pub fn scopes(&self) -> Vec<&str> {
        match &self.scope {
            Some(s) => s.split_whitespace().collect(),
            None => Vec::new(),
        }
    }
}

/// mTLS peer certificate identity.
#[derive(Debug, Clone)]
pub struct PeerIdentity {
    /// Certificate Subject Distinguished Name
    pub subject_dn: String,
    /// Certificate Issuer Distinguished Name
    pub issuer_dn: String,
    /// SHA-256 fingerprint (hex-encoded)
    pub fingerprint: String,
}
