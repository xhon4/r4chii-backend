use app_core::{new_id, Uuid};
use chrono::{DateTime, Utc};

use crate::{channel, server_role, PgPool};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileLinkInput {
    pub label: String,
    pub url: String,
}

impl ProfileLinkInput {
    pub fn new(label: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            url: url.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ProfileLinkRow {
    pub id: Uuid,
    pub label: String,
    pub url: String,
    pub position: i16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRow {
    pub id: Uuid,
    pub username: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub bio: Option<String>,
    pub banner_url: Option<String>,
    pub accent_color: Option<String>,
    pub pronouns: Option<String>,
    pub created_at: DateTime<Utc>,
    pub status: String,
    pub custom_status: Option<String>,
    pub custom_emoji: Option<String>,
    pub custom_expires_at: Option<DateTime<Utc>>,
    pub theme: String,
    pub vis_bio: String,
    pub vis_communities: String,
    pub vis_friends: String,
    pub deleted_at: Option<DateTime<Utc>>,
    pub links: Vec<ProfileLinkRow>,
}

#[derive(Debug, Clone)]
pub struct ServerContextRow {
    pub nickname: Option<String>,
    pub joined_at: DateTime<Utc>,
    pub roles: Vec<server_role::ServerRoleRow>,
}

#[derive(sqlx::FromRow)]
struct ProfileWithLinkRow {
    request_position: i64,
    id: Uuid,
    username: String,
    display_name: String,
    avatar_url: Option<String>,
    bio: Option<String>,
    banner_url: Option<String>,
    accent_color: Option<String>,
    pronouns: Option<String>,
    created_at: DateTime<Utc>,
    status: String,
    custom_status: Option<String>,
    custom_emoji: Option<String>,
    custom_expires_at: Option<DateTime<Utc>>,
    theme: String,
    vis_bio: String,
    vis_communities: String,
    vis_friends: String,
    deleted_at: Option<DateTime<Utc>>,
    link_id: Option<Uuid>,
    link_label: Option<String>,
    link_url: Option<String>,
    link_position: Option<i16>,
}

/// Reads all requested profiles and their ordered links in one query. The
/// caller owns the API's 100-id bound; this repository operation keeps the
/// set-based query valid for any slice supplied by an internal caller.
pub async fn get_profiles_bulk(
    pool: &PgPool,
    account_ids: &[Uuid],
) -> Result<Vec<ProfileRow>, sqlx::Error> {
    let rows = sqlx::query_as::<_, ProfileWithLinkRow>(
        "WITH requested AS ( \
             SELECT id, ordinality AS request_position \
             FROM unnest($1::uuid[]) WITH ORDINALITY AS ids(id, ordinality) \
         ) \
         SELECT requested.request_position, \
                a.id, a.username, a.display_name, a.avatar_url, a.bio, a.banner_url, \
                a.accent_color, a.pronouns, a.created_at, a.status::text AS status, \
                a.custom_status, a.custom_emoji, a.custom_expires_at, a.theme::text AS theme, \
                a.vis_bio::text AS vis_bio, a.vis_communities::text AS vis_communities, \
                a.vis_friends::text AS vis_friends, a.deleted_at, \
                link.id AS link_id, link.label AS link_label, link.url AS link_url, \
                link.position AS link_position \
         FROM requested \
         JOIN account a ON a.id = requested.id \
         LEFT JOIN account_profile_link link ON link.account_id = a.id \
         ORDER BY requested.request_position, link.position",
    )
    .bind(account_ids)
    .fetch_all(pool)
    .await?;

    let mut profiles = Vec::new();
    let mut current_request_position = None;

    for row in rows {
        if current_request_position != Some(row.request_position) {
            current_request_position = Some(row.request_position);
            profiles.push(ProfileRow {
                id: row.id,
                username: row.username,
                display_name: row.display_name,
                avatar_url: row.avatar_url,
                bio: row.bio,
                banner_url: row.banner_url,
                accent_color: row.accent_color,
                pronouns: row.pronouns,
                created_at: row.created_at,
                status: row.status,
                custom_status: row.custom_status,
                custom_emoji: row.custom_emoji,
                custom_expires_at: row.custom_expires_at,
                theme: row.theme,
                vis_bio: row.vis_bio,
                vis_communities: row.vis_communities,
                vis_friends: row.vis_friends,
                deleted_at: row.deleted_at,
                links: Vec::new(),
            });
        }

        if let (Some(id), Some(label), Some(url), Some(position)) =
            (row.link_id, row.link_label, row.link_url, row.link_position)
        {
            if let Some(profile) = profiles.last_mut() {
                profile.links.push(ProfileLinkRow {
                    id,
                    label,
                    url,
                    position,
                });
            }
        }
    }

    Ok(profiles)
}

/// Reads one account's ordered profile links.
pub async fn list_links<'e, E>(
    executor: E,
    account_id: Uuid,
) -> Result<Vec<ProfileLinkRow>, sqlx::Error>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_as::<_, ProfileLinkRow>(
        "SELECT id, label, url, position FROM account_profile_link \
         WHERE account_id = $1 ORDER BY position",
    )
    .bind(account_id)
    .fetch_all(executor)
    .await
}

/// Replaces an account's complete ordered link set. Link ids are generated in
/// Rust before the delete and insert. The caller supplies the connection, so
/// the replacement commits with whatever else its transaction carries.
pub async fn replace_links(
    conn: &mut sqlx::PgConnection,
    account_id: Uuid,
    links: &[ProfileLinkInput],
) -> Result<(), sqlx::Error> {
    let ids: Vec<Uuid> = links.iter().map(|_| new_id()).collect();
    let labels: Vec<&str> = links.iter().map(|link| link.label.as_str()).collect();
    let urls: Vec<&str> = links.iter().map(|link| link.url.as_str()).collect();

    sqlx::query_scalar::<_, Uuid>("SELECT id FROM account WHERE id = $1 FOR KEY SHARE")
        .bind(account_id)
        .fetch_one(&mut *conn)
        .await?;

    sqlx::query("DELETE FROM account_profile_link WHERE account_id = $1")
        .bind(account_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO account_profile_link (id, account_id, label, url, position) \
         SELECT id, $1, label, url, (ordinality - 1)::smallint \
         FROM unnest($2::uuid[], $3::text[], $4::text[]) WITH ORDINALITY \
              AS links(id, label, url, ordinality)",
    )
    .bind(account_id)
    .bind(ids)
    .bind(labels)
    .bind(urls)
    .execute(&mut *conn)
    .await?;

    Ok(())
}

/// Resolves an account id from a username, compared against the normalized
/// column that carries the unique index. Casing and compatibility forms
/// resolve to the same account; the input is normalized by the same two
/// functions that generate the column, so there is one implementation.
pub async fn find_id_by_username(
    pool: &PgPool,
    username: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM account WHERE username_normalized = lower(normalize($1, NFKC))",
    )
    .bind(username)
    .fetch_optional(pool)
    .await
}

/// Resolves per-server profile context from the existing membership and role
/// assignment repositories. A missing membership has no server context.
pub async fn get_server_context(
    pool: &PgPool,
    server_id: Uuid,
    account_id: Uuid,
) -> Result<Option<ServerContextRow>, sqlx::Error> {
    let Some(membership) = channel::find_membership(pool, server_id, account_id).await? else {
        return Ok(None);
    };
    let mut roles = server_role::roles_for_membership(pool, server_id, membership.id).await?;
    roles.sort_by(|left, right| {
        right
            .position
            .cmp(&left.position)
            .then_with(|| left.id.cmp(&right.id))
    });

    Ok(Some(ServerContextRow {
        nickname: membership.nickname,
        joined_at: membership.joined_at,
        roles,
    }))
}
