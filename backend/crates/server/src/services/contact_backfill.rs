//! Idempotent startup backfills for contacts (ports of `contactRichBackfill.ts`, run by the
//! Node migration runner, and `backfillContactPhotos` in `index.ts`).

use inboxora_core::pg::{Db, Q, Val};
use inboxora_core::{Result, params};

fn to_json<T: serde::Serialize>(v: &T) -> Val {
    Val::from(inboxora_core::json::stringify(&serde_json::to_value(v).unwrap_or_default()))
}

/// `backfillRichContactFields`: fill the denormalised rich columns from legacy vCards.
pub async fn backfill_rich_fields(db: &Db) -> Result<usize> {
    let rows = db
        .rows(
            r"SELECT id, vcard FROM contacts
              WHERE vcard IS NOT NULL AND rich_fields_backfilled_at IS NULL AND (
                (vcard ~* '(?m)^(?:[A-Z0-9-]+\.)?TITLE[^:]*:.+' AND title IS NULL)
                OR (vcard ~* '(?m)^(?:[A-Z0-9-]+\.)?ROLE[^:]*:.+' AND role IS NULL)
                OR (vcard ~* '(?m)^(?:[A-Z0-9-]+\.)?NICKNAME[^:]*:.+' AND nickname IS NULL)
                OR (vcard ~* '(?m)^(?:[A-Z0-9-]+\.)?URL[^:]*:.+' AND urls = '[]'::jsonb)
                OR (vcard ~* '(?m)^(?:[A-Z0-9-]+\.)?IMPP[^:]*:.+' AND instant_messages = '[]'::jsonb)
                OR (vcard ~* '(?m)^(?:[A-Z0-9-]+\.)?CATEGORIES[^:]*:.+' AND categories = '[]'::jsonb)
                OR (vcard ~* '(?m)^(?:[A-Z0-9-]+\.)?ADR[^:]*:.+' AND addresses = '[]'::jsonb))",
            params![],
        )
        .await?;
    for row in &rows {
        let id: uuid::Uuid = row.get("id");
        let vcard: String = row.get("vcard");
        let rich = inboxora_pim::vcard::parse(&vcard);
        db.exec(
            "UPDATE contacts SET title = $1, role = $2, nickname = $3, urls = $4, instant_messages = $5, categories = $6, \
             addresses = $7, rich_fields_backfilled_at = NOW() WHERE id = $8",
            &[
                Val::from(rich.title),
                Val::from(rich.role),
                Val::from(rich.nickname),
                to_json(&rich.urls),
                to_json(&rich.instant_messages),
                to_json(&rich.categories),
                to_json(&rich.addresses),
                Val::from(id),
            ],
        )
        .await?;
    }
    Ok(rows.len())
}

/// `backfillContactPhotos`: store `photo_data` for contacts synced before it was persisted.
pub async fn backfill_photos(db: &Db) -> Result<usize> {
    let rows =
        db.rows("SELECT id, vcard FROM contacts WHERE vcard IS NOT NULL AND photo_data IS NULL", params![]).await?;
    let mut count = 0;
    for row in &rows {
        let Some(vcard) = row.get::<_, Option<String>>("vcard") else { continue };
        let Some(photo) = inboxora_pim::vcard::parse(&vcard).photo_data.filter(|p| !p.is_empty()) else { continue };
        db.exec("UPDATE contacts SET photo_data = $1 WHERE id = $2", params![photo, row.get::<_, uuid::Uuid>("id")])
            .await?;
        count += 1;
    }
    Ok(count)
}
