//! Request/reply payload types for the four read capabilities:
//! `resources.list`, `balances.read`, `history.read`, `status.read`.
//!
//! These are the typed shapes carried in a `Request`'s `params` and a
//! `Reply`'s `ok` once the op string has been recognized (see
//! `envelope::Request`, which keeps `params` as raw JSON precisely so an
//! unrecognized op or a schema mismatch is an ordinary `err` reply, not a
//! parse failure). No `execute()` -- exactly these four, plus `hello`.

use crate::observation::{BalanceWire, ObservationWire, PageRequest, ResourceStatus};
use crate::shape::object_only;
use serde::{Deserialize, Serialize};

pub const OP_HELLO: &str = "hello";
pub const OP_RESOURCES_LIST: &str = "resources.list";
pub const OP_BALANCES_READ: &str = "balances.read";
pub const OP_HISTORY_READ: &str = "history.read";
pub const OP_STATUS_READ: &str = "status.read";

/// One resource to read, with its own paging state. A single
/// `balances.read`/`history.read` call can name several resources at once
/// -- "EVERY REQUESTED resource_id APPEARS IN statuses EXACTLY ONCE" (the
/// frozen contract, section (e)) only makes sense for a batch request.
///
/// `page` is `Option<PageRequest>` (Contract Amendment 1, Ruling A8):
/// absent means "from the start of available history." An empty-string (or
/// otherwise sentinel) cursor value overloading "start from the beginning"
/// would make an opaque, provider-owned token carry host-defined meaning --
/// exactly the "unexplained magic" the merge bar rejects. `None` says it
/// directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct ResourceQuery {
    pub resource_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<PageRequest>,
}

object_only!(ResourceQuery, "a resource query: an object", serialize);

/// `resources.list` params: no filter fields defined in this milestone.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct ResourcesListParams {}

object_only!(
    ResourcesListParams,
    "resources.list params: an object",
    serialize
);

/// One resource an adapter can serve reads for.
///
/// No `surface` here: surface is a property of an *observation* -- carried
/// on `Provenance` -- not of a resource, because one resource can be
/// observed through several surfaces (Wise's activities-vs-statements
/// case). `resources.list` runs before anything has been observed, so
/// there is nothing to attach a surface to yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct ResourceDescriptor {
    pub resource_id: String,
    pub provider_id: String,
    pub kind: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_extra: Option<serde_json::Map<String, serde_json::Value>>,
}

object_only!(
    ResourceDescriptor,
    "a resource descriptor: an object",
    serialize
);

/// `resources.list` reply: `{"resources": [...]}`, and nothing else --
/// no `observations`, no `statuses`. Nothing was requested before
/// discovery, so there is nothing to report a per-resource outcome for.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct ResourcesListReply {
    pub resources: Vec<ResourceDescriptor>,
}

object_only!(
    ResourcesListReply,
    "a resources.list reply body: an object",
    serialize
);

/// `balances.read` params: batched, NOT paginated -- balances have no
/// cursor, so this is just the list of resources to read.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct BalancesReadParams {
    pub resource_ids: Vec<String>,
}

object_only!(
    BalancesReadParams,
    "balances.read params: an object",
    serialize
);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct BalancesReadReply {
    pub observations: Vec<BalanceWire>,
    pub statuses: Vec<ResourceStatus>,
}

object_only!(
    BalancesReadReply,
    "a balances.read reply body: an object",
    serialize
);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct HistoryReadParams {
    pub resources: Vec<ResourceQuery>,
}

object_only!(
    HistoryReadParams,
    "history.read params: an object",
    serialize
);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct HistoryReadReply {
    pub observations: Vec<ObservationWire>,
    pub statuses: Vec<ResourceStatus>,
}

object_only!(
    HistoryReadReply,
    "a history.read reply body: an object",
    serialize
);

/// `status.read` params: just the resource ids -- there is nothing to
/// paginate when the answer is "is this resource still reachable".
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct StatusReadParams {
    pub resource_ids: Vec<String>,
}

object_only!(StatusReadParams, "status.read params: an object", serialize);

/// `status.read` reply. No `observations` field: this op fetches nothing,
/// it only reports reachability/credential state per resource (the extra
/// `credential_expires_at`/`strong_auth_expires_at`/`history_start` fields
/// on `ResourceStatus` are populated here, and only here).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct StatusReadReply {
    pub statuses: Vec<ResourceStatus>,
}

object_only!(
    StatusReadReply,
    "a status.read reply body: an object",
    serialize
);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn balances_read_params_round_trip() {
        let params = BalancesReadParams {
            resource_ids: vec!["acct1".to_owned()],
        };
        let json = serde_json::to_string(&params).unwrap();
        let back: BalancesReadParams = serde_json::from_str(&json).unwrap();
        assert_eq!(back.resource_ids, vec!["acct1".to_owned()]);
    }

    #[test]
    fn balances_read_params_wire_shape_has_no_page_field() {
        // A3: balances are batched, not paginated -- ResourceQuery::page
        // (which history.read still uses) must not leak into this shape.
        let params = BalancesReadParams {
            resource_ids: vec!["acct1".to_owned()],
        };
        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(json, serde_json::json!({"resource_ids": ["acct1"]}));
    }

    #[test]
    fn history_read_params_stay_batched_per_resource_cursor() {
        let params = HistoryReadParams {
            resources: vec![ResourceQuery {
                resource_id: "acct1".to_owned(),
                page: Some(PageRequest::Cursor {
                    cursor: "abc".to_owned(),
                }),
            }],
        };
        let json = serde_json::to_string(&params).unwrap();
        let back: HistoryReadParams = serde_json::from_str(&json).unwrap();
        assert_eq!(back.resources.len(), 1);
        assert_eq!(back.resources[0].resource_id, "acct1");
    }

    #[test]
    fn history_read_params_absent_page_means_start_of_history() {
        // Ruling A8: absent `page` means "from the start of available
        // history" -- not an empty-string cursor sentinel.
        let json = serde_json::json!({
            "resources": [{"resource_id": "acct1"}]
        });
        let params: HistoryReadParams = serde_json::from_value(json).unwrap();
        assert!(params.resources[0].page.is_none());

        let out = serde_json::to_value(&params).unwrap();
        assert!(out["resources"][0].get("page").is_none());
    }

    #[test]
    fn resources_list_reply_drops_surface_and_has_no_observations_or_statuses() {
        let reply = ResourcesListReply {
            resources: vec![ResourceDescriptor {
                resource_id: "acct1".to_owned(),
                provider_id: "p1".to_owned(),
                kind: "bank_checking".to_owned(),
                label: "Checking".to_owned(),
                provider_extra: None,
            }],
        };
        let json = serde_json::to_value(&reply).unwrap();
        assert!(json.get("observations").is_none());
        assert!(json.get("statuses").is_none());
        assert!(json["resources"][0].get("surface").is_none());
    }

    #[test]
    fn status_read_reply_has_no_observations_field() {
        let reply = StatusReadReply { statuses: vec![] };
        let json = serde_json::to_value(&reply).unwrap();
        assert!(json.get("observations").is_none());
        assert!(json.get("statuses").is_some());
    }
}
