//! Secret references in tool arguments: finding them, resolving them, and
//! deciding whether an approval already covers a call that names one.
//!
//! Split out of `guarded.rs` when the permission gate pushed that file past its
//! module-size ratchet. This is the half of the gateway that never looks at a
//! policy: it walks the arguments, substitutes what a reference names, and
//! reports what it substituted so the result can be scrubbed of it. Keeping it
//! here means the guarded executor reads as the decisions it makes rather than
//! as the walking it does.
//!
//! A resolved value is returned beside the invocation rather than logged or
//! cached: the caller injects it immediately before the tool runs and scrubs it
//! from whatever comes back, and nothing else in the process ever holds it.

use agent_ports::secret_provider::{SecretProvider, SecretProviderError};
use agent_ports::tool_gateway::ToolGatewayError;
use agent_ports::tool_provider::ToolInvocation;
use agent_protocol::ids::SessionId;
use agent_protocol::permissions::{Capability, SecretRef};
use agent_protocol::tool::ToolErrorKind;

use crate::action::SECRET_SCHEME;
use crate::approval::ApprovalService;
use crate::scrub::ResolvedSecret;

/// Whether any string in `value` names a secret.
pub(crate) fn references_secret(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => text.contains(SECRET_SCHEME),
        serde_json::Value::Array(items) => items.iter().any(references_secret),
        serde_json::Value::Object(map) => map.values().any(references_secret),
        _ => false,
    }
}

/// Resolve every reference in the arguments, refusing the whole call if one
/// cannot be resolved.
///
/// All-or-nothing: a call that half-resolved would hand the tool a mix of
/// values and reference strings, and the tool would act on the mix. The
/// returned invocation is a copy — the caller's own value, and the event
/// already written from it, keep the references.
pub(crate) fn resolve_arguments(
    secrets: Option<&dyn SecretProvider>,
    invocation: &ToolInvocation,
) -> Result<(ToolInvocation, Vec<ResolvedSecret>), ToolGatewayError> {
    if !references_secret(&invocation.arguments) {
        return Ok((invocation.clone(), Vec::new()));
    }
    let mut resolved = Vec::new();
    let arguments =
        resolve_value(secrets, &invocation.tool_name, &invocation.arguments, &mut resolved)?;
    let mut resolved_invocation = invocation.clone();
    resolved_invocation.arguments = arguments;
    Ok((resolved_invocation, resolved))
}

fn resolve_value(
    secrets: Option<&dyn SecretProvider>,
    tool: &str,
    value: &serde_json::Value,
    resolved: &mut Vec<ResolvedSecret>,
) -> Result<serde_json::Value, ToolGatewayError> {
    match value {
        serde_json::Value::String(text) => {
            resolve_text(secrets, tool, text, resolved).map(serde_json::Value::String)
        }
        serde_json::Value::Array(items) => items
            .iter()
            .map(|item| resolve_value(secrets, tool, item, resolved))
            .collect::<Result<Vec<_>, _>>()
            .map(serde_json::Value::Array),
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(key, item)| {
                resolve_value(secrets, tool, item, resolved).map(|value| (key.clone(), value))
            })
            .collect::<Result<serde_json::Map<_, _>, _>>()
            .map(serde_json::Value::Object),
        other => Ok(other.clone()),
    }
}

/// Substitute every `secret://<source>/<name>` in one string.
///
/// A reference is parsed only at the scheme's start and ends at the first
/// character that cannot appear in a name, so `Bearer secret://env/TOKEN`
/// inside a shell command substitutes exactly the token and leaves the rest of
/// the command alone. A malformed reference is refused rather than passed
/// through as text: a tool handed the literal reference would do something with
/// it, and the caller would have meant a value.
fn resolve_text(
    secrets: Option<&dyn SecretProvider>,
    tool: &str,
    text: &str,
    resolved: &mut Vec<ResolvedSecret>,
) -> Result<String, ToolGatewayError> {
    let Some(first) = text.find(SECRET_SCHEME) else {
        return Ok(text.to_string());
    };
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..first]);
    let mut rest = &text[first..];
    while let Some(start) = rest.find(SECRET_SCHEME) {
        out.push_str(&rest[..start]);
        let after = &rest[start + SECRET_SCHEME.len()..];
        let end = after
            .find(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/')))
            .unwrap_or(after.len());
        let body = &after[..end];
        let reference = format!("{SECRET_SCHEME}{body}");
        let malformed = || ToolGatewayError::SecretUnavailable {
            tool: tool.to_string(),
            reference: reference.clone(),
            reason: "a secret reference is written secret://<source>/<name>".to_string(),
            kind: ToolErrorKind::InvalidArguments,
        };
        let Some((source, name)) = body.split_once('/') else {
            return Err(malformed());
        };
        if source.is_empty() || name.is_empty() || name.contains('/') {
            return Err(malformed());
        }
        let secret = SecretRef { source: source.to_string(), name: name.to_string() };
        let Some(provider) = secrets else {
            return Err(ToolGatewayError::SecretUnavailable {
                tool: tool.to_string(),
                reference: secret.reference(),
                reason: "this run has no secret provider configured".to_string(),
                kind: ToolErrorKind::PermissionDenied,
            });
        };
        let value = provider.resolve(&secret).map_err(|error| match error {
            SecretProviderError::NotFound { .. } => ToolGatewayError::SecretUnavailable {
                tool: tool.to_string(),
                reference: secret.reference(),
                reason: "is not available to this run".to_string(),
                kind: ToolErrorKind::PermissionDenied,
            },
            SecretProviderError::Failed(message) => ToolGatewayError::SecretUnavailable {
                tool: tool.to_string(),
                reference: secret.reference(),
                reason: format!("the secret provider failed: {message}"),
                kind: ToolErrorKind::Unavailable,
            },
        })?;
        resolved.push(ResolvedSecret { reference: secret.reference(), value: value.clone() });
        out.push_str(&value);
        rest = &after[end..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Whether an approval already covers the capabilities a secret-using call
/// declares.
///
/// Any intersecting grant is enough: the grant was given for this tool's
/// capability, and a secret names a value the person approving the call could
/// not see either way.
pub(crate) async fn secret_use_granted(
    approvals: &ApprovalService,
    session_id: &SessionId,
    capabilities: &[Capability],
) -> Result<bool, ToolGatewayError> {
    let leases = approvals
        .leases_in_effect(session_id)
        .await
        .map_err(|e| ToolGatewayError::Approval(e.to_string()))?;
    Ok(leases.iter().any(|lease| capabilities.contains(&lease.capability)))
}
