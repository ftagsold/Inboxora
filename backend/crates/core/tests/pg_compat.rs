//! node-pg compatibility of row decoding and parameter binding against a real PostgreSQL.

mod common;

use inboxora_core::params;
use inboxora_core::pg::{Q, Val};

#[tokio::test]
async fn row_json_matches_node_pg() {
    let Some(db) = common::db() else { return };
    let sql = include_str!("fixtures/pg_types.sql");
    let row = db.json_opt(sql, params![]).await.unwrap().unwrap();
    let ours = serde_json::to_string(&row).unwrap();
    let node = include_str!("fixtures/pg_types.node.json").trim();
    assert_eq!(ours, node);
}

#[tokio::test]
async fn loose_params_bind_like_node_pg() {
    let Some(db) = common::db() else { return };
    let row = db
        .json_opt(
            "SELECT $1::uuid AS u, $2::int4 + 1 AS i, $3::bool AS b, $4::timestamptz AS t, $5::jsonb AS j, \
             $6::text[] AS a, $7::numeric AS n, $8::date AS d, $9::int8 AS big, $10::uuid[] AS ua",
            &[
                Val::from("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"),
                Val::from("41"),
                Val::from("true"),
                Val::from("2026-10-22T06:59:26.109Z"),
                Val::from(serde_json::json!({"k": [1, "x"]})),
                Val::from(vec!["x", "y"]),
                Val::from("12.340"),
                Val::from("2026-01-31"),
                Val::from(9_007_199_254_740_993_i64),
                Val::from(vec!["a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"]),
            ],
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_string(&row).unwrap(),
        r#"{"u":"a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11","i":42,"b":true,"t":"2026-10-22T06:59:26.109Z","j":{"k":[1,"x"]},"a":["x","y"],"n":"12.340","d":"2026-01-31T00:00:00.000Z","big":"9007199254740993","ua":["a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"]}"#
    );
}

#[tokio::test]
async fn invalid_uuid_param_is_an_error_not_a_panic() {
    let Some(db) = common::db() else { return };
    let err = db.opt("SELECT $1::uuid", params!["not-a-uuid"]).await.unwrap_err();
    assert!(err.to_string().contains("uuid"), "{err}");
}

#[tokio::test]
async fn serializable_transaction_commits_and_rolls_back() {
    let Some(db) = common::db() else { return };
    db.batch("CREATE TEMP TABLE IF NOT EXISTS _tx_probe (v int)").await.unwrap();
    db.batch("CREATE TABLE IF NOT EXISTS _rs_tx_probe (v int)").await.unwrap();
    db.exec("DELETE FROM _rs_tx_probe", params![]).await.unwrap();
    db.transaction(true, 2, |tx| {
        Box::pin(async move {
            tx.exec("INSERT INTO _rs_tx_probe VALUES (1)", params![]).await?;
            Ok(())
        })
    })
    .await
    .unwrap();
    let r: Result<(), _> = db
        .transaction(false, 0, |tx| {
            Box::pin(async move {
                tx.exec("INSERT INTO _rs_tx_probe VALUES (2)", params![]).await?;
                Err(inboxora_core::Error::internal("boom"))
            })
        })
        .await;
    assert!(r.is_err());
    let rows = db.rows("SELECT v FROM _rs_tx_probe ORDER BY v", params![]).await.unwrap();
    assert_eq!(rows.iter().map(|r| r.get::<_, i32>(0)).collect::<Vec<_>>(), vec![1]);
    db.batch("DROP TABLE _rs_tx_probe").await.unwrap();
}
