use app_core::Uuid;
use sqlx::PgExecutor;

use crate::channel::ChannelRow;

/// A dm/group_dm row plus its roster. A DM carries no `name`, so its
/// participants are the only thing a client can label it with — they are
/// selected alongside the channel rather than in a follow-up query per row.
///
/// The correlated `ARRAY(..)` is an index-only scan on
/// `channel_member (channel_id, account_id)`, and keeps `ChannelRow` usable
/// unchanged by every other channel query.
#[derive(sqlx::FromRow)]
pub struct DmChannelRow {
    #[sqlx(flatten)]
    pub channel: ChannelRow,
    pub participant_ids: Vec<Uuid>,
}

/// The 1:1 `dm` channel shared by exactly `account_a` and `account_b`, if one
/// already exists.
pub async fn find_existing_dm(
    executor: impl PgExecutor<'_>,
    account_a: Uuid,
    account_b: Uuid,
) -> Result<Option<ChannelRow>, sqlx::Error> {
    sqlx::query_as::<_, ChannelRow>(
        "SELECT c.id, c.server_id, c.kind, c.name, c.created_at, c.visibility, \
                c.parent_channel_id, c.root_message_id, c.title, c.slug, c.restricted \
         FROM channel c \
         WHERE c.kind = 'dm' \
         AND EXISTS (SELECT 1 FROM channel_member WHERE channel_id = c.id AND account_id = $1) \
         AND EXISTS (SELECT 1 FROM channel_member WHERE channel_id = c.id AND account_id = $2) \
         AND (SELECT COUNT(*) FROM channel_member WHERE channel_id = c.id) = 2 \
         LIMIT 1",
    )
    .bind(account_a)
    .bind(account_b)
    .fetch_optional(executor)
    .await
}

pub async fn insert_dm_channel(
    executor: impl PgExecutor<'_>,
    id: Uuid,
) -> Result<ChannelRow, sqlx::Error> {
    sqlx::query_as::<_, ChannelRow>(
        "INSERT INTO channel (id, kind) VALUES ($1, 'dm') \
         RETURNING id, server_id, kind, name, created_at, visibility, \
                   parent_channel_id, root_message_id, title, slug, restricted",
    )
    .bind(id)
    .fetch_one(executor)
    .await
}

pub async fn insert_group_dm_channel(
    executor: impl PgExecutor<'_>,
    id: Uuid,
) -> Result<ChannelRow, sqlx::Error> {
    sqlx::query_as::<_, ChannelRow>(
        "INSERT INTO channel (id, kind) VALUES ($1, 'group_dm') \
         RETURNING id, server_id, kind, name, created_at, visibility, \
                   parent_channel_id, root_message_id, title, slug, restricted",
    )
    .bind(id)
    .fetch_one(executor)
    .await
}

/// Inserts exactly the two `channel_member` rows for a fresh 1:1 dm, in one
/// statement.
pub async fn insert_two_channel_members(
    executor: impl PgExecutor<'_>,
    member_id_a: Uuid,
    channel_id: Uuid,
    account_a: Uuid,
    member_id_b: Uuid,
    account_b: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO channel_member (id, channel_id, account_id) VALUES ($1, $2, $3), ($4, $2, $5)",
    )
    .bind(member_id_a)
    .bind(channel_id)
    .bind(account_a)
    .bind(member_id_b)
    .bind(account_b)
    .execute(executor)
    .await
    .map(|_| ())
}

pub async fn insert_channel_member(
    executor: impl PgExecutor<'_>,
    id: Uuid,
    channel_id: Uuid,
    account_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO channel_member (id, channel_id, account_id) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(channel_id)
        .bind(account_id)
        .execute(executor)
        .await
        .map(|_| ())
}

/// Every `dm`/`group_dm` channel `account_id` is a member of.
pub async fn list_for_account(
    executor: impl PgExecutor<'_>,
    account_id: Uuid,
) -> Result<Vec<DmChannelRow>, sqlx::Error> {
    sqlx::query_as::<_, DmChannelRow>(
        "SELECT c.id, c.server_id, c.kind, c.name, c.created_at, c.visibility, \
                c.parent_channel_id, c.root_message_id, c.title, c.slug, c.restricted, \
                ARRAY( \
                    SELECT m.account_id FROM channel_member m \
                    WHERE m.channel_id = c.id ORDER BY m.added_at \
                ) AS participant_ids \
         FROM channel c \
         JOIN channel_member cm ON cm.channel_id = c.id \
         WHERE cm.account_id = $1 AND c.kind IN ('dm', 'group_dm') \
         ORDER BY c.created_at ASC",
    )
    .bind(account_id)
    .fetch_all(executor)
    .await
}

/// Every `channel_member` of `channel_id`, oldest first.
pub async fn channel_member_ids(
    executor: impl PgExecutor<'_>,
    channel_id: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT account_id FROM channel_member WHERE channel_id = $1 ORDER BY added_at",
    )
    .bind(channel_id)
    .fetch_all(executor)
    .await
}

/// The other `channel_member` of `channel_id` besides `account_id` — used
/// only where a channel is known to be a 1:1 dm.
pub async fn other_channel_member(
    executor: impl PgExecutor<'_>,
    channel_id: Uuid,
    account_id: Uuid,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT account_id FROM channel_member WHERE channel_id = $1 AND account_id != $2",
    )
    .bind(channel_id)
    .bind(account_id)
    .fetch_optional(executor)
    .await
}
