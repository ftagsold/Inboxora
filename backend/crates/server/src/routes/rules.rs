//! `/api/rules` CRUD and reorder (port of routes/rules.ts). `POST /run` executes the inbox
//! rules engine and is served with the mail engine (Phase 5).

use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, patch, put};
use inboxora_core::pg::{Q, Val};
use inboxora_core::{Error, Result, params};
use inboxora_mail::rules::{normalize_actions, validate_actions, validate_conditions};
use serde_json::{Value, json};

use crate::http::{Auth, Body};
use crate::routes::util::{err, fail, json, ok};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/reorder", patch(reorder))
        .route("/{id}", put(update).delete(remove))
}

async fn list(State(s): State<AppState>, auth: Auth) -> Result<Response> {
    let rows =
        s.db.json_rows(
            "SELECT * FROM inbox_rules WHERE user_id = $1 ORDER BY priority ASC, created_at ASC",
            params![&auth.user_id],
        )
        .await
        .map_err(fail("GET /rules error", "Failed to load rules"))?;
    Ok(ok(Value::Array(rows)))
}

struct Validated {
    conditions: Value,
    actions: Vec<Value>,
    account_id: Option<Value>,
}

fn validate(body: &Body) -> std::result::Result<Validated, Response> {
    let (Some(conditions), Some(actions)) =
        (body.get("conditions").and_then(Value::as_array), body.get("actions").and_then(Value::as_array))
    else {
        return Err(err(400, "conditions and actions must be arrays"));
    };
    if let Some(e) = validate_conditions(conditions) {
        return Err(err(400, e));
    }
    let normalized = normalize_actions(actions);
    if let Some(e) = validate_actions(&normalized) {
        return Err(err(400, e));
    }
    let account_id = body.get("accountId").filter(|v| inboxora_core::json::truthy(Some(v))).cloned();
    let actions = normalized
        .into_iter()
        .filter(|a| account_id.is_some() || a.get("type").and_then(Value::as_str) != Some("move"))
        .collect();
    Ok(Validated { conditions: Value::Array(conditions.clone()), actions, account_id })
}

/// Ownership of the account and existence of a move destination (when folders are known).
async fn check_account(s: &AppState, user_id: &str, v: &Validated) -> Result<Option<Response>> {
    let Some(account) = &v.account_id else { return Ok(None) };
    let owned = s
        .db
        .opt("SELECT id FROM email_accounts WHERE id = $1 AND user_id = $2", &[Val::from(account), Val::from(user_id)])
        .await?;
    if owned.is_none() {
        return Ok(Some(err(403, "Account not found")));
    }
    let dest = v.actions.iter().find_map(|a| {
        (a.get("type").and_then(Value::as_str) == Some("move"))
            .then(|| a.get("value").and_then(Value::as_str))
            .flatten()
            .map(|d| inboxora_mail::text::js_trim(d).to_owned())
            .filter(|d| !d.is_empty())
    });
    if let Some(dest) = dest {
        let row = s
            .db
            .opt(
                "SELECT COUNT(*) AS total, COUNT(*) FILTER (WHERE path = $2) AS match FROM folders WHERE account_id = $1",
                &[Val::from(account), Val::from(dest)],
            )
            .await?;
        if let Some(r) = row {
            let (total, matched): (i64, i64) = (r.get(0), r.get(1));
            if total > 0 && matched == 0 {
                return Ok(Some(err(400, "Move destination folder not found for this account")));
            }
        }
    }
    Ok(None)
}

fn name_of(body: &Body) -> Val {
    match body.get("name") {
        Some(v) if inboxora_core::json::truthy(Some(v)) => Val::from(v.clone()),
        _ => Val::from(""),
    }
}

fn logic_of(body: &Body) -> &'static str {
    if body.str("conditionLogic") == Some("OR") { "OR" } else { "AND" }
}

async fn create(State(s): State<AppState>, auth: Auth, body: Body) -> Result<Response> {
    let v = match validate(&body) {
        Ok(v) => v,
        Err(r) => return Ok(r),
    };
    let run = async {
        if let Some(r) = check_account(&s, &auth.user_id, &v).await? {
            return Ok(r);
        }
        let priority =
            s.db.opt("SELECT COUNT(*) AS cnt FROM inbox_rules WHERE user_id = $1", params![&auth.user_id])
                .await?
                .map_or(0, |r| r.get::<_, i64>(0));
        let row = s
            .db
            .json_opt(
                "INSERT INTO inbox_rules (user_id, account_id, name, enabled, stop_processing, priority, condition_logic, conditions, actions) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING *",
                &[
                    Val::from(&auth.user_id),
                    v.account_id.clone().map_or(Val::Null, Val::from),
                    name_of(&body),
                    Val::Bool(body.get("enabled") != Some(&Value::Bool(false))),
                    Val::Bool(body.truthy("stopProcessing")),
                    Val::Int(priority),
                    Val::from(logic_of(&body)),
                    Val::from(inboxora_core::json::stringify(&v.conditions)),
                    Val::from(inboxora_core::json::stringify(&Value::Array(v.actions.clone()))),
                ],
            )
            .await?;
        Ok::<_, Error>(json(201, row.map_or(Value::Null, Value::Object)))
    };
    run.await.map_err(fail("POST /rules error", "Failed to create rule"))
}

async fn update(State(s): State<AppState>, auth: Auth, Path(id): Path<String>, body: Body) -> Result<Response> {
    let v = match validate(&body) {
        Ok(v) => v,
        Err(r) => return Ok(r),
    };
    let run = async {
        if let Some(r) = check_account(&s, &auth.user_id, &v).await? {
            return Ok(r);
        }
        let row = s
            .db
            .json_opt(
                "UPDATE inbox_rules SET name = $1, account_id = $2, enabled = $3, stop_processing = $4, condition_logic = $5, \
                 conditions = $6, actions = $7, updated_at = NOW() WHERE id = $8 AND user_id = $9 RETURNING *",
                &[
                    name_of(&body),
                    v.account_id.clone().map_or(Val::Null, Val::from),
                    Val::Bool(body.get("enabled") != Some(&Value::Bool(false))),
                    Val::Bool(body.truthy("stopProcessing")),
                    Val::from(logic_of(&body)),
                    Val::from(inboxora_core::json::stringify(&v.conditions)),
                    Val::from(inboxora_core::json::stringify(&Value::Array(v.actions.clone()))),
                    Val::from(&id),
                    Val::from(&auth.user_id),
                ],
            )
            .await?;
        Ok::<_, Error>(match row {
            Some(r) => ok(Value::Object(r)),
            None => err(404, "Rule not found"),
        })
    };
    run.await.map_err(fail("PUT /rules/:id error", "Failed to update rule"))
}

async fn remove(State(s): State<AppState>, auth: Auth, Path(id): Path<String>) -> Result<Response> {
    let row =
        s.db.opt("DELETE FROM inbox_rules WHERE id = $1 AND user_id = $2 RETURNING id", params![&id, &auth.user_id])
            .await
            .map_err(fail("DELETE /rules/:id error", "Failed to delete rule"))?;
    Ok(match row {
        Some(_) => ok(json!({ "ok": true })),
        None => err(404, "Rule not found"),
    })
}

async fn reorder(State(s): State<AppState>, auth: Auth, body: Body) -> Result<Response> {
    let Some(ids) = body.get("ids").and_then(Value::as_array).cloned() else {
        return Ok(err(400, "ids must be an array"));
    };
    let run = async {
        let owned =
            s.db.rows(
                "SELECT id FROM inbox_rules WHERE id = ANY($1::uuid[]) AND user_id = $2",
                &[Val::from(Value::Array(ids.clone())), Val::from(&auth.user_id)],
            )
            .await?;
        if owned.len() != ids.len() {
            return Ok(err(403, "One or more rules not found"));
        }
        for (i, id) in ids.iter().enumerate() {
            s.db.exec("UPDATE inbox_rules SET priority = $1 WHERE id = $2", &[Val::from(i), Val::from(id)]).await?;
        }
        Ok::<_, Error>(ok(json!({ "ok": true })))
    };
    run.await.map_err(fail("PATCH /rules/reorder error", "Failed to reorder rules"))
}
