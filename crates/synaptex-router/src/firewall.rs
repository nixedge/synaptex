/// nftables firewall rule management.
///
/// Rules are applied via the `nft` CLI tool (shells out).  A future
/// implementation may use the `nftnl` crate for direct netlink communication,
/// which avoids the `nft` binary dependency and enables atomic rule batches.
///
/// # Table / chain layout (assumed pre-existing on the router)
/// ```
/// table inet synaptex {
///     chain forward {
///         type filter hook forward priority filter; policy accept;
///         # managed rules inserted here, keyed by comment UUID
///     }
///     chain input {
///         type filter hook input priority filter; policy accept;
///         # managed rules inserted here
///     }
/// }
/// ```
///
/// # Current state
/// Stub — logs the intended operation but does not execute anything.
use anyhow::Result;

use synaptex_router_proto::{FirewallAction, FirewallHook, FirewallRule};

/// The nftables table synaptex rules live in.
const TABLE: &str = "inet synaptex";

pub async fn upsert(rule: &FirewallRule) -> Result<()> {
    // Remove any existing rule with this ID first (idempotent upsert).
    remove(&rule.id).await?;

    let nft_rule = build_nft_rule(rule)?;
    tracing::warn!(
        id      = %rule.id,
        comment = %rule.comment,
        nft     = %nft_rule,
        "firewall: upsert not yet implemented — would run: nft add rule {TABLE} {}",
        chain_name(rule.hook()),
    );
    // TODO: tokio::process::Command::new("nft").args(["add", "rule", TABLE, chain_name(hook), &nft_rule]).status().await?;
    Ok(())
}

pub async fn remove(id: &str) -> Result<()> {
    // Rules are identified by their comment field (UUID stored in the comment).
    // nftables supports `nft delete rule ... handle N` but requires knowing
    // the handle.  Approach: `nft -a list chain` → find handle for comment=id
    // → `nft delete rule ... handle N`.
    tracing::warn!(%id, "firewall: remove not yet implemented");
    // TODO: implement handle lookup + delete
    Ok(())
}

pub async fn list() -> Result<Vec<FirewallRule>> {
    // TODO: `nft -j list ruleset` → parse JSON → extract synaptex-managed rules
    Ok(vec![])
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn chain_name(hook: FirewallHook) -> &'static str {
    match hook {
        FirewallHook::Input   => "input",
        FirewallHook::Forward => "forward",
        FirewallHook::Output  => "output",
        FirewallHook::Unspecified => "forward",
    }
}

fn action_keyword(action: FirewallAction) -> &'static str {
    match action {
        FirewallAction::Accept      => "accept",
        FirewallAction::Drop        => "drop",
        FirewallAction::Reject      => "reject",
        FirewallAction::Unspecified => "drop",
    }
}

/// Build the nftables rule fragment (everything after `nft add rule TABLE CHAIN`).
fn build_nft_rule(rule: &FirewallRule) -> Result<String> {
    let mut parts: Vec<String> = vec![];

    if !rule.src_cidr.is_empty() {
        parts.push(format!("ip saddr {}", rule.src_cidr));
    }
    if !rule.dst_cidr.is_empty() {
        parts.push(format!("ip daddr {}", rule.dst_cidr));
    }
    if !rule.ip_proto.is_empty() {
        parts.push(rule.ip_proto.clone());
    }
    if rule.dst_port != 0 {
        parts.push(format!("dport {}", rule.dst_port));
    }

    parts.push(action_keyword(rule.action()).to_string());

    // Embed the rule UUID as a comment so we can find it for deletion.
    if !rule.id.is_empty() {
        let desc = if rule.comment.is_empty() {
            rule.id.clone()
        } else {
            format!("{} ({})", rule.id, rule.comment)
        };
        parts.push(format!("comment \"{}\"", desc));
    }

    Ok(parts.join(" "))
}
