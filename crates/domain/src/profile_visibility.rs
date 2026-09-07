/// The caller's ordinary relationship to the profile owner.
///
/// Directional block facts are modeled separately in [`ProfileVisibilityInput`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileViewerRelationship {
    SelfView,
    Friend,
    None,
}

/// A persisted profile field's visibility setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileVisibility {
    Public,
    Friends,
    Private,
}

impl ProfileVisibility {
    /// Converts a raw persisted value without widening access for malformed data.
    pub fn from_db(value: &str) -> Self {
        match value {
            "public" => Self::Public,
            "friends" => Self::Friends,
            "private" => Self::Private,
            _ => Self::Private,
        }
    }
}

/// All facts the visibility policy needs; database and realtime lookups happen outside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileVisibilityInput {
    pub relationship: ProfileViewerRelationship,
    /// Controls the grouped bio, pronouns, and links exposure.
    pub vis_bio: ProfileVisibility,
    /// Controls the separate communities exposure.
    pub vis_communities: ProfileVisibility,
    /// Controls the separate friends exposure.
    pub vis_friends: ProfileVisibility,
    /// The caller has blocked the profile owner.
    pub caller_blocked_owner: bool,
    /// The profile owner has blocked the caller.
    pub owner_blocked_caller: bool,
    /// The caller and owner share a server where server context is available.
    pub has_shared_server_context: bool,
    pub is_deleted: bool,
}

/// Whether the profile identity is rendered normally or as a deleted-account tombstone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileIdentityExposure {
    Visible,
    Tombstone,
}

/// Whether a response mapper retains profile media values or maps them to defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileMediaExposure {
    Actual,
    Default,
}

/// Whether a group of profile fields is included in the later response mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileFieldExposure {
    Visible,
    Hidden,
}

/// Whether realtime presence is usable or must be represented as offline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfilePresenceExposure {
    Real,
    ForcedOffline,
}

/// The relationship payload detail a response mapper may expose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileRelationshipExposure {
    SelfView,
    Friend,
    NoRelationship,
    Blocked,
    /// Minimal payload that does not disclose an inbound block, friendship, or block direction.
    Minimum,
    None,
}

/// Visibility decisions for a response mapper. No profile values are carried here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileVisibilityDecision {
    /// The sole source of truth for normal versus deleted-account identity exposure.
    pub identity: ProfileIdentityExposure,
    pub media: ProfileMediaExposure,
    /// Grouped exposure for bio, pronouns, and links.
    pub bio_pronouns_and_links: ProfileFieldExposure,
    /// Separate exposure for communities.
    pub communities: ProfileFieldExposure,
    /// Separate exposure for friends.
    pub friends: ProfileFieldExposure,
    pub custom_status: ProfileFieldExposure,
    pub presence: ProfilePresenceExposure,
    /// Per-server nickname and roles, gated only by shared membership and higher precedence masks.
    pub server_context: ProfileFieldExposure,
    pub relationship: ProfileRelationshipExposure,
}

impl ProfileVisibilityDecision {
    /// Combines the persisted presence preference with this decision's
    /// exposure. An `invisible` account resolves to `ForcedOffline` for every
    /// viewer except itself.
    pub fn presence_for_status(&self, status: &str) -> ProfilePresenceExposure {
        if self.relationship == ProfileRelationshipExposure::SelfView {
            return self.presence;
        }

        if status == "invisible" {
            ProfilePresenceExposure::ForcedOffline
        } else {
            self.presence
        }
    }
}

fn field_exposure(
    relationship: ProfileViewerRelationship,
    visibility: ProfileVisibility,
) -> ProfileFieldExposure {
    match relationship {
        ProfileViewerRelationship::SelfView => ProfileFieldExposure::Visible,
        ProfileViewerRelationship::Friend => match visibility {
            ProfileVisibility::Public | ProfileVisibility::Friends => ProfileFieldExposure::Visible,
            ProfileVisibility::Private => ProfileFieldExposure::Hidden,
        },
        ProfileViewerRelationship::None => match visibility {
            ProfileVisibility::Public => ProfileFieldExposure::Visible,
            ProfileVisibility::Friends | ProfileVisibility::Private => ProfileFieldExposure::Hidden,
        },
    }
}

fn deleted_profile_decision() -> ProfileVisibilityDecision {
    ProfileVisibilityDecision {
        identity: ProfileIdentityExposure::Tombstone,
        media: ProfileMediaExposure::Default,
        bio_pronouns_and_links: ProfileFieldExposure::Hidden,
        communities: ProfileFieldExposure::Hidden,
        friends: ProfileFieldExposure::Hidden,
        custom_status: ProfileFieldExposure::Hidden,
        presence: ProfilePresenceExposure::ForcedOffline,
        server_context: ProfileFieldExposure::Hidden,
        relationship: ProfileRelationshipExposure::None,
    }
}

/// Resolves profile visibility without fetching rows, constructing API DTOs, or mutating state.
///
/// A deleted account resolves to a tombstone and outranks every relationship fact. A block in
/// either direction degrades the pair to no relationship for the visibility-gated fields and
/// never redacts anything else; an inbound block additionally withholds its own direction.
pub fn decide_profile_visibility(input: ProfileVisibilityInput) -> ProfileVisibilityDecision {
    if input.is_deleted {
        return deleted_profile_decision();
    }

    let effective_relationship = if input.caller_blocked_owner || input.owner_blocked_caller {
        ProfileViewerRelationship::None
    } else {
        input.relationship
    };
    // An outbound block is checked first, so a mutual block reports `Blocked`
    // rather than `Minimum`.
    let relationship = if input.caller_blocked_owner {
        ProfileRelationshipExposure::Blocked
    } else if input.owner_blocked_caller {
        ProfileRelationshipExposure::Minimum
    } else {
        match input.relationship {
            ProfileViewerRelationship::SelfView => ProfileRelationshipExposure::SelfView,
            ProfileViewerRelationship::Friend => ProfileRelationshipExposure::Friend,
            ProfileViewerRelationship::None => ProfileRelationshipExposure::NoRelationship,
        }
    };

    ProfileVisibilityDecision {
        identity: ProfileIdentityExposure::Visible,
        media: ProfileMediaExposure::Actual,
        bio_pronouns_and_links: field_exposure(effective_relationship, input.vis_bio),
        communities: field_exposure(effective_relationship, input.vis_communities),
        friends: field_exposure(effective_relationship, input.vis_friends),
        custom_status: ProfileFieldExposure::Visible,
        presence: ProfilePresenceExposure::Real,
        server_context: if input.has_shared_server_context {
            ProfileFieldExposure::Visible
        } else {
            ProfileFieldExposure::Hidden
        },
        relationship,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        decide_profile_visibility, ProfileFieldExposure, ProfileIdentityExposure,
        ProfileMediaExposure, ProfilePresenceExposure, ProfileRelationshipExposure,
        ProfileViewerRelationship, ProfileVisibility, ProfileVisibilityInput,
    };

    #[derive(Clone, Copy)]
    enum VisibilityAxis {
        BioPronounsAndLinks,
        Communities,
        Friends,
    }

    fn input(relationship: ProfileViewerRelationship) -> ProfileVisibilityInput {
        ProfileVisibilityInput {
            relationship,
            vis_bio: ProfileVisibility::Private,
            vis_communities: ProfileVisibility::Private,
            vis_friends: ProfileVisibility::Private,
            caller_blocked_owner: false,
            owner_blocked_caller: false,
            has_shared_server_context: false,
            is_deleted: false,
        }
    }

    fn with_visibility(
        mut input: ProfileVisibilityInput,
        axis: VisibilityAxis,
        visibility: ProfileVisibility,
    ) -> ProfileVisibilityInput {
        match axis {
            VisibilityAxis::BioPronounsAndLinks => input.vis_bio = visibility,
            VisibilityAxis::Communities => input.vis_communities = visibility,
            VisibilityAxis::Friends => input.vis_friends = visibility,
        }
        input
    }

    fn exposure_for(
        decision: super::ProfileVisibilityDecision,
        axis: VisibilityAxis,
    ) -> ProfileFieldExposure {
        match axis {
            VisibilityAxis::BioPronounsAndLinks => decision.bio_pronouns_and_links,
            VisibilityAxis::Communities => decision.communities,
            VisibilityAxis::Friends => decision.friends,
        }
    }

    #[test]
    fn ordinary_relationship_visibility_matrix_covers_every_persisted_axis() {
        let axes = [
            VisibilityAxis::BioPronounsAndLinks,
            VisibilityAxis::Communities,
            VisibilityAxis::Friends,
        ];
        let cases = [
            (
                ProfileViewerRelationship::SelfView,
                ProfileVisibility::Public,
                ProfileFieldExposure::Visible,
            ),
            (
                ProfileViewerRelationship::SelfView,
                ProfileVisibility::Friends,
                ProfileFieldExposure::Visible,
            ),
            (
                ProfileViewerRelationship::SelfView,
                ProfileVisibility::Private,
                ProfileFieldExposure::Visible,
            ),
            (
                ProfileViewerRelationship::Friend,
                ProfileVisibility::Public,
                ProfileFieldExposure::Visible,
            ),
            (
                ProfileViewerRelationship::Friend,
                ProfileVisibility::Friends,
                ProfileFieldExposure::Visible,
            ),
            (
                ProfileViewerRelationship::Friend,
                ProfileVisibility::Private,
                ProfileFieldExposure::Hidden,
            ),
            (
                ProfileViewerRelationship::None,
                ProfileVisibility::Public,
                ProfileFieldExposure::Visible,
            ),
            (
                ProfileViewerRelationship::None,
                ProfileVisibility::Friends,
                ProfileFieldExposure::Hidden,
            ),
            (
                ProfileViewerRelationship::None,
                ProfileVisibility::Private,
                ProfileFieldExposure::Hidden,
            ),
        ];

        for axis in axes {
            for (relationship, visibility, expected) in cases {
                let decision = decide_profile_visibility(with_visibility(
                    input(relationship),
                    axis,
                    visibility,
                ));
                assert_eq!(exposure_for(decision, axis), expected);
            }
        }
    }

    #[test]
    fn an_inbound_block_withholds_its_direction_without_redacting_readable_data() {
        let mut policy_input = input(ProfileViewerRelationship::None);
        policy_input.vis_bio = ProfileVisibility::Public;
        policy_input.vis_communities = ProfileVisibility::Public;
        policy_input.vis_friends = ProfileVisibility::Public;
        policy_input.owner_blocked_caller = true;
        policy_input.has_shared_server_context = true;

        let decision = decide_profile_visibility(policy_input);

        assert_eq!(decision.identity, ProfileIdentityExposure::Visible);
        assert_eq!(decision.media, ProfileMediaExposure::Actual);
        assert_eq!(
            decision.bio_pronouns_and_links,
            ProfileFieldExposure::Visible
        );
        assert_eq!(decision.communities, ProfileFieldExposure::Visible);
        assert_eq!(decision.friends, ProfileFieldExposure::Visible);
        assert_eq!(decision.custom_status, ProfileFieldExposure::Visible);
        assert_eq!(decision.presence, ProfilePresenceExposure::Real);
        assert_eq!(decision.server_context, ProfileFieldExposure::Visible);
        assert_eq!(
            decision.relationship,
            ProfileRelationshipExposure::Minimum,
            "the one thing an inbound block withholds is that it exists"
        );
    }

    #[test]
    fn an_inbound_block_leaves_no_friends_only_field_open() {
        let mut policy_input = input(ProfileViewerRelationship::Friend);
        policy_input.vis_bio = ProfileVisibility::Friends;
        policy_input.owner_blocked_caller = true;

        let decision = decide_profile_visibility(policy_input);

        assert_eq!(
            decision.bio_pronouns_and_links,
            ProfileFieldExposure::Hidden
        );
    }

    #[test]
    fn a_mutual_block_reports_the_callers_own_outbound_block() {
        let mut policy_input = input(ProfileViewerRelationship::None);
        policy_input.caller_blocked_owner = true;
        policy_input.owner_blocked_caller = true;

        let decision = decide_profile_visibility(policy_input);

        assert_eq!(decision.relationship, ProfileRelationshipExposure::Blocked);
        assert_eq!(
            decide_profile_visibility(ProfileVisibilityInput {
                caller_blocked_owner: true,
                owner_blocked_caller: false,
                ..policy_input
            })
            .relationship,
            ProfileRelationshipExposure::Blocked,
            "an outbound block reads the same whether or not it is reciprocated"
        );
    }

    #[test]
    fn outbound_block_is_distinct_but_uses_no_relationship_visibility() {
        let mut policy_input = input(ProfileViewerRelationship::Friend);
        policy_input.vis_bio = ProfileVisibility::Friends;
        policy_input.caller_blocked_owner = true;

        let decision = decide_profile_visibility(policy_input);

        assert_eq!(
            decision.bio_pronouns_and_links,
            ProfileFieldExposure::Hidden
        );
        assert_eq!(decision.relationship, ProfileRelationshipExposure::Blocked);
    }

    #[test]
    fn deleted_precedes_all_relationship_and_directional_block_facts() {
        for relationship in [
            ProfileViewerRelationship::SelfView,
            ProfileViewerRelationship::Friend,
            ProfileViewerRelationship::None,
        ] {
            let mut policy_input = input(relationship);
            policy_input.vis_bio = ProfileVisibility::Public;
            policy_input.vis_communities = ProfileVisibility::Public;
            policy_input.vis_friends = ProfileVisibility::Public;
            policy_input.caller_blocked_owner = true;
            policy_input.owner_blocked_caller = true;
            policy_input.has_shared_server_context = true;
            policy_input.is_deleted = true;

            let decision = decide_profile_visibility(policy_input);

            assert_eq!(decision.identity, ProfileIdentityExposure::Tombstone);
            assert_eq!(decision.media, ProfileMediaExposure::Default);
            assert_eq!(
                decision.bio_pronouns_and_links,
                ProfileFieldExposure::Hidden
            );
            assert_eq!(decision.communities, ProfileFieldExposure::Hidden);
            assert_eq!(decision.friends, ProfileFieldExposure::Hidden);
            assert_eq!(decision.custom_status, ProfileFieldExposure::Hidden);
            assert_eq!(decision.presence, ProfilePresenceExposure::ForcedOffline);
            assert_eq!(decision.server_context, ProfileFieldExposure::Hidden);
            assert_eq!(decision.relationship, ProfileRelationshipExposure::None);
        }
    }

    #[test]
    fn invisible_status_forces_offline_after_the_visibility_decision() {
        for relationship in [
            ProfileViewerRelationship::Friend,
            ProfileViewerRelationship::None,
        ] {
            let decision = decide_profile_visibility(input(relationship));

            assert_eq!(
                decision.presence_for_status("invisible"),
                ProfilePresenceExposure::ForcedOffline
            );
            assert_eq!(
                decision.presence_for_status("online"),
                ProfilePresenceExposure::Real
            );
        }
    }

    #[test]
    fn an_account_still_sees_its_own_invisible_status() {
        let decision = decide_profile_visibility(input(ProfileViewerRelationship::SelfView));

        assert_eq!(
            decision.presence_for_status("invisible"),
            ProfilePresenceExposure::Real,
            "reporting your own invisible state back as offline would hide the setting from you"
        );
    }

    #[test]
    fn folding_status_never_widens_an_already_forced_offline_decision() {
        let decision = decide_profile_visibility(ProfileVisibilityInput {
            is_deleted: true,
            ..input(ProfileViewerRelationship::Friend)
        });

        for status in ["online", "idle", "dnd", "invisible"] {
            assert_eq!(
                decision.presence_for_status(status),
                ProfilePresenceExposure::ForcedOffline
            );
        }
    }

    #[test]
    fn server_context_requires_shared_membership_and_is_masked_only_by_deletion() {
        let cases = [
            (
                input(ProfileViewerRelationship::SelfView),
                ProfileFieldExposure::Hidden,
            ),
            (
                ProfileVisibilityInput {
                    has_shared_server_context: true,
                    ..input(ProfileViewerRelationship::SelfView)
                },
                ProfileFieldExposure::Visible,
            ),
            (
                input(ProfileViewerRelationship::Friend),
                ProfileFieldExposure::Hidden,
            ),
            (
                ProfileVisibilityInput {
                    has_shared_server_context: true,
                    ..input(ProfileViewerRelationship::None)
                },
                ProfileFieldExposure::Visible,
            ),
            (
                ProfileVisibilityInput {
                    has_shared_server_context: true,
                    caller_blocked_owner: true,
                    ..input(ProfileViewerRelationship::Friend)
                },
                ProfileFieldExposure::Visible,
            ),
            (
                ProfileVisibilityInput {
                    has_shared_server_context: true,
                    owner_blocked_caller: true,
                    ..input(ProfileViewerRelationship::Friend)
                },
                ProfileFieldExposure::Visible,
            ),
            (
                ProfileVisibilityInput {
                    has_shared_server_context: true,
                    is_deleted: true,
                    ..input(ProfileViewerRelationship::SelfView)
                },
                ProfileFieldExposure::Hidden,
            ),
        ];

        for (policy_input, expected) in cases {
            assert_eq!(
                decide_profile_visibility(policy_input).server_context,
                expected
            );
        }
    }

    #[test]
    fn relationship_exposure_keeps_minimum_payload_non_directional() {
        let cases = [
            (
                input(ProfileViewerRelationship::SelfView),
                ProfileRelationshipExposure::SelfView,
            ),
            (
                input(ProfileViewerRelationship::Friend),
                ProfileRelationshipExposure::Friend,
            ),
            (
                input(ProfileViewerRelationship::None),
                ProfileRelationshipExposure::NoRelationship,
            ),
            (
                ProfileVisibilityInput {
                    caller_blocked_owner: true,
                    ..input(ProfileViewerRelationship::None)
                },
                ProfileRelationshipExposure::Blocked,
            ),
            (
                ProfileVisibilityInput {
                    owner_blocked_caller: true,
                    ..input(ProfileViewerRelationship::None)
                },
                ProfileRelationshipExposure::Minimum,
            ),
        ];

        for (policy_input, expected) in cases {
            assert_eq!(
                decide_profile_visibility(policy_input).relationship,
                expected
            );
        }
    }

    #[test]
    fn unknown_visibility_values_fail_closed_to_private() {
        assert_eq!(
            ProfileVisibility::from_db("public"),
            ProfileVisibility::Public
        );
        assert_eq!(
            ProfileVisibility::from_db("friends"),
            ProfileVisibility::Friends
        );
        assert_eq!(
            ProfileVisibility::from_db("private"),
            ProfileVisibility::Private
        );
        assert_eq!(
            ProfileVisibility::from_db("unexpected"),
            ProfileVisibility::Private
        );
        assert_eq!(
            ProfileVisibility::from_db("PUBLIC"),
            ProfileVisibility::Private
        );
    }
}
