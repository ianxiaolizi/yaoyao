//! Secret references in tool arguments: finding them, checking them, resolving
//! them, and writing down that one was used.
//!
//! Split out of `guarded.rs` when the permission gate pushed that file past its
//! module-size ratchet. This is the half of the gateway that never makes a
//! policy decision: it walks the arguments, answers the two questions a
//! reference raises — *could this run resolve it* (before a person is asked) and
//! *what is it* (after they said yes) — and reports what it substituted so the
//! result can be scrubbed of it. Keeping it here means the guarded executor
//! reads as the decisions it makes rather than as the walking it does.
//!
//! The order is the point. [`check_arguments`] runs before the approval is
//! raised and never produces a value; [`resolve_arguments`] runs after the
//! decision to execute exists. A value that exists before the decision is a
//! value that can leak before anyone agreed to it.
//!
//! A resolved value is returned beside the invocation rather than logged or
//! cached: the caller injects it immediately before the tool runs and scrubs it
//! from whatever comes back, and nothing else in the process ever holds it.

use agent_ports::secret_provider::{SecretProvider, SecretProviderError};
use agent_ports::tool_gateway::ToolGatewayError;
use agent_ports::tool_provider::ToolInvocation;
use agent_protocol::ids::SessionId;
use agent_protocol::permission::ActionFacts;
use agent_protocol::permissions::SecretRef;
use agent_protocol::tool::ToolErrorKind;

use crate::action::SECRET_SCHEME;
use crate::approval::ApprovalService;
use crate::permission::{PermissionDecisionRecord, PermissionService};
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

/// One reference and the byte range it occupies in the string it came from.
struct Found {
    range: std::ops::Range<usize>,
    secret: SecretRef,
}

/// Whether a character can be part of a reference's body.
fn is_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/')
}

/// Every `secret://<source>/<name>` in one string, refusing a malformed one.
///
/// A reference is parsed only at the scheme's start and ends at the first
/// character that cannot appear in a name, so `Bearer secret://env/TOKEN`
/// inside a shell command substitutes exactly the token and leaves the rest of
/// the command alone. A malformed reference is refused rather than passed
/// through as text: a tool handed the literal reference would do something with
/// it, and the caller would have meant a value.
fn scan_text(tool: &str, text: &str) -> Result<Vec<Found>, ToolGatewayError> {
    let mut found = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = text[cursor..].find(SECRET_SCHEME) {
        let start = cursor + offset;
        let body_at = start + SECRET_SCHEME.len();
        let after = &text[body_at..];
        let end = after.find(|ch: char| !is_name_char(ch)).unwrap_or(after.len());
        let body = &after[..end];
        let malformed = || ToolGatewayError::SecretUnavailable {
            tool: tool.to_string(),
            reference: format!("{SECRET_SCHEME}{body}"),
            reason: "a secret reference is written secret://<source>/<name>".to_string(),
            kind: ToolErrorKind::InvalidArguments,
        };
        let Some((source, name)) = body.split_once('/') else {
            return Err(malformed());
        };
        if source.is_empty() || name.is_empty() || name.contains('/') {
            return Err(malformed());
        }
        found.push(Found {
            range: start..body_at + end,
            secret: SecretRef { source: source.to_string(), name: name.to_string() },
        });
        cursor = body_at + end;
    }
    Ok(found)
}

/// Every string inside `value`, in the order the arguments hold them.
fn strings<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a str>) {
    match value {
        serde_json::Value::String(text) => out.push(text),
        serde_json::Value::Array(items) => items.iter().for_each(|item| strings(item, out)),
        serde_json::Value::Object(map) => map.values().for_each(|item| strings(item, out)),
        _ => {}
    }
}

/// Refuse a call whose references this run could not resolve — without
/// producing a single value.
///
/// Runs before the approval is raised, because a reference this run cannot
/// resolve is a call that could never run, and a person should not be asked to
/// approve one. It is a separate pass from [`resolve_arguments`] rather than an
/// early return of it for that reason: this one asks the provider *whether*, and
/// a value it never asked for cannot be in memory before the decision exists.
pub(crate) fn check_arguments(
    secrets: Option<&dyn SecretProvider>,
    invocation: &ToolInvocation,
) -> Result<(), ToolGatewayError> {
    let mut texts = Vec::new();
    strings(&invocation.arguments, &mut texts);
    for text in texts {
        for found in scan_text(&invocation.tool_name, text)? {
            let secret = found.secret;
            let Some(provider) = secrets else {
                return Err(ToolGatewayError::SecretUnavailable {
                    tool: invocation.tool_name.clone(),
                    reference: secret.reference(),
                    reason: "this run has no secret provider configured".to_string(),
                    kind: ToolErrorKind::PermissionDenied,
                });
            };
            provider
                .has(&secret)
                .map_err(|error| unavailable(&invocation.tool_name, &secret, error))?;
        }
    }
    Ok(())
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

/// Substitute every reference in one string.
fn resolve_text(
    secrets: Option<&dyn SecretProvider>,
    tool: &str,
    text: &str,
    resolved: &mut Vec<ResolvedSecret>,
) -> Result<String, ToolGatewayError> {
    let found = scan_text(tool, text)?;
    if found.is_empty() {
        return Ok(text.to_string());
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for Found { range, secret } in found {
        out.push_str(&text[cursor..range.start]);
        let value = resolve_one(secrets, tool, &secret)?;
        resolved.push(ResolvedSecret { reference: secret.reference(), value: value.clone() });
        out.push_str(&value);
        cursor = range.end;
    }
    out.push_str(&text[cursor..]);
    Ok(out)
}

/// The value behind one reference, or the refusal that stands in its place.
fn resolve_one(
    secrets: Option<&dyn SecretProvider>,
    tool: &str,
    secret: &SecretRef,
) -> Result<String, ToolGatewayError> {
    let Some(provider) = secrets else {
        return Err(ToolGatewayError::SecretUnavailable {
            tool: tool.to_string(),
            reference: secret.reference(),
            reason: "this run has no secret provider configured".to_string(),
            kind: ToolErrorKind::PermissionDenied,
        });
    };
    provider.resolve(secret).map_err(|error| unavailable(tool, secret, error))
}

/// A provider's refusal, as the caller reads it.
///
/// The provider's own message is not copied: this refusal reaches the model and
/// is written into `tool/failed`, and a message is arbitrary text nobody here
/// can check for the value it may be about. A secret that cannot be resolved is
/// worth a vague sentence and not worth a plaintext in a log (CONTRACT §7 #181).
fn unavailable(tool: &str, secret: &SecretRef, error: SecretProviderError) -> ToolGatewayError {
    match error {
        SecretProviderError::NotFound { .. } => ToolGatewayError::SecretUnavailable {
            tool: tool.to_string(),
            reference: secret.reference(),
            reason: "is not available to this run".to_string(),
            kind: ToolErrorKind::PermissionDenied,
        },
        SecretProviderError::Failed(_) => ToolGatewayError::SecretUnavailable {
            tool: tool.to_string(),
            reference: secret.reference(),
            reason: "the secret provider could not resolve it".to_string(),
            kind: ToolErrorKind::Unavailable,
        },
    }
}

/// Write down that a call is about to use these references.
///
/// Called at dispatch — after the decision, immediately before the tool runs —
/// and it carries no value: the digest of the action the person answered, the
/// tool, the references as written, and the origin the call would reach. That
/// is the whole of what an auditor needs and the whole of what this event may
/// hold (CONTRACT §7 #181).
///
/// A gateway assembled without a ledger has nowhere to write and says so by
/// writing nothing, exactly as its permission decisions are made but not kept.
pub(crate) async fn record_access(
    permissions: Option<&PermissionService>,
    session_id: &SessionId,
    action: &ActionFacts,
    decision: &PermissionDecisionRecord,
    secrets: &[ResolvedSecret],
) -> Result<(), ToolGatewayError> {
    if secrets.is_empty() {
        return Ok(());
    }
    let Some(service) = permissions else {
        return Ok(());
    };
    let mut references: Vec<String> =
        secrets.iter().map(|secret| secret.reference.clone()).collect();
    references.sort();
    references.dedup();
    service
        .record_secret_access(session_id, action, decision, &references)
        .await
        .map(|_| ())
        .map_err(|error| ToolGatewayError::PermissionAudit {
            tool: action.tool_name.clone(),
            reason: error.to_string(),
        })
}

/// Whether an approval already covers the secret-using call in front of us.
///
/// The lease has to cover *this* action, not merely one of its capabilities
/// (B4): a grant made for reading a file says nothing about a call that would
/// send a secret somewhere. One lease covering the whole fingerprint is what
/// makes this call one a person already saw.
pub(crate) async fn secret_use_granted(
    approvals: &ApprovalService,
    session_id: &SessionId,
    action: &ActionFacts,
) -> Result<bool, ToolGatewayError> {
    let leases = approvals
        .leases_in_effect(session_id)
        .await
        .map_err(|e| ToolGatewayError::Approval(e.to_string()))?;
    Ok(leases.iter().any(|lease| lease.matches_action(action)))
}
