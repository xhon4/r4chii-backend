use app_core::Uuid;
use chrono::{DateTime, Utc};
use sqlx::PgExecutor;

#[derive(sqlx::FromRow)]
pub struct MessageRow {
    pub id: Uuid,
    pub channel_id: Uuid,
    pub author_account_id: Uuid,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub edited_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
    /// `None` = not pinned.
    pub pinned_at: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
pub struct MessageOwnerRow {
    pub author_account_id: Uuid,
    pub deleted_at: Option<DateTime<Utc>>,
}

/// `search_vector` is computed inline via `to_tsvector` at write
/// time rather than a `GENERATED` column — this project's Postgres target is
/// 11 (`0006_server_roles.sql`'s own `gen_random_uuid()` avoidance note),
/// which predates `GENERATED ALWAYS AS (...) STORED` (PG12+).
pub async fn insert(
    executor: impl PgExecutor<'_>,
    id: Uuid,
    channel_id: Uuid,
    author_account_id: Uuid,
    content: &str,
) -> Result<MessageRow, sqlx::Error> {
    sqlx::query_as::<_, MessageRow>(
        "INSERT INTO message (id, channel_id, author_account_id, content, search_vector) \
         VALUES ($1, $2, $3, $4, to_tsvector('english', $4)) \
         RETURNING id, channel_id, author_account_id, content, created_at, edited_at, deleted_at, pinned_at",
    )
    .bind(id)
    .bind(channel_id)
    .bind(author_account_id)
    .bind(content)
    .fetch_one(executor)
    .await
}

/// Refreshes `search_vector` alongside `content` — an edited
/// message must stay findable by its NEW content, not its stale one.
pub async fn update_content(
    executor: impl PgExecutor<'_>,
    message_id: Uuid,
    channel_id: Uuid,
    content: &str,
) -> Result<Option<MessageRow>, sqlx::Error> {
    sqlx::query_as::<_, MessageRow>(
        "UPDATE message SET content = $1, edited_at = now(), \
                             search_vector = to_tsvector('english', $1) \
         WHERE id = $2 AND channel_id = $3 \
         RETURNING id, channel_id, author_account_id, content, created_at, edited_at, deleted_at, pinned_at",
    )
    .bind(content)
    .bind(message_id)
    .bind(channel_id)
    .fetch_optional(executor)
    .await
}

/// Soft-delete only — the row stays ("M0 delete is author
/// soft-delete").
pub async fn soft_delete(
    executor: impl PgExecutor<'_>,
    message_id: Uuid,
    channel_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE message SET deleted_at = now() WHERE id = $1 AND channel_id = $2")
        .bind(message_id)
        .bind(channel_id)
        .execute(executor)
        .await
        .map(|_| ())
}

/// Newest-first page, `before` pages backward in time
/// (the frozen cursor pagination convention used throughout this API).
/// Soft-deleted
/// rows are NOT filtered out here — only the caller's row-to-summary
/// mapping nulls their content.
pub async fn list_page(
    executor: impl PgExecutor<'_>,
    channel_id: Uuid,
    before: Option<Uuid>,
    limit: i64,
) -> Result<Vec<MessageRow>, sqlx::Error> {
    sqlx::query_as::<_, MessageRow>(
        "SELECT id, channel_id, author_account_id, content, created_at, edited_at, deleted_at, pinned_at \
         FROM message \
         WHERE channel_id = $1 AND ($2::uuid IS NULL OR id < $2) \
         ORDER BY id DESC \
         LIMIT $3",
    )
    .bind(channel_id)
    .bind(before)
    .bind(limit)
    .fetch_all(executor)
    .await
}

/// Every non-deleted message in a channel, oldest first, no page cap — the
/// full-dump query behind a server export. Deliberately not
/// `list_page`: an export needs everything in one pass, not a cursor page,
/// and soft-deleted rows are excluded outright — the export's own rule is
/// that soft-deleted messages are excluded — unlike `list_page`'s own choice
/// to keep them with nulled content for the live chat view.
pub async fn list_all_for_export(
    executor: impl PgExecutor<'_>,
    channel_id: Uuid,
) -> Result<Vec<MessageRow>, sqlx::Error> {
    sqlx::query_as::<_, MessageRow>(
        "SELECT id, channel_id, author_account_id, content, created_at, edited_at, deleted_at, pinned_at \
         FROM message \
         WHERE channel_id = $1 AND deleted_at IS NULL \
         ORDER BY id ASC",
    )
    .bind(channel_id)
    .fetch_all(executor)
    .await
}

/// Batched form of `list_all_for_export` — every non-deleted message across
/// ANY of `channel_ids`, ordered by channel then id, in one query instead of
/// one per channel/thread. `DomainService::build_and_upload_export`'s own way
/// of fetching every message in a server (top-level channels and their
/// threads alike) in one round trip; the caller groups rows by
/// `channel_id` in Rust.
pub async fn list_all_for_export_batch(
    executor: impl PgExecutor<'_>,
    channel_ids: &[Uuid],
) -> Result<Vec<MessageRow>, sqlx::Error> {
    if channel_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, MessageRow>(
        "SELECT id, channel_id, author_account_id, content, created_at, edited_at, deleted_at, pinned_at \
         FROM message \
         WHERE channel_id = ANY($1) AND deleted_at IS NULL \
         ORDER BY channel_id, id ASC",
    )
    .bind(channel_ids)
    .fetch_all(executor)
    .await
}

/// One message as rendered on the public read path — author
/// DISPLAY NAME already joined in (the public page has no client-side
/// account cache to resolve an id against, unlike the SPA), content only
/// (no `edited_at`/`deleted_at` — soft-deleted rows are excluded outright
/// below, same choice search already makes for the same reason: a
/// deleted message has nothing left worth showing an anonymous reader).
#[derive(sqlx::FromRow)]
pub struct PublicMessageRow {
    pub id: Uuid,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub author_display_name: String,
}

/// Oldest-first page of a channel/thread's messages for the public read
/// path — the order an anonymous reader actually reads top to bottom in,
/// unlike the SPA's newest-first chat view. `after` (not `before`) pages
/// FORWARD in time, since ascending order's cursor moves the other
/// direction from `list_page`'s descending one.
pub async fn list_page_ascending_with_author(
    executor: impl PgExecutor<'_>,
    channel_id: Uuid,
    after: Option<Uuid>,
    limit: i64,
) -> Result<Vec<PublicMessageRow>, sqlx::Error> {
    sqlx::query_as::<_, PublicMessageRow>(
        "SELECT m.id, m.content, m.created_at, a.display_name AS author_display_name \
         FROM message m \
         JOIN account a ON a.id = m.author_account_id \
         WHERE m.channel_id = $1 AND m.deleted_at IS NULL \
         AND ($2::uuid IS NULL OR m.id > $2) \
         ORDER BY m.id ASC \
         LIMIT $3",
    )
    .bind(channel_id)
    .bind(after)
    .bind(limit)
    .fetch_all(executor)
    .await
}

/// Full-text search across a server's messages, filterable by
/// author/channel/date. Every filter is a fixed, known-at-compile-time
/// column — not an arbitrary caller-built query — so this uses the same
/// `($n::type IS NULL OR ...)` conditional-bind trick `list_page`'s own
/// `before` cursor already established, rather than a query builder: the SQL
/// text never varies, only which bound values are `NULL`. Soft-deleted
/// messages are excluded outright (unlike `list_page`, which keeps them with
/// nulled content) — a deleted message has nothing left worth finding.
///
/// Ranked by `ts_rank` first, `id DESC` (recency) as the tiebreaker.
/// `before` pages by id like every other cursor here, applied after ranking
/// — an approximation for a ranked result set, not a true rank-ordered
/// keyset cursor, acceptable for v1 under a "no ranking tuning
/// without real query data" stance.
/// `bypass` is `true` for the server's owner or an `ADMIN` holder — skips the
/// restriction filter below entirely, same tier `DomainService::
/// member_can_view_channel` grants it everywhere else. `caller_role_ids`
/// (harmless if empty) is every role the caller holds, checked against
/// `channel_role_permission` for a restricted channel/thread's resolved
/// parent — the permission model's own resolution rule (OR across held
/// roles), applied
/// here so a restricted channel's messages can never surface in a plain
/// member's search results just because search is server-wide rather than
/// per-channel like every other read path.
#[allow(clippy::too_many_arguments)]
pub async fn search_in_server(
    executor: impl PgExecutor<'_>,
    server_id: Uuid,
    query: &str,
    author_account_id: Option<Uuid>,
    channel_id: Option<Uuid>,
    created_after: Option<DateTime<Utc>>,
    created_before: Option<DateTime<Utc>>,
    before: Option<Uuid>,
    limit: i64,
    bypass: bool,
    caller_role_ids: &[Uuid],
    view_channel_bit: i64,
) -> Result<Vec<MessageRow>, sqlx::Error> {
    sqlx::query_as::<_, MessageRow>(
        "SELECT m.id, m.channel_id, m.author_account_id, m.content, \
                m.created_at, m.edited_at, m.deleted_at, m.pinned_at \
         FROM message m \
         JOIN channel c ON c.id = m.channel_id \
         LEFT JOIN channel parent ON parent.id = c.parent_channel_id \
         WHERE c.server_id = $1 \
         AND m.deleted_at IS NULL \
         AND m.search_vector @@ websearch_to_tsquery('english', $2) \
         AND ($3::uuid IS NULL OR m.author_account_id = $3) \
         AND ($4::uuid IS NULL OR m.channel_id = $4) \
         AND ($5::timestamptz IS NULL OR m.created_at >= $5) \
         AND ($6::timestamptz IS NULL OR m.created_at <= $6) \
         AND ($7::uuid IS NULL OR m.id < $7) \
         AND ( \
             $9 \
             OR NOT coalesce(parent.restricted, c.restricted) \
             OR EXISTS( \
                 SELECT 1 FROM channel_role_permission crp \
                 WHERE crp.channel_id = coalesce(c.parent_channel_id, c.id) \
                 AND crp.role_id = ANY($10) \
                 AND crp.permissions & $11 != 0 \
             ) \
         ) \
         ORDER BY ts_rank(m.search_vector, websearch_to_tsquery('english', $2)) DESC, m.id DESC \
         LIMIT $8",
    )
    .bind(server_id)
    .bind(query)
    .bind(author_account_id)
    .bind(channel_id)
    .bind(created_after)
    .bind(created_before)
    .bind(before)
    .bind(limit)
    .bind(bypass)
    .bind(caller_role_ids)
    .bind(view_channel_bit)
    .fetch_all(executor)
    .await
}

pub async fn find_owner(
    executor: impl PgExecutor<'_>,
    message_id: Uuid,
    channel_id: Uuid,
) -> Result<Option<MessageOwnerRow>, sqlx::Error> {
    sqlx::query_as::<_, MessageOwnerRow>(
        "SELECT author_account_id, deleted_at FROM message WHERE id = $1 AND channel_id = $2",
    )
    .bind(message_id)
    .bind(channel_id)
    .fetch_optional(executor)
    .await
}

/// Sets `pinned_at`; a no-op (still returns the row) if it was
/// already pinned, same idempotent shape `PIN_MESSAGES` callers expect from a
/// "pin" action. Excludes soft-deleted messages — nothing left worth pinning.
pub async fn pin(
    executor: impl PgExecutor<'_>,
    message_id: Uuid,
    channel_id: Uuid,
) -> Result<Option<MessageRow>, sqlx::Error> {
    sqlx::query_as::<_, MessageRow>(
        "UPDATE message SET pinned_at = COALESCE(pinned_at, now()) \
         WHERE id = $1 AND channel_id = $2 AND deleted_at IS NULL \
         RETURNING id, channel_id, author_account_id, content, created_at, edited_at, deleted_at, pinned_at",
    )
    .bind(message_id)
    .bind(channel_id)
    .fetch_optional(executor)
    .await
}

/// Clears `pinned_at` — a no-op (still returns the row) if it
/// wasn't pinned.
pub async fn unpin(
    executor: impl PgExecutor<'_>,
    message_id: Uuid,
    channel_id: Uuid,
) -> Result<Option<MessageRow>, sqlx::Error> {
    sqlx::query_as::<_, MessageRow>(
        "UPDATE message SET pinned_at = NULL \
         WHERE id = $1 AND channel_id = $2 \
         RETURNING id, channel_id, author_account_id, content, created_at, edited_at, deleted_at, pinned_at",
    )
    .bind(message_id)
    .bind(channel_id)
    .fetch_optional(executor)
    .await
}

/// Every pinned, non-deleted message in a channel, newest-pinned
/// first — reading pins needs no permission bit (baseline, same tier as
/// reading messages themselves).
pub async fn list_pinned(
    executor: impl PgExecutor<'_>,
    channel_id: Uuid,
) -> Result<Vec<MessageRow>, sqlx::Error> {
    sqlx::query_as::<_, MessageRow>(
        "SELECT id, channel_id, author_account_id, content, created_at, edited_at, deleted_at, pinned_at \
         FROM message \
         WHERE channel_id = $1 AND pinned_at IS NOT NULL AND deleted_at IS NULL \
         ORDER BY pinned_at DESC",
    )
    .bind(channel_id)
    .fetch_all(executor)
    .await
}
