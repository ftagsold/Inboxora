//! CardDAV server for local address books (port of `routes/carddav.ts`), mounted at
//! `/carddav` outside `/api` and authenticated with DAV application passwords.

use std::collections::HashSet;
use std::sync::LazyLock;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use inboxora_core::pg::Row;
use inboxora_core::pg::{Q, Val};
use inboxora_core::{Error, Result, crypto, params};
use inboxora_mail::sanitize::encode_uri_component;
use inboxora_mail::text::js_trim;
use regex::Regex;
use uuid::Uuid;

use super::dav::{self, Buckets, header_str, present, send_with_etag, send_xml, tagged, with_etag, xml_escape};
use super::util::empty;
use crate::http::ClientIp;
use crate::state::AppState;

const MOUNT: &str = "/carddav";
const DAV_NS: &str = "DAV:";
const CARD_NS: &str = "urn:ietf:params:xml:ns:carddav";
const CDAV_NS: &str = "http://calendarserver.org/ns/";
const XML_HEADER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>"#;
const VCARD_TYPE: &str = "<D:getcontenttype>text/vcard;charset=utf-8</D:getcontenttype>";

static BUCKETS: LazyLock<Buckets> = LazyLock::new(Buckets::default);
static SYNC_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<(?:[A-Za-z0-9_.-]+:)?sync-token(?:\s[^>]*)?>([^<]*)</(?:[A-Za-z0-9_.-]+:)?sync-token>").unwrap()
});
static HREF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<(?:[A-Za-z0-9_.-]+:)?href(?:\s[^>]*)?>([^<]+)</(?:[A-Za-z0-9_.-]+:)?href>").unwrap()
});
static UP_TO_SLASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^.*/").unwrap());
static VCF_SUFFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\.vcf$").unwrap());

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/carddav", any(handle))
        .route("/carddav/", any(handle))
        .route("/carddav/{*rest}", any(handle))
        .route("/.well-known/carddav", any(dav::well_known_carddav))
        .route("/.well-known/carddav/", any(dav::well_known_carddav))
}

struct Book {
    id: Uuid,
    name: Option<String>,
    sync_token: Option<String>,
    sync_version: i64,
}

impl Book {
    fn from_row(r: &Row) -> Self {
        Book {
            id: r.get("id"),
            name: r.get("name"),
            sync_token: r.get("sync_token"),
            sync_version: r.try_get::<_, i64>("sync_version").unwrap_or(0),
        }
    }

    fn token(&self) -> String {
        format!("urn:inboxora:carddav:{}:{}", self.id, self.sync_version)
    }

    fn props(&self) -> Vec<String> {
        vec![
            "<D:resourcetype><D:collection/><C:addressbook/></D:resourcetype>".into(),
            format!("<D:displayname>{}</D:displayname>", xml_escape(self.name.as_deref())),
            format!("<D:sync-token>{}</D:sync-token>", xml_escape(Some(&self.token()))),
            format!("<CS:getctag>{}</CS:getctag>", xml_escape(self.sync_token.as_deref())),
        ]
    }
}

fn multistatus(responses: &[String]) -> String {
    format!(
        "{XML_HEADER}<D:multistatus xmlns:D=\"{DAV_NS}\" xmlns:C=\"{CARD_NS}\" xmlns:CS=\"{CDAV_NS}\">{}</D:multistatus>",
        responses.concat()
    )
}

fn response(href: &str, propstats: &[String]) -> String {
    format!("<D:response><D:href>{}</D:href>{}</D:response>", xml_escape(Some(href)), propstats.concat())
}

fn propstat(props: &[String], status: &str) -> String {
    format!("<D:propstat><D:prop>{}</D:prop><D:status>HTTP/1.1 {status}</D:status></D:propstat>", props.concat())
}

const NOT_FOUND_STATUS: &str = "<D:status>HTTP/1.1 404 Not Found</D:status>";

/// A card row from any of the listing queries (columns a query lacks read as absent).
struct Card {
    uid: Option<String>,
    dav_filename: Option<String>,
    etag: Option<String>,
    vcard: Option<String>,
    deleted: bool,
}

impl Card {
    fn from_row(r: &Row) -> Self {
        let text = |c: &str| r.try_get::<_, Option<String>>(c).ok().flatten();
        Card {
            uid: text("uid"),
            dav_filename: text("dav_filename"),
            etag: text("etag"),
            vcard: text("vcard"),
            deleted: r.try_get::<_, Option<bool>>("deleted").ok().flatten().unwrap_or(false),
        }
    }

    fn filename(&self) -> String {
        self.dav_filename
            .clone()
            .filter(|f| !f.is_empty())
            .unwrap_or_else(|| format!("{}.vcf", self.uid.as_deref().unwrap_or("undefined")))
    }
}

async fn handle(State(s): State<AppState>, ClientIp(ip): ClientIp, req: Request) -> Response {
    let method = req.method().clone();
    if let Some(limited) = BUCKETS.check(&ip) {
        return tagged(limited, &method, MOUNT);
    }
    let user_id = match dav::authenticate(&s, req.headers(), &ip, "Inboxora CardDAV", "carddav_auth_fail").await {
        Ok(u) => u,
        Err(rejection) => return tagged(rejection, &method, MOUNT),
    };
    let segs = match dav::segments(dav::rest_of(&req, MOUNT)) {
        Ok(s) => s,
        Err(e) => return tagged(e.into_response(), &method, "unmatched"),
    };
    let m = &method;
    let (pattern, result) = match segs.as_deref() {
        _ if dav::is(m, "OPTIONS") => ("/carddav*", Ok(options())),
        Some([]) if dav::is(m, "PROPFIND") => ("/carddav/", Ok(propfind_root(&user_id))),
        Some([u]) if dav::is(m, "PROPFIND") => {
            ("/carddav/:userId/", propfind_user(&s, &user_id, u, req.headers()).await)
        }
        Some([u, b]) if dav::is(m, "PROPFIND") => {
            ("/carddav/:userId/:bookId/", propfind_book(&s, &user_id, u, b, req.headers()).await)
        }
        Some([u, b]) if dav::is(m, "REPORT") => {
            let (u, b) = (u.clone(), b.clone());
            ("/carddav/:userId/:bookId/", report(&s, &user_id, &u, &b, req).await)
        }
        Some([u, b, f]) if dav::is(m, "GET") || dav::is(m, "HEAD") => {
            ("/carddav/:userId/:bookId/:filename", get_card(&s, &user_id, u, b, f).await)
        }
        Some([u, b, f]) if dav::is(m, "PUT") => {
            let (u, b, f) = (u.clone(), b.clone(), f.clone());
            ("/carddav/:userId/:bookId/:filename", Ok(put_card(&s, &user_id, &u, &b, &f, req).await))
        }
        Some([u, b, f]) if dav::is(m, "DELETE") => {
            ("/carddav/:userId/:bookId/:filename", Ok(delete_card(&s, &user_id, u, b, f, req.headers()).await))
        }
        _ => return tagged(crate::http::middleware::not_found(req).await, m, "unmatched"),
    };
    tagged(result.unwrap_or_else(IntoResponse::into_response), m, pattern)
}

fn options() -> Response {
    let mut res = empty(200);
    let h = res.headers_mut();
    h.insert(header::ALLOW, HeaderValue::from_static("OPTIONS, GET, PUT, DELETE, PROPFIND, REPORT"));
    h.insert("dav", HeaderValue::from_static("1, 2, 3, addressbook"));
    res
}

fn propfind_root(user_id: &str) -> Response {
    let principal = format!("/carddav/{user_id}/");
    send_xml(
        207,
        multistatus(&[response(
            "/carddav/",
            &[propstat(
                &[
                    "<D:resourcetype><D:collection/></D:resourcetype>".into(),
                    format!(
                        "<D:current-user-principal><D:href>{}</D:href></D:current-user-principal>",
                        xml_escape(Some(&principal))
                    ),
                ],
                "200 OK",
            )],
        )]),
    )
}

/// `req.headers.depth || '0'`.
fn depth_is_zero(headers: &HeaderMap) -> bool {
    header_str(headers, "depth").filter(|d| !d.is_empty()).unwrap_or("0") == "0"
}

async fn propfind_user(s: &AppState, user_id: &str, param: &str, headers: &HeaderMap) -> Result<Response> {
    if param != user_id {
        return Ok(empty(403));
    }
    let principal_path = format!("/carddav/{user_id}/");
    let rows =
        s.db.rows(
            "SELECT id, name, sync_token, sync_version FROM address_books WHERE user_id = $1 ORDER BY created_at",
            params![user_id],
        )
        .await?;
    let p = xml_escape(Some(&principal_path));
    let mut out = vec![response(
        &principal_path,
        &[propstat(
            &[
                "<D:resourcetype><D:principal/><D:collection/></D:resourcetype>".into(),
                format!("<D:displayname>{}</D:displayname>", xml_escape(Some(user_id))),
                format!("<D:principal-URL><D:href>{p}</D:href></D:principal-URL>"),
                format!("<C:addressbook-home-set><D:href>{p}</D:href></C:addressbook-home-set>"),
                format!("<D:current-user-principal><D:href>{p}</D:href></D:current-user-principal>"),
            ],
            "200 OK",
        )],
    )];
    if !depth_is_zero(headers) {
        for book in rows.iter().map(Book::from_row) {
            out.push(response(&format!("/carddav/{user_id}/{}/", book.id), &[propstat(&book.props(), "200 OK")]));
        }
    }
    Ok(send_xml(207, multistatus(&out)))
}

async fn load_book(s: &AppState, book_id: &str, user_id: &str) -> Result<Option<Book>> {
    let row = s.db.opt("SELECT * FROM address_books WHERE id = $1 AND user_id = $2", params![book_id, user_id]).await?;
    Ok(row.as_ref().map(Book::from_row))
}

fn card_props(c: &Card, with_data: bool) -> Vec<String> {
    let mut props = vec![
        "<D:resourcetype/>".to_owned(),
        format!("<D:getetag>\"{}\"</D:getetag>", xml_escape(c.etag.as_deref())),
        VCARD_TYPE.to_owned(),
    ];
    if with_data {
        props.push(format!("<C:address-data>{}</C:address-data>", xml_escape(c.vcard.as_deref())));
    }
    props
}

async fn propfind_book(s: &AppState, user_id: &str, u: &str, b: &str, headers: &HeaderMap) -> Result<Response> {
    if u != user_id {
        return Ok(empty(403));
    }
    let Some(book) = load_book(s, b, user_id).await? else { return Ok(empty(404)) };
    let book_path = format!("/carddav/{user_id}/{}/", book.id);
    let mut out = vec![response(&book_path, &[propstat(&book.props(), "200 OK")])];
    if !depth_is_zero(headers) {
        let rows =
            s.db.rows("SELECT uid, dav_filename, etag FROM contacts WHERE address_book_id = $1", params![book.id])
                .await?;
        for c in rows.iter().map(Card::from_row) {
            let href = format!("{book_path}{}", encode_uri_component(&c.filename()));
            out.push(response(&href, &[propstat(&card_props(&c, false), "200 OK")]));
        }
    }
    Ok(send_xml(207, multistatus(&out)))
}

/// A sync-token version that passes `Number.isSafeInteger`.
fn safe_version(digits: &str) -> Option<i64> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v: f64 = digits.parse().ok()?;
    (v <= 9_007_199_254_740_991.0).then_some(v as i64)
}

fn invalid_sync_token() -> Response {
    send_xml(409, format!("{XML_HEADER}<D:error xmlns:D=\"{DAV_NS}\"><D:valid-sync-token/></D:error>"))
}

async fn report(s: &AppState, user_id: &str, u: &str, b: &str, req: Request) -> Result<Response> {
    if u != user_id {
        return Ok(empty(403));
    }
    let Some(book) = load_book(s, b, user_id).await? else { return Ok(empty(404)) };
    let book_path = format!("/carddav/{user_id}/{}/", book.id);
    let body = match dav::raw_body(req).await {
        Ok(b) => b,
        Err(r) => return Ok(r),
    };
    let is_sync = body.contains("sync-collection");
    let is_multiget = body.contains("addressbook-multiget");
    if !is_sync && !is_multiget && !body.contains("addressbook-query") {
        return Ok(empty(400));
    }
    let mut cards: Option<Vec<Card>> = None;
    let mut filenames: Vec<String> = Vec::new();
    if is_sync {
        let token = SYNC_TOKEN.captures(&body).map(|c| js_trim(c.get(1).map_or("", |m| m.as_str())).to_owned());
        if let Some(token) = token.filter(|t| !t.is_empty()) {
            let prefix = format!("urn:inboxora:carddav:{}:", book.id);
            let version = token.strip_prefix(&prefix).and_then(safe_version);
            let Some(version) = version.filter(|v| *v <= book.sync_version) else { return Ok(invalid_sync_token()) };
            let rows =
                s.db.rows(
                    "SELECT DISTINCT ON (filename) filename AS dav_filename, etag, vcard, deleted \
                     FROM contact_sync_changes WHERE address_book_id = $1 AND version > $2 AND version <= $3 \
                     ORDER BY filename, version DESC",
                    params![book.id, version, book.sync_version],
                )
                .await?;
            cards = Some(rows.iter().map(Card::from_row).collect());
        }
    } else if is_multiget {
        let mut seen = HashSet::new();
        for c in HREF.captures_iter(&body) {
            let href = js_trim(&c[1]);
            let last = UP_TO_SLASH.replace(href, "");
            let Some(name) = dav::decode_uri_component(&last) else { return Ok(empty(400)) };
            if seen.insert(name.clone()) {
                filenames.push(name);
            }
        }
        if filenames.is_empty() {
            return Ok(empty(400));
        }
        let rows = s
            .db
            .rows(
                "SELECT uid, dav_filename, vcard, etag FROM contacts WHERE address_book_id = $1 AND COALESCE(dav_filename, uid || '.vcf') = ANY($2)",
                &[Val::from(book.id), Val::from(filenames.clone())],
            )
            .await?;
        cards = Some(rows.iter().map(Card::from_row).collect());
    }
    let cards = match cards {
        Some(c) => c,
        None => s
            .db
            .rows("SELECT uid, dav_filename, vcard, etag FROM contacts WHERE address_book_id = $1", params![book.id])
            .await?
            .iter()
            .map(Card::from_row)
            .collect(),
    };
    let mut out: Vec<String> = cards
        .iter()
        .map(|c| {
            let href = format!("{book_path}{}", encode_uri_component(&c.filename()));
            if c.deleted {
                response(&href, &[NOT_FOUND_STATUS.to_owned()])
            } else {
                response(&href, &[propstat(&card_props(c, true), "200 OK")])
            }
        })
        .collect();
    let returned: HashSet<String> = cards.iter().map(Card::filename).collect();
    for name in filenames.iter().filter(|n| !returned.contains(*n)) {
        out.push(response(&format!("{book_path}{}", encode_uri_component(name)), &[NOT_FOUND_STATUS.to_owned()]));
    }
    if is_sync {
        return Ok(send_xml(
            207,
            format!(
                "{XML_HEADER}<D:multistatus xmlns:D=\"{DAV_NS}\" xmlns:C=\"{CARD_NS}\">{}<D:sync-token>{}</D:sync-token></D:multistatus>",
                out.concat(),
                xml_escape(Some(&book.token()))
            ),
        ));
    }
    Ok(send_xml(207, multistatus(&out)))
}

async fn get_card(s: &AppState, user_id: &str, u: &str, b: &str, filename: &str) -> Result<Response> {
    if u != user_id {
        return Ok(empty(403));
    }
    let row =
        s.db.opt(
            "SELECT c.vcard, c.etag FROM contacts c JOIN address_books ab ON ab.id = c.address_book_id \
             WHERE ab.id = $1 AND ab.user_id = $2 AND COALESCE(c.dav_filename, c.uid || '.vcf') = $3",
            params![b, user_id, filename],
        )
        .await?;
    let Some(row) = row else { return Ok(empty(404)) };
    let etag: Option<String> = row.get("etag");
    Ok(send_with_etag("text/vcard; charset=utf-8", etag.as_deref(), row.get("vcard")))
}

async fn bump_book(s: &AppState, book_id: Uuid) -> Result<()> {
    s.db.exec(
        "UPDATE address_books SET sync_token = gen_random_uuid()::text, updated_at = NOW() WHERE id = $1",
        params![book_id],
    )
    .await?;
    Ok(())
}

fn to_json<T: serde::Serialize>(v: &T) -> Val {
    Val::from(inboxora_core::json::stringify(&serde_json::to_value(v).unwrap_or_default()))
}

async fn put_card(s: &AppState, user_id: &str, u: &str, b: &str, filename: &str, req: Request) -> Response {
    if u != user_id {
        return empty(403);
    }
    let headers = req.headers().clone();
    let body = match dav::raw_body(req).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    if js_trim(&body).is_empty() {
        return empty(400);
    }
    let parsed = inboxora_pim::vcard::parse(&body);
    if !parsed.invalid_dates.is_empty() || !parsed.invalid_date_labels.is_empty() {
        return empty(400);
    }
    let uid =
        parsed.uid.clone().filter(|u| !u.is_empty()).unwrap_or_else(|| VCF_SUFFIX.replace(filename, "").into_owned());
    let etag = crypto::md5_hex(&body);
    let primary_email = parsed
        .emails
        .iter()
        .find(|e| e.primary)
        .or_else(|| parsed.emails.first())
        .map(|e| e.value.to_lowercase())
        .filter(|e| !e.is_empty());
    let run = async {
        let Some(book) =
            s.db.opt("SELECT id, source FROM address_books WHERE id = $1 AND user_id = $2", params![b, user_id])
                .await?
        else {
            return Ok(empty(404));
        };
        if book.get::<_, Option<String>>("source").as_deref() != Some("local") {
            return Ok(empty(403));
        }
        let book_id: Uuid = book.get("id");
        let existing = s
            .db
            .rows(
                "SELECT id, uid, dav_filename, etag FROM contacts WHERE address_book_id = $1 AND (uid = $2 OR COALESCE(dav_filename, uid || '.vcf') = $3)",
                params![book_id, &uid, filename],
            )
            .await?;
        let current = existing.first().map(|r| {
            let id: Uuid = r.get("id");
            (id, Card::from_row(r))
        });
        if let Some((_, c)) = &current
            && let Some(cur_uid) = c.uid.as_deref().filter(|u| !u.is_empty())
            && (cur_uid != uid || c.filename() != filename)
        {
            return Ok(empty(409));
        }
        if header_str(&headers, "if-none-match") == Some("*") && current.is_some() {
            return Ok(empty(412));
        }
        let if_match = present(&headers, "if-match");
        if if_match.is_some() && current.is_none() {
            return Ok(empty(412));
        }
        let common = |p: &mut Vec<Val>| {
            p.extend([
                Val::from(parsed.display_name.clone()),
                Val::from(parsed.first_name.clone()),
                Val::from(parsed.last_name.clone()),
                Val::from(primary_email.clone()),
                to_json(&parsed.emails),
                to_json(&parsed.phones),
                Val::from(parsed.organization.clone()),
                Val::from(parsed.notes.clone()),
                Val::from(parsed.birthday.clone()),
                Val::from(parsed.anniversary.clone()),
                to_json(&parsed.contact_dates),
                Val::from(parsed.photo_data.clone()),
            ]);
        };
        let rich = |p: &mut Vec<Val>| {
            p.extend([
                Val::from(parsed.title.clone()),
                Val::from(parsed.role.clone()),
                Val::from(parsed.nickname.clone()),
                to_json(&parsed.urls),
                to_json(&parsed.instant_messages),
                to_json(&parsed.categories),
                to_json(&parsed.addresses),
                Val::from(filename),
            ]);
        };
        if let Some((id, cur)) = &current {
            if let Some(m) = if_match.filter(|m| *m != "*") {
                let client = m.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(m);
                if cur.etag.as_deref() != Some(client) {
                    return Ok(empty(412));
                }
            }
            let mut p = vec![Val::from(&body), Val::from(&etag)];
            common(&mut p);
            p.push(Val::from(*id));
            rich(&mut p);
            p.push(Val::from(cur.etag.clone()));
            let updated = s
                .db
                .rows(
                    "UPDATE contacts SET vcard = $1, etag = $2, display_name = $3, first_name = $4, last_name = $5, \
                     primary_email = $6, emails = $7, phones = $8, organization = $9, notes = $10, birthday = $11, \
                     anniversary = $12, contact_dates = $13::jsonb, photo_data = $14, title = $16, role = $17, nickname = $18, \
                     urls = $19::jsonb, instant_messages = $20::jsonb, categories = $21::jsonb, addresses = $22::jsonb, \
                     dav_filename = $23, is_auto = false, updated_at = NOW() WHERE id = $15 AND etag = $24 RETURNING id",
                    &p,
                )
                .await?;
            if updated.is_empty() {
                return Ok(empty(412));
            }
            bump_book(s, book_id).await?;
            Ok::<_, Error>(with_etag(204, &etag))
        } else {
            let mut p =
                vec![Val::from(book_id), Val::from(user_id), Val::from(&uid), Val::from(&body), Val::from(&etag)];
            common(&mut p);
            rich(&mut p);
            s.db
                .exec(
                    "INSERT INTO contacts (address_book_id, user_id, uid, vcard, etag, display_name, first_name, last_name, \
                     primary_email, emails, phones, organization, notes, birthday, anniversary, contact_dates, photo_data, \
                     title, role, nickname, urls, instant_messages, categories, addresses, dav_filename, is_auto) \
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16::jsonb,$17,$18,$19,$20,$21::jsonb,$22::jsonb,$23::jsonb,$24::jsonb,$25, false)",
                    &p,
                )
                .await?;
            bump_book(s, book_id).await?;
            Ok(with_etag(201, &etag))
        }
    };
    match run.await {
        Ok(r) => r,
        Err(e) if e.is_unique_violation() => empty(409),
        Err(e) => {
            tracing::error!("CardDAV PUT error: {e}");
            empty(500)
        }
    }
}

async fn delete_card(s: &AppState, user_id: &str, u: &str, b: &str, filename: &str, headers: &HeaderMap) -> Response {
    if u != user_id {
        return empty(403);
    }
    let if_match = present(headers, "if-match");
    let run = async {
        let Some(book) =
            s.db.opt("SELECT id, source FROM address_books WHERE id = $1 AND user_id = $2", params![b, user_id])
                .await?
        else {
            return Ok(empty(404));
        };
        if book.get::<_, Option<String>>("source").as_deref() != Some("local") {
            return Ok(empty(403));
        }
        // `replace(/^"|"$/g, '')`: each quote is stripped independently.
        let expected = if_match.filter(|m| *m != "*").map(|m| {
            let m = m.strip_prefix('"').unwrap_or(m);
            m.strip_suffix('"').unwrap_or(m).to_owned()
        });
        let row =
            s.db.opt(
                "DELETE FROM contacts USING address_books WHERE contacts.address_book_id = address_books.id \
                 AND address_books.id = $1 AND address_books.user_id = $2 \
                 AND COALESCE(contacts.dav_filename, contacts.uid || '.vcf') = $3 \
                 AND ($4::text IS NULL OR contacts.etag = $4) RETURNING address_books.id AS book_id",
                params![b, user_id, filename, expected],
            )
            .await?;
        let Some(row) = row else { return Ok(empty(if if_match.is_some() { 412 } else { 404 })) };
        bump_book(s, row.get("book_id")).await?;
        Ok::<_, Error>(empty(204))
    };
    match run.await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("CardDAV DELETE error: {e}");
            empty(500)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_token_versions_must_be_safe_integers() {
        assert_eq!(safe_version("12"), Some(12));
        assert_eq!(safe_version("9007199254740991"), Some(9_007_199_254_740_991));
        assert_eq!(safe_version("9007199254740993"), None);
        assert_eq!(safe_version("1e3"), None);
        assert_eq!(safe_version(""), None);
    }

    #[test]
    fn report_patterns_match_node() {
        let body = "<D:sync-collection xmlns:D=\"DAV:\"><D:sync-token> urn:x:1 </D:sync-token></D:sync-collection>";
        assert_eq!(&SYNC_TOKEN.captures(body).unwrap()[1], " urn:x:1 ");
        let hrefs: Vec<String> = HREF
            .captures_iter("<href>/a/b/c%20d.vcf</href><x:href a=\"1\">e.vcf</x:href>")
            .map(|c| c[1].to_owned())
            .collect();
        assert_eq!(hrefs, ["/a/b/c%20d.vcf", "e.vcf"]);
        assert_eq!(UP_TO_SLASH.replace("/a/b/c.vcf", ""), "c.vcf");
        assert_eq!(VCF_SUFFIX.replace("x.VCF", ""), "x");
    }
}
