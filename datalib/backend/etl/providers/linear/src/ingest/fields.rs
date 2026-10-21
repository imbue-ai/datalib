//! What each table asks Linear for: one GraphQL selection per kind of
//! object. A reference to another object is asked for as `{ id }`, so
//! a record changes only when its own fields do.
//!
//! Left out on purpose: what depends on who is asking (`favorite`,
//! `isMe`, `viewer*`, `subscribers`), what Linear's AI regenerates on
//! its own (`aiAnalysis`, `activitySummary`, `summary`), and the
//! collaborative-editing state (`descriptionState`, `contentState`,
//! `bodyData`), which re-encodes the markdown beside it.

const BOT: &str = "botActor { id type subType name }";

pub const TEAM: &str = "id createdAt updatedAt archivedAt retiredAt name key displayName \
    description icon color timezone visibility cyclesEnabled cycleDuration \
    cycleStartDay cycleCooldownTime triageEnabled issueEstimationType autoClosePeriod \
    autoArchivePeriod parent { id } defaultIssueState { id } triageIssueState { id } \
    issueCount progressHistory currentProgress activeCycle { id }";

pub const USER: &str = "id createdAt updatedAt archivedAt name displayName email avatarUrl \
    description title timezone active admin owner guest app url disableReason statusEmoji \
    statusLabel statusUntilAt lastSeen createdIssueCount";

pub const WORKFLOW_STATE: &str =
    "id createdAt updatedAt archivedAt name type color description position team { id }";

pub const ISSUE_LABEL: &str = "id createdAt updatedAt archivedAt retiredAt name description \
    color isGroup lastAppliedAt team { id } parent { id } creator { id }";

pub const PROJECT_STATUS: &str = "id createdAt updatedAt archivedAt name type color \
    description position indefinite team { id }";

pub const CYCLE: &str = "id createdAt updatedAt archivedAt number name description startsAt \
    endsAt completedAt autoArchivedAt team { id } progress progressHistory currentProgress \
    issueCountHistory completedIssueCountHistory scopeHistory completedScopeHistory \
    inProgressScopeHistory";

pub const PROJECT: &str = "id createdAt updatedAt archivedAt name description content slugId \
    icon color url priority priorityLabel health healthUpdatedAt startDate startDateResolution \
    targetDate targetDateResolution startedAt completedAt canceledAt autoArchivedAt trashed \
    labelIds sortOrder prioritySortOrder status { id } creator { id } lead { id } \
    convertedFromIssue { id } teams(first: 50) { nodes { id } } \
    members(first: 100) { nodes { id } } progress scope currentProgress progressHistory \
    issueCountHistory completedIssueCountHistory scopeHistory completedScopeHistory \
    inProgressScopeHistory lastUpdate { id } resourceCount";

pub const PROJECT_MILESTONE: &str = "id createdAt updatedAt archivedAt name description \
    targetDate status sortOrder project { id } progress progressHistory currentProgress";

pub const INITIATIVE: &str = "id createdAt updatedAt archivedAt name description content \
    slugId icon color url status priority targetDate targetDateResolution startedAt completedAt \
    canceledAt health healthUpdatedAt trashed labelIds sortOrder prioritySortOrder owner { id } \
    creator { id } parentInitiative { id } lastUpdate { id } projects(first: 100) { nodes { id } }";

pub const ISSUE_HISTORY: &str = "id createdAt updatedAt archivedAt actorId updatedDescription \
    fromTitle toTitle fromAssigneeId toAssigneeId fromPriority toPriority fromTeamId toTeamId \
    fromParentId toParentId fromStateId toStateId fromCycleId toCycleId toConvertedProjectId \
    fromProjectId toProjectId fromEstimate toEstimate archived trashed attachmentId \
    addedLabelIds removedLabelIds addedToReleaseIds removedFromReleaseIds autoClosed \
    autoArchived fromDueDate toDueDate customerNeedId changes \
    relationChanges { identifier type } fromProjectMilestone { id } toProjectMilestone { id } \
    fromDelegate { id } toDelegate { id } fromSlaStartedAt toSlaStartedAt fromSlaBreachesAt \
    toSlaBreachesAt fromSlaBreached toSlaBreached fromSlaType toSlaType issue { id }";

/// How many history entries come with each issue on its page; an issue
/// with more has the rest fetched on their own.
pub const HISTORY_PER_ISSUE: u32 = 50;

pub fn issue() -> String {
    format!(
        "id createdAt updatedAt archivedAt number identifier title description priority \
         priorityLabel estimate sortOrder prioritySortOrder subIssueSortOrder startedAt \
         completedAt startedTriageAt triagedAt canceledAt autoClosedAt autoArchivedAt dueDate \
         slaStartedAt slaMediumRiskAt slaHighRiskAt slaBreachesAt slaType addedToProjectAt \
         addedToCycleAt addedToTeamAt trashed snoozedUntilAt labelIds previousIdentifiers \
         reactionData integrationSourceType url branchName customerTicketCount team {{ id }} \
         cycle {{ id }} project {{ id }} projectMilestone {{ id }} creator {{ id }} \
         assignee {{ id }} delegate {{ id }} snoozedBy {{ id }} state {{ id }} parent {{ id }} \
         sourceComment {{ id }} externalUserCreator {{ id }} {BOT} \
         relations(first: 50) {{ nodes {{ id type relatedIssue {{ id }} }} }} \
         history(first: {HISTORY_PER_ISSUE}) {{ nodes {{ {ISSUE_HISTORY} {BOT} }} \
         pageInfo {{ hasNextPage endCursor }} }}"
    )
}

pub fn issue_history() -> String {
    format!("{ISSUE_HISTORY} {BOT}")
}

pub fn comment() -> String {
    format!(
        "id createdAt updatedAt archivedAt body editedAt resolvedAt quotedText reactionData \
         url hideInLinear threadSummary issueId parentId projectUpdateId initiativeUpdateId \
         projectId initiativeId documentContentId resolvingCommentId user {{ id }} \
         externalUser {{ id }} resolvingUser {{ id }} onBehalfOf {{ id }} {BOT}"
    )
}

pub const DOCUMENT: &str = "id createdAt updatedAt archivedAt title summary icon color slugId \
    hiddenAt trashed sortOrder subDocumentSortOrder content documentContentId url creator { id } \
    owner { id } updatedBy { id } project { id } initiative { id } team { id } issue { id } \
    release { id } cycle { id } parentDocument { id } lastAppliedTemplate { id }";

pub const PROJECT_UPDATE: &str = "id createdAt updatedAt archivedAt body editedAt slugId \
    shortSummary health isDiffHidden isStale diffMarkdown reactionData commentCount url \
    project { id } user { id }";

/// `issue`'s team and project are asked for only to decide whether the
/// attachment is in the selection, and are dropped before it is stored.
pub const ATTACHMENT: &str = "id createdAt updatedAt archivedAt title subtitle url metadata \
    source sourceType groupBySource creator { id } externalUserCreator { id } \
    originalIssue { id } issue { id team { id } project { id } }";
