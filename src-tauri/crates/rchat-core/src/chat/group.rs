use std::collections::HashSet;

use anyhow::{anyhow, Context};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use libp2p::{identity, PeerId};
use rusqlite::OptionalExtension;

use crate::{
    chat_kind,
    events::{
        CoreEvent, GroupMessageReceiptUpdatedEvent, GroupRecordAppliedEvent,
        GroupRosterUpdatedEvent, SharedCoreEventSink,
    },
    network::{
        command::NetworkCommand,
        gossip::{
            GroupContentType, GroupInvitePayload, GroupReceiptStatus, GroupRecordBody,
            GroupSettings,
            SignedGroupRecord,
        },
    },
    storage::db,
    AppState, NetworkState,
};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GroupChatResult {
    pub chat_id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct GroupPolicy {
    pub admin_peer_id: String,
    pub settings: GroupSettings,
    pub active_members: HashSet<String>,
    pub invited_members: HashSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordDisposition {
    Apply,
    PendingDependency,
}

pub async fn create_group(
    app_state: &AppState,
    network_state: Option<&NetworkState>,
    name: Option<String>,
) -> anyhow::Result<GroupChatResult> {
    let keypair = load_or_create_local_keypair(app_state).await?;
    let group_id = chat_kind::generate_group_chat_id();
    let resolved_name = name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| chat_kind::default_group_name(&group_id));
    let record = sign_record(
        &keypair,
        group_id.clone(),
        GroupRecordBody::GroupCreated {
            name: resolved_name.clone(),
            settings: None,
        },
    )?;

    {
        let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
        db::upsert_chat(&conn, &group_id, &resolved_name, true)?;
        db::add_chat_member(&conn, &group_id, "Me", "admin")?;
        db::insert_group_record(&conn, &record, true, false)?;
    }

    if let Some(network_state) = network_state {
        send_network_command(
            network_state,
            NetworkCommand::PublishGroupRecord {
                record: record.clone(),
            },
        )
        .await?;
    }

    Ok(GroupChatResult {
        chat_id: group_id,
        name: resolved_name,
    })
}

pub async fn join_group_legacy(
    app_state: &AppState,
    network_state: Option<&NetworkState>,
    group_id: String,
    name: Option<String>,
) -> anyhow::Result<GroupChatResult> {
    let _ = (app_state, network_state, group_id, name);
    Err(anyhow!(
        "Groups are invite-gated; accept an invite instead."
    ))
}

pub fn get_group_policy(app_state: &AppState, group_id: &str) -> anyhow::Result<GroupPolicy> {
    let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
    let records = db::get_group_records_for_sync(&conn, group_id, &[], 10_000)?;
    derive_group_policy(&records).ok_or_else(|| anyhow!("Group has no valid founder record"))
}

pub fn can_peer_sync_group_records(
    app_state: &AppState,
    group_id: &str,
    peer_id: &str,
) -> anyhow::Result<bool> {
    let policy = get_group_policy(app_state, group_id)?;
    Ok(policy.active_members.contains(peer_id))
}

pub async fn update_group_settings(
    app_state: &AppState,
    network_state: &NetworkState,
    group_id: String,
    settings: GroupSettings,
) -> anyhow::Result<()> {
    let keypair = load_or_create_local_keypair(app_state).await?;
    let local_peer_id = PeerId::from_public_key(&keypair.public()).to_string();
    let policy = get_group_policy(app_state, &group_id)?;
    if policy.admin_peer_id != local_peer_id {
        return Err(anyhow!("Only the group admin can update group settings"));
    }

    let record = sign_record(
        &keypair,
        group_id,
        GroupRecordBody::GroupSettingsUpdated { settings },
    )?;
    apply_signed_record(app_state, None, &record, true)?;
    send_network_command(network_state, NetworkCommand::PublishGroupRecord { record }).await
}

pub async fn remove_member(
    app_state: &AppState,
    network_state: &NetworkState,
    group_id: String,
    peer_id: String,
) -> anyhow::Result<()> {
    let keypair = load_or_create_local_keypair(app_state).await?;
    let local_peer_id = PeerId::from_public_key(&keypair.public()).to_string();
    let policy = get_group_policy(app_state, &group_id)?;
    if policy.admin_peer_id != local_peer_id {
        return Err(anyhow!("Only the group admin can remove members"));
    }
    if peer_id == policy.admin_peer_id {
        return Err(anyhow!("Admin removal is blocked until succession is implemented"));
    }

    let record = sign_record(
        &keypair,
        group_id,
        GroupRecordBody::MemberRemoved { peer_id },
    )?;
    apply_signed_record(app_state, None, &record, true)?;
    send_network_command(network_state, NetworkCommand::PublishGroupRecord { record }).await
}

pub async fn invite_member(
    app_state: &AppState,
    network_state: &NetworkState,
    group_id: String,
    peer_id: String,
) -> anyhow::Result<String> {
    if !chat_kind::is_group_chat_id(&group_id) {
        return Err(anyhow!("Invalid group id. Expected format group:<uuid>"));
    }

    let keypair = load_or_create_local_keypair(app_state).await?;
    let local_peer_id = PeerId::from_public_key(&keypair.public()).to_string();
    let policy = get_group_policy(app_state, &group_id)?;
    if !can_invite(&policy, &local_peer_id) {
        return Err(anyhow!("Only the group admin can invite members"));
    }
    let (group_name, related_records) = {
        let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
        let group_name = db::get_chat_list(&conn)?
            .into_iter()
            .find(|chat| chat.id == group_id)
            .map(|chat| chat.name)
            .unwrap_or_else(|| chat_kind::default_group_name(&group_id));
        let related_records = db::get_group_records_for_sync(&conn, &group_id, &[], 64)?;
        (group_name, related_records)
    };

    let invite_record = sign_record(
        &keypair,
        group_id.clone(),
        GroupRecordBody::MemberInvited {
            peer_id: peer_id.clone(),
            role: "member".to_string(),
        },
    )?;
    let invite_id = invite_record.id().to_string();
    let payload = GroupInvitePayload {
        version: 1,
        invite_id: invite_id.clone(),
        group_id: group_id.clone(),
        group_name,
        inviter_peer_id: local_peer_id,
        invitee_peer_id: peer_id.clone(),
        created_at: timestamp_now(),
        invite_record: invite_record.clone(),
        related_records,
    };

    {
        let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
        db::upsert_group_invite(&conn, &payload, "sent")?;
        db::insert_group_record(&conn, &invite_record, true, false)?;
    }

    send_network_command(
        network_state,
        NetworkCommand::PublishGroupRecord {
            record: invite_record,
        },
    )
    .await?;
    send_network_command(
        network_state,
        NetworkCommand::SendGroupInvite {
            target_peer_id: peer_id,
            invite: payload,
        },
    )
    .await?;

    Ok(invite_id)
}

pub async fn accept_invite(
    app_state: &AppState,
    network_state: &NetworkState,
    invite_id: String,
) -> anyhow::Result<String> {
    let invite = {
        let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
        db::get_group_invite_payload(&conn, &invite_id)?
            .ok_or_else(|| anyhow!("Unknown group invite: {invite_id}"))?
    };
    if !invite.invite_record.verify() {
        return Err(anyhow!("Group invite signature could not be verified"));
    }

    let keypair = load_or_create_local_keypair(app_state).await?;
    let local_peer_id = PeerId::from_public_key(&keypair.public()).to_string();
    if invite.invitee_peer_id != local_peer_id {
        return Err(anyhow!("Group invite was not addressed to this peer"));
    }

    for record in invite
        .related_records
        .iter()
        .chain(std::iter::once(&invite.invite_record))
    {
        apply_signed_record(app_state, None, record, true)?;
    }

    let joined_record = sign_record(
        &keypair,
        invite.group_id.clone(),
        GroupRecordBody::MemberJoined {
            peer_id: invite.invitee_peer_id.clone(),
        },
    )?;
    {
        let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
        db::update_group_invite_status(&conn, &invite_id, "accepted")?;
        db::upsert_chat(&conn, &invite.group_id, &invite.group_name, true)?;
        db::add_chat_member(&conn, &invite.group_id, "Me", "member")?;
        db::insert_group_record(&conn, &joined_record, true, false)?;
    }

    send_network_command(
        network_state,
        NetworkCommand::SubscribeGroup {
            group_id: invite.group_id.clone(),
        },
    )
    .await?;
    send_network_command(
        network_state,
        NetworkCommand::PublishGroupRecord {
            record: joined_record,
        },
    )
    .await?;
    send_network_command(
        network_state,
        NetworkCommand::SyncGroup {
            group_id: invite.group_id.clone(),
        },
    )
    .await?;

    Ok(invite.group_id)
}

pub fn reject_invite(app_state: &AppState, invite_id: &str) -> anyhow::Result<()> {
    let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
    db::update_group_invite_status(&conn, invite_id, "rejected")
}

pub async fn leave_group(
    app_state: &AppState,
    network_state: &NetworkState,
    group_id: String,
) -> anyhow::Result<()> {
    let keypair = load_or_create_local_keypair(app_state).await?;
    let local_peer_id = PeerId::from_public_key(&keypair.public()).to_string();
    let policy = get_group_policy(app_state, &group_id)?;
    if policy.admin_peer_id == local_peer_id {
        return Err(anyhow!("Admin leave is blocked until succession is implemented"));
    }
    let record = sign_record(
        &keypair,
        group_id.clone(),
        GroupRecordBody::MemberLeft {
            peer_id: local_peer_id,
        },
    )?;
    {
        let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
        let _ = db::insert_group_record(&conn, &record, true, false)?;
        let _ = db::remove_chat_member(&conn, &group_id, "Me");
        db::delete_group_chat(&conn, &group_id)?;
    }
    send_network_command(network_state, NetworkCommand::PublishGroupRecord { record }).await?;
    send_network_command(
        network_state,
        NetworkCommand::UnsubscribeGroup {
            group_id: group_id.clone(),
        },
    )
    .await?;
    Ok(())
}

pub async fn rename_group(
    app_state: &AppState,
    network_state: &NetworkState,
    group_id: String,
    name: String,
) -> anyhow::Result<()> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(anyhow!("Group name cannot be empty"));
    }
    let keypair = load_or_create_local_keypair(app_state).await?;
    let local_peer_id = PeerId::from_public_key(&keypair.public()).to_string();
    let policy = get_group_policy(app_state, &group_id)?;
    if policy.admin_peer_id != local_peer_id {
        return Err(anyhow!("Only the group admin can rename the group"));
    }
    let record = sign_record(
        &keypair,
        group_id.clone(),
        GroupRecordBody::GroupRenamed { name: name.clone() },
    )?;
    {
        let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
        db::upsert_chat(&conn, &group_id, &name, true)?;
        db::insert_group_record(&conn, &record, true, false)?;
    }
    send_network_command(network_state, NetworkCommand::PublishGroupRecord { record }).await?;
    Ok(())
}

pub async fn send_group_text(
    app_state: &AppState,
    network_state: &NetworkState,
    group_id: String,
    text: String,
    sender_alias: Option<String>,
) -> anyhow::Result<String> {
    send_group_message_record(
        app_state,
        network_state,
        group_id,
        GroupContentType::Text,
        Some(text),
        None,
        sender_alias,
    )
    .await
}

pub async fn send_group_media_reference(
    app_state: &AppState,
    network_state: &NetworkState,
    group_id: String,
    kind: GroupContentType,
    file_hash: String,
    display_name: Option<String>,
    sender_alias: Option<String>,
) -> anyhow::Result<String> {
    if !kind.needs_file_transfer() {
        return Err(anyhow!("Group media reference requires a file-backed content type"));
    }
    send_group_message_record(
        app_state,
        network_state,
        group_id,
        kind,
        display_name,
        Some(file_hash),
        sender_alias,
    )
    .await
}

pub async fn sync_group(network_state: &NetworkState, group_id: String) -> anyhow::Result<()> {
    send_network_command(network_state, NetworkCommand::SyncGroup { group_id }).await
}

pub async fn mark_read(
    app_state: &AppState,
    network_state: &NetworkState,
    group_id: String,
    message_ids: Vec<String>,
) -> anyhow::Result<()> {
    if message_ids.is_empty() {
        return Ok(());
    }
    let record = create_receipt_record(
        app_state,
        group_id.clone(),
        message_ids,
        GroupReceiptStatus::Read,
    )
    .await?;
    apply_signed_record(app_state, None, &record, true)?;
    send_network_command(network_state, NetworkCommand::PublishGroupRecord { record }).await
}

pub async fn create_receipt_record(
    app_state: &AppState,
    group_id: String,
    message_ids: Vec<String>,
    status: GroupReceiptStatus,
) -> anyhow::Result<SignedGroupRecord> {
    if message_ids.is_empty() {
        return Err(anyhow!("Group receipt requires at least one message id"));
    }
    let keypair = load_or_create_local_keypair(app_state).await?;
    sign_record(
        &keypair,
        group_id,
        GroupRecordBody::Receipt {
            message_ids,
            status,
        },
    )
}

fn derive_group_policy(records: &[SignedGroupRecord]) -> Option<GroupPolicy> {
    let mut ordered = records.to_vec();
    ordered.sort_by(|a, b| {
        a.timestamp()
            .cmp(&b.timestamp())
            .then_with(|| a.id().cmp(b.id()))
    });

    let mut admin_peer_id = None;
    let mut settings = GroupSettings::default();
    let mut active_members = HashSet::new();
    let mut invited_members = HashSet::new();

    for record in ordered {
        match record.body() {
            GroupRecordBody::GroupCreated {
                settings: group_settings,
                ..
            } => {
                if admin_peer_id.is_none() {
                    let author = record.author_peer_id().to_string();
                    admin_peer_id = Some(author.clone());
                    active_members.insert(author);
                    settings = group_settings.clone().unwrap_or_default();
                }
            }
            GroupRecordBody::MemberInvited { peer_id, .. } => {
                if let Some(admin) = admin_peer_id.as_deref() {
                    if record.author_peer_id() == admin
                        || (settings.members_can_invite
                            && active_members.contains(record.author_peer_id()))
                    {
                        invited_members.insert(peer_id.clone());
                    }
                }
            }
            GroupRecordBody::MemberJoined { peer_id } => {
                if record.author_peer_id() == peer_id && invited_members.contains(peer_id) {
                    active_members.insert(peer_id.clone());
                    invited_members.remove(peer_id);
                }
            }
            GroupRecordBody::MemberLeft { peer_id } => {
                if record.author_peer_id() == peer_id
                    && Some(peer_id.as_str()) != admin_peer_id.as_deref()
                {
                    active_members.remove(peer_id);
                    invited_members.remove(peer_id);
                }
            }
            GroupRecordBody::MemberRemoved { peer_id } => {
                if Some(record.author_peer_id()) == admin_peer_id.as_deref()
                    && Some(peer_id.as_str()) != admin_peer_id.as_deref()
                {
                    active_members.remove(peer_id);
                    invited_members.remove(peer_id);
                }
            }
            GroupRecordBody::GroupSettingsUpdated {
                settings: updated_settings,
            } => {
                if Some(record.author_peer_id()) == admin_peer_id.as_deref() {
                    settings = updated_settings.clone();
                }
            }
            GroupRecordBody::GroupRenamed { .. }
            | GroupRecordBody::Message { .. }
            | GroupRecordBody::Receipt { .. }
            | GroupRecordBody::Head { .. }
            | GroupRecordBody::FileAvailability { .. } => {}
        }
    }

    admin_peer_id.map(|admin_peer_id| GroupPolicy {
        admin_peer_id,
        settings,
        active_members,
        invited_members,
    })
}

fn record_precedes(candidate: &SignedGroupRecord, existing: &SignedGroupRecord) -> bool {
    existing.timestamp() < candidate.timestamp()
        || (existing.timestamp() == candidate.timestamp() && existing.id() < candidate.id())
}

fn derive_group_policy_before(
    records: &[SignedGroupRecord],
    candidate: &SignedGroupRecord,
) -> Option<GroupPolicy> {
    let prior_records: Vec<SignedGroupRecord> = records
        .iter()
        .filter(|record| record_precedes(candidate, record))
        .cloned()
        .collect();
    derive_group_policy(&prior_records)
}

fn can_invite(policy: &GroupPolicy, peer_id: &str) -> bool {
    policy.admin_peer_id == peer_id
        || (policy.settings.members_can_invite && policy.active_members.contains(peer_id))
}

fn group_record_state(
    conn: &rusqlite::Connection,
    record_id: &str,
) -> anyhow::Result<Option<(bool, bool)>> {
    conn.query_row(
        "SELECT verified, pending FROM group_records WHERE id = ?1",
        [record_id],
        |row| Ok((row.get::<_, i64>(0)? != 0, row.get::<_, i64>(1)? != 0)),
    )
    .optional()
    .map_err(Into::into)
}

fn mark_group_record_verified(
    conn: &rusqlite::Connection,
    record_id: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE group_records SET verified = 1, pending = 0 WHERE id = ?1",
        [record_id],
    )?;
    Ok(())
}

fn pending_group_records(
    conn: &rusqlite::Connection,
    group_id: &str,
) -> anyhow::Result<Vec<SignedGroupRecord>> {
    let mut stmt = conn.prepare(
        "SELECT payload_json FROM group_records
         WHERE group_id = ?1 AND pending = 1
         ORDER BY timestamp ASC",
    )?;
    let rows = stmt.query_map([group_id], |row| row.get::<_, String>(0))?;
    let mut records = Vec::new();
    for row in rows {
        records.push(serde_json::from_str(&row?)?);
    }
    Ok(records)
}

fn validate_group_record(
    conn: &rusqlite::Connection,
    record: &SignedGroupRecord,
) -> anyhow::Result<RecordDisposition> {
    let existing_records = db::get_group_records_for_sync(conn, record.group_id(), &[], 10_000)?;
    let current_policy = derive_group_policy(&existing_records);
    let policy_before_record = derive_group_policy_before(&existing_records, record);

    match record.body() {
        GroupRecordBody::GroupCreated { .. } => {
            if current_policy.is_some() {
                return Err(anyhow!("Group already has a founder record"));
            }
            Ok(RecordDisposition::Apply)
        }
        GroupRecordBody::MemberInvited { .. } => {
            let Some(policy) = policy_before_record else {
                return Ok(RecordDisposition::PendingDependency);
            };
            if can_invite(&policy, record.author_peer_id()) {
                Ok(RecordDisposition::Apply)
            } else if policy.active_members.contains(record.author_peer_id()) {
                Err(anyhow!("Members cannot invite unless group settings allow it"))
            } else {
                Ok(RecordDisposition::PendingDependency)
            }
        }
        GroupRecordBody::MemberJoined { peer_id } => {
            let Some(policy) = policy_before_record else {
                return Ok(RecordDisposition::PendingDependency);
            };
            if record.author_peer_id() != peer_id {
                return Err(anyhow!("MemberJoined author must match joined peer"));
            }
            if policy.invited_members.contains(peer_id) {
                Ok(RecordDisposition::Apply)
            } else {
                Ok(RecordDisposition::PendingDependency)
            }
        }
        GroupRecordBody::MemberLeft { peer_id } => {
            let Some(policy) = policy_before_record else {
                return Ok(RecordDisposition::PendingDependency);
            };
            if record.author_peer_id() != peer_id {
                return Err(anyhow!("Members can only leave for themselves"));
            }
            if policy.admin_peer_id == *peer_id {
                return Err(anyhow!(
                    "Admin leave is blocked until succession is implemented"
                ));
            }
            if policy.active_members.contains(peer_id) {
                Ok(RecordDisposition::Apply)
            } else {
                Ok(RecordDisposition::PendingDependency)
            }
        }
        GroupRecordBody::GroupRenamed { .. }
        | GroupRecordBody::GroupSettingsUpdated { .. }
        | GroupRecordBody::MemberRemoved { .. } => {
            let Some(policy) = policy_before_record else {
                return Ok(RecordDisposition::PendingDependency);
            };
            if policy.admin_peer_id != record.author_peer_id() {
                return Err(anyhow!("Only the group admin can apply this record"));
            }
            if let GroupRecordBody::MemberRemoved { peer_id } = record.body() {
                if policy.admin_peer_id == *peer_id {
                    return Err(anyhow!(
                        "Admin removal is blocked until succession is implemented"
                    ));
                }
            }
            Ok(RecordDisposition::Apply)
        }
        GroupRecordBody::Message { .. }
        | GroupRecordBody::Receipt { .. }
        | GroupRecordBody::Head { .. }
        | GroupRecordBody::FileAvailability { .. } => {
            let Some(policy) = policy_before_record else {
                return Ok(RecordDisposition::PendingDependency);
            };
            if policy.active_members.contains(record.author_peer_id()) {
                Ok(RecordDisposition::Apply)
            } else {
                Ok(RecordDisposition::PendingDependency)
            }
        }
    }
}

fn retry_pending_group_records(
    app_state: &AppState,
    event_sink: Option<&SharedCoreEventSink>,
    group_id: &str,
) {
    let pending = {
        let Ok(conn) = app_state.db_conn.lock() else {
            return;
        };
        pending_group_records(&conn, group_id).unwrap_or_default()
    };

    for record in pending {
        if let Err(err) = apply_signed_record(app_state, event_sink, &record, true) {
            eprintln!(
                "[Group] Failed to retry pending record {}: {}",
                record.id(),
                err
            );
        }
    }
}

pub fn apply_signed_record(
    app_state: &AppState,
    event_sink: Option<&SharedCoreEventSink>,
    record: &SignedGroupRecord,
    verified: bool,
) -> anyhow::Result<bool> {
    let mut emitted_message = None;
    let mut roster_event = None;
    let mut receipt_events = Vec::new();
    let record_applied;
    {
        let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
        if !verified {
            if db::group_record_exists(&conn, record.id()) {
                return Ok(false);
            }
            return db::insert_group_record(&conn, record, false, true);
        }

        let existing_state = group_record_state(&conn, record.id())?;
        if let Some((_, false)) = existing_state {
            return Ok(false);
        }

        match validate_group_record(&conn, record)? {
            RecordDisposition::Apply => {
                if existing_state.is_some() {
                    mark_group_record_verified(&conn, record.id())?;
                    record_applied = true;
                } else {
                    record_applied = db::insert_group_record(&conn, record, true, false)?;
                }
            }
            RecordDisposition::PendingDependency => {
                if existing_state.is_none() {
                    let _ = db::insert_group_record(&conn, record, false, true)?;
                }
                return Ok(false);
            }
        }

        match record.body() {
            GroupRecordBody::GroupCreated { name, .. } => {
                db::upsert_chat(&conn, record.group_id(), name, true)?;
                ensure_peer(&conn, record.author_peer_id(), "group")?;
                db::upsert_chat_member_state(
                    &conn,
                    record.group_id(),
                    record.author_peer_id(),
                    "admin",
                    "joined",
                    None,
                    Some(record.id()),
                )?;
                roster_event = Some(GroupRosterUpdatedEvent {
                    group_id: record.group_id().to_string(),
                    peer_id: record.author_peer_id().to_string(),
                    membership_state: "joined".to_string(),
                });
            }
            GroupRecordBody::MemberInvited { peer_id, role } => {
                ensure_peer(&conn, peer_id, "group")?;
                db::upsert_chat_member_state(
                    &conn,
                    record.group_id(),
                    peer_id,
                    role,
                    "invited",
                    Some(record.author_peer_id()),
                    Some(record.id()),
                )?;
                roster_event = Some(GroupRosterUpdatedEvent {
                    group_id: record.group_id().to_string(),
                    peer_id: peer_id.clone(),
                    membership_state: "invited".to_string(),
                });
            }
            GroupRecordBody::MemberJoined { peer_id } => {
                ensure_peer(&conn, peer_id, "group")?;
                db::upsert_chat_member_state(
                    &conn,
                    record.group_id(),
                    peer_id,
                    "member",
                    "joined",
                    None,
                    Some(record.id()),
                )?;
                roster_event = Some(GroupRosterUpdatedEvent {
                    group_id: record.group_id().to_string(),
                    peer_id: peer_id.clone(),
                    membership_state: "joined".to_string(),
                });
            }
            GroupRecordBody::MemberLeft { peer_id } => {
                let _ = db::remove_chat_member(&conn, record.group_id(), peer_id);
                roster_event = Some(GroupRosterUpdatedEvent {
                    group_id: record.group_id().to_string(),
                    peer_id: peer_id.clone(),
                    membership_state: "left".to_string(),
                });
            }
            GroupRecordBody::GroupRenamed { name } => {
                db::upsert_chat(&conn, record.group_id(), name, true)?;
            }
            GroupRecordBody::GroupSettingsUpdated { .. } => {}
            GroupRecordBody::MemberRemoved { peer_id } => {
                let _ = db::remove_chat_member(&conn, record.group_id(), peer_id);
                roster_event = Some(GroupRosterUpdatedEvent {
                    group_id: record.group_id().to_string(),
                    peer_id: peer_id.clone(),
                    membership_state: "removed".to_string(),
                });
            }
            GroupRecordBody::Message {
                content_type,
                text_content,
                file_hash,
                sender_alias,
            } => {
                let db_msg = group_record_to_db_message(
                    record,
                    *content_type,
                    text_content.clone(),
                    file_hash.clone(),
                    sender_alias.clone(),
                );
                ensure_peer(&conn, record.author_peer_id(), "group")?;
                db::upsert_chat(
                    &conn,
                    record.group_id(),
                    &chat_kind::default_group_name(record.group_id()),
                    true,
                )?;
                db::upsert_chat_member_state(
                    &conn,
                    record.group_id(),
                    "Me",
                    "member",
                    "joined",
                    None,
                    None,
                )?;
                db::upsert_chat_member_state(
                    &conn,
                    record.group_id(),
                    record.author_peer_id(),
                    "member",
                    "joined",
                    None,
                    None,
                )?;
                if let Some(file_hash) = file_hash {
                    ensure_incomplete_file_row(&conn, file_hash)?;
                    db::upsert_group_file_source(&conn, record.group_id(), file_hash, record.author_peer_id())?;
                }
                match db::insert_message(&conn, &db_msg) {
                    Ok(()) => emitted_message = Some(db_msg),
                    Err(err) => {
                        let duplicate = err
                            .to_string()
                            .to_ascii_lowercase()
                            .contains("unique constraint");
                        if !duplicate {
                            return Err(err);
                        }
                    }
                }
            }
            GroupRecordBody::Receipt {
                message_ids,
                status,
            } => {
                for message_id in message_ids {
                    db::upsert_group_message_receipt(
                        &conn,
                        record.group_id(),
                        message_id,
                        record.author_peer_id(),
                        status.as_str(),
                        record.timestamp(),
                    )?;
                    receipt_events.push(GroupMessageReceiptUpdatedEvent {
                        group_id: record.group_id().to_string(),
                        message_id: message_id.clone(),
                        peer_id: record.author_peer_id().to_string(),
                        status: status.as_str().to_string(),
                    });
                }
            }
            GroupRecordBody::Head { .. } => {}
            GroupRecordBody::FileAvailability { file_hash } => {
                db::upsert_group_file_source(
                    &conn,
                    record.group_id(),
                    file_hash,
                    record.author_peer_id(),
                )?;
            }
        }
    }

    if let Some(event_sink) = event_sink {
        event_sink.emit(CoreEvent::GroupRecordApplied(GroupRecordAppliedEvent {
            group_id: record.group_id().to_string(),
            record_id: record.id().to_string(),
            record_type: record.body().kind().to_string(),
        }));
        if let Some(roster) = roster_event {
            event_sink.emit(CoreEvent::GroupRosterUpdated(roster));
        }
        for receipt in receipt_events {
            event_sink.emit(CoreEvent::GroupMessageReceiptUpdated(receipt));
        }
        if let Some(message) = emitted_message {
            event_sink.emit(CoreEvent::MessageReceived(message));
        }
    }

    if record_applied {
        retry_pending_group_records(app_state, event_sink, record.group_id());
    }

    Ok(record_applied)
}

pub fn store_incoming_invite(
    app_state: &AppState,
    event_sink: Option<&SharedCoreEventSink>,
    invite: &GroupInvitePayload,
) -> anyhow::Result<()> {
    if !invite.invite_record.verify() {
        return Err(anyhow!("Group invite signature could not be verified"));
    }
    let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
    db::upsert_group_invite(&conn, invite, "pending")?;
    drop(conn);

    if let Some(event_sink) = event_sink {
        event_sink.emit(CoreEvent::GroupInviteReceived(
            crate::events::GroupInviteReceivedEvent {
                invite_id: invite.invite_id.clone(),
                group_id: invite.group_id.clone(),
                group_name: invite.group_name.clone(),
                inviter_peer_id: invite.inviter_peer_id.clone(),
            },
        ));
    }
    Ok(())
}

async fn send_group_message_record(
    app_state: &AppState,
    network_state: &NetworkState,
    group_id: String,
    content_type: GroupContentType,
    text_content: Option<String>,
    file_hash: Option<String>,
    sender_alias: Option<String>,
) -> anyhow::Result<String> {
    let keypair = load_or_create_local_keypair(app_state).await?;
    let local_peer_id = PeerId::from_public_key(&keypair.public()).to_string();
    if let Ok(policy) = get_group_policy(app_state, &group_id) {
        if !policy.active_members.contains(&local_peer_id) {
            return Err(anyhow!("Only active group members can send group messages"));
        }
    }
    let record = sign_record(
        &keypair,
        group_id.clone(),
        GroupRecordBody::Message {
            content_type,
            text_content: text_content.clone(),
            file_hash: file_hash.clone(),
            sender_alias: sender_alias.clone(),
        },
    )?;
    let mut db_msg = group_record_to_db_message(
        &record,
        content_type,
        text_content,
        file_hash.clone(),
        sender_alias,
    );
    db_msg.peer_id = "Me".to_string();
    {
        let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
        if !db::chat_exists(&conn, &group_id) {
            db::upsert_chat(&conn, &group_id, &chat_kind::default_group_name(&group_id), true)?;
            db::add_chat_member(&conn, &group_id, "Me", "member")?;
        }
        if let Some(file_hash) = &file_hash {
            db::upsert_group_file_source(&conn, &group_id, file_hash, "Me")?;
        }
        db::insert_message(&conn, &db_msg)?;
        db::insert_group_record(&conn, &record, true, false)?;
    }
    let msg_id = record.id().to_string();
    send_network_command(network_state, NetworkCommand::PublishGroupRecord { record }).await?;
    Ok(msg_id)
}

fn group_record_to_db_message(
    record: &SignedGroupRecord,
    content_type: GroupContentType,
    text_content: Option<String>,
    file_hash: Option<String>,
    sender_alias: Option<String>,
) -> db::Message {
    let text_content = match content_type {
        GroupContentType::Text => text_content,
        GroupContentType::Image | GroupContentType::Sticker => None,
        GroupContentType::Document => Some(
            text_content
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| "document".to_string()),
        ),
        GroupContentType::Video => Some(
            text_content
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| "video".to_string()),
        ),
        GroupContentType::Audio => Some(
            text_content
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| "audio".to_string()),
        ),
    };

    db::Message {
        id: record.id().to_string(),
        chat_id: record.group_id().to_string(),
        peer_id: record.author_peer_id().to_string(),
        timestamp: record.timestamp(),
        content_type: content_type.as_str().to_string(),
        text_content,
        file_hash,
        status: "delivered".to_string(),
        content_metadata: None,
        sender_alias,
    }
}

fn sign_record(
    keypair: &identity::Keypair,
    group_id: String,
    body: GroupRecordBody,
) -> anyhow::Result<SignedGroupRecord> {
    SignedGroupRecord::new(
        keypair,
        group_id,
        format!("group-rec-{}-{}", timestamp_now(), rand::random::<u32>()),
        timestamp_now(),
        Vec::new(),
        body,
    )
}

pub async fn load_or_create_local_keypair(
    app_state: &AppState,
) -> anyhow::Result<identity::Keypair> {
    let config_manager = app_state.config_manager.lock().await;
    let mut config = config_manager.load().await.unwrap_or_default();
    if let Some(ref key_b64) = config.user.libp2p_keypair {
        if let Ok(key_bytes) = BASE64.decode(key_b64) {
            if let Ok(keypair) = identity::Keypair::from_protobuf_encoding(&key_bytes) {
                return Ok(keypair);
            }
        }
    }

    let keypair = identity::Keypair::generate_ed25519();
    let key_bytes = keypair
        .to_protobuf_encoding()
        .context("encode generated libp2p keypair")?;
    config.user.libp2p_keypair = Some(BASE64.encode(&key_bytes));
    config_manager.save(&config).await?;
    Ok(keypair)
}

fn timestamp_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn send_network_command(
    network_state: &NetworkState,
    command: NetworkCommand,
) -> anyhow::Result<()> {
    let tx = network_state.sender.lock().await;
    tx.send(command)
        .await
        .map_err(|_| anyhow!("network command channel is closed"))
}

fn ensure_peer(conn: &rusqlite::Connection, peer_id: &str, method: &str) -> anyhow::Result<()> {
    if !db::is_peer(conn, peer_id) {
        db::add_peer(conn, peer_id, None, None, method)?;
    }
    Ok(())
}

fn ensure_incomplete_file_row(conn: &rusqlite::Connection, file_hash: &str) -> anyhow::Result<()> {
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM files WHERE file_hash = ?1",
            [file_hash],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !exists {
        conn.execute(
            "INSERT INTO files (file_hash, file_name, mime_type, size_bytes, is_complete)
             VALUES (?1, NULL, 'application/octet-stream', 0, 0)",
            [file_hash],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_state() -> AppState {
        let temp = tempfile::tempdir().expect("temp");
        crate::runtime::create_app_state(temp.path().to_path_buf()).expect("app state")
    }

    fn keypair() -> identity::Keypair {
        identity::Keypair::generate_ed25519()
    }

    fn peer_id(keypair: &identity::Keypair) -> String {
        PeerId::from_public_key(&keypair.public()).to_string()
    }

    fn signed(
        keypair: &identity::Keypair,
        group_id: &str,
        id: &str,
        timestamp: i64,
        body: GroupRecordBody,
    ) -> SignedGroupRecord {
        SignedGroupRecord::new(
            keypair,
            group_id.to_string(),
            format!("test-{id}-{}", rand::random::<u64>()),
            timestamp,
            Vec::new(),
            body,
        )
        .expect("record")
    }

    fn apply(app_state: &AppState, record: &SignedGroupRecord) -> anyhow::Result<bool> {
        apply_signed_record(app_state, None, record, true)
    }

    #[test]
    fn invite_gated_join_requires_existing_invite_payload() {
        let app_state = app_state();
        let conn = app_state.db_conn.lock().expect("db");
        let missing = db::get_group_invite_payload(&conn, "missing").expect("query");
        assert!(missing.is_none());
    }

    #[test]
    fn default_group_policy_makes_founder_admin_and_disables_member_invites() {
        let app_state = app_state();
        let founder = keypair();
        let group_id = chat_kind::generate_group_chat_id();
        let created = signed(
            &founder,
            &group_id,
            "created",
            1,
            GroupRecordBody::GroupCreated {
                name: "Test".to_string(),
                settings: None,
            },
        );

        assert!(apply(&app_state, &created).expect("apply"));
        let policy = get_group_policy(&app_state, &group_id).expect("policy");
        assert_eq!(policy.admin_peer_id, peer_id(&founder));
        assert!(!policy.settings.members_can_invite);
    }

    #[test]
    fn non_admin_rename_is_rejected() {
        let app_state = app_state();
        let founder = keypair();
        let member = keypair();
        let group_id = chat_kind::generate_group_chat_id();

        apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "created",
                1,
                GroupRecordBody::GroupCreated {
                    name: "Test".to_string(),
                    settings: None,
                },
            ),
        )
        .expect("created");
        apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "invite-member",
                2,
                GroupRecordBody::MemberInvited {
                    peer_id: peer_id(&member),
                    role: "member".to_string(),
                },
            ),
        )
        .expect("invite");
        apply(
            &app_state,
            &signed(
                &member,
                &group_id,
                "member-joined",
                3,
                GroupRecordBody::MemberJoined {
                    peer_id: peer_id(&member),
                },
            ),
        )
        .expect("joined");

        let result = apply(
            &app_state,
            &signed(
                &member,
                &group_id,
                "bad-rename",
                4,
                GroupRecordBody::GroupRenamed {
                    name: "Owned".to_string(),
                },
            ),
        );

        assert!(result.is_err());
    }

    #[test]
    fn member_join_without_invite_is_pending_not_applied() {
        let app_state = app_state();
        let founder = keypair();
        let member = keypair();
        let group_id = chat_kind::generate_group_chat_id();

        apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "created",
                1,
                GroupRecordBody::GroupCreated {
                    name: "Test".to_string(),
                    settings: None,
                },
            ),
        )
        .expect("created");

        let joined = signed(
            &member,
            &group_id,
            "member-joined",
            2,
            GroupRecordBody::MemberJoined {
                peer_id: peer_id(&member),
            },
        );

        assert!(!apply(&app_state, &joined).expect("pending"));
        let policy = get_group_policy(&app_state, &group_id).expect("policy");
        assert!(!policy.active_members.contains(&peer_id(&member)));
    }

    #[test]
    fn member_invite_is_accepted_when_setting_allows_it() {
        let app_state = app_state();
        let founder = keypair();
        let member = keypair();
        let invited_by_member = keypair();
        let group_id = chat_kind::generate_group_chat_id();

        apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "created",
                1,
                GroupRecordBody::GroupCreated {
                    name: "Test".to_string(),
                    settings: Some(GroupSettings {
                        members_can_invite: true,
                    }),
                },
            ),
        )
        .expect("created");
        apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "invite-member",
                2,
                GroupRecordBody::MemberInvited {
                    peer_id: peer_id(&member),
                    role: "member".to_string(),
                },
            ),
        )
        .expect("founder invite");
        apply(
            &app_state,
            &signed(
                &member,
                &group_id,
                "member-joined",
                3,
                GroupRecordBody::MemberJoined {
                    peer_id: peer_id(&member),
                },
            ),
        )
        .expect("member joined");

        assert!(apply(
            &app_state,
            &signed(
                &member,
                &group_id,
                "member-invite",
                4,
                GroupRecordBody::MemberInvited {
                    peer_id: peer_id(&invited_by_member),
                    role: "member".to_string(),
                },
            ),
        )
        .expect("member invite"));
        let policy = get_group_policy(&app_state, &group_id).expect("policy");
        assert!(policy
            .invited_members
            .contains(&peer_id(&invited_by_member)));
    }

    #[test]
    fn pending_join_applies_after_required_invite_arrives() {
        let app_state = app_state();
        let founder = keypair();
        let member = keypair();
        let group_id = chat_kind::generate_group_chat_id();

        apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "created",
                1,
                GroupRecordBody::GroupCreated {
                    name: "Test".to_string(),
                    settings: None,
                },
            ),
        )
        .expect("created");
        assert!(!apply(
            &app_state,
            &signed(
                &member,
                &group_id,
                "member-joined",
                4,
                GroupRecordBody::MemberJoined {
                    peer_id: peer_id(&member),
                },
            ),
        )
        .expect("pending join"));
        apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "invite-member",
                3,
                GroupRecordBody::MemberInvited {
                    peer_id: peer_id(&member),
                    role: "member".to_string(),
                },
            ),
        )
        .expect("invite");

        let policy = get_group_policy(&app_state, &group_id).expect("policy");
        assert!(policy.active_members.contains(&peer_id(&member)));
    }

    #[test]
    fn pre_join_message_remains_pending_after_member_later_joins() {
        let app_state = app_state();
        let founder = keypair();
        let member = keypair();
        let group_id = chat_kind::generate_group_chat_id();
        let message = signed(
            &member,
            &group_id,
            "pre-join-message",
            2,
            GroupRecordBody::Message {
                content_type: GroupContentType::Text,
                text_content: Some("too early".to_string()),
                file_hash: None,
                sender_alias: None,
            },
        );

        apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "created",
                1,
                GroupRecordBody::GroupCreated {
                    name: "Test".to_string(),
                    settings: None,
                },
            ),
        )
        .expect("created");
        assert!(!apply(&app_state, &message).expect("pending message"));
        apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "invite-member",
                3,
                GroupRecordBody::MemberInvited {
                    peer_id: peer_id(&member),
                    role: "member".to_string(),
                },
            ),
        )
        .expect("invite");
        apply(
            &app_state,
            &signed(
                &member,
                &group_id,
                "member-joined",
                4,
                GroupRecordBody::MemberJoined {
                    peer_id: peer_id(&member),
                },
            ),
        )
        .expect("joined");

        let conn = app_state.db_conn.lock().expect("db");
        assert_eq!(
            group_record_state(&conn, message.id()).expect("state"),
            Some((false, true))
        );
        let inserted: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE id = ?1)",
                [message.id()],
                |row| row.get(0),
            )
            .expect("message exists query");
        assert!(!inserted);
    }

    #[test]
    fn only_active_members_can_sync_group_records() {
        let app_state = app_state();
        let founder = keypair();
        let member = keypair();
        let outsider = keypair();
        let group_id = chat_kind::generate_group_chat_id();

        apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "created",
                1,
                GroupRecordBody::GroupCreated {
                    name: "Test".to_string(),
                    settings: None,
                },
            ),
        )
        .expect("created");
        apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "invite-member",
                2,
                GroupRecordBody::MemberInvited {
                    peer_id: peer_id(&member),
                    role: "member".to_string(),
                },
            ),
        )
        .expect("invite");
        apply(
            &app_state,
            &signed(
                &member,
                &group_id,
                "member-joined",
                3,
                GroupRecordBody::MemberJoined {
                    peer_id: peer_id(&member),
                },
            ),
        )
        .expect("joined");

        assert!(can_peer_sync_group_records(&app_state, &group_id, &peer_id(&founder))
            .expect("founder sync"));
        assert!(can_peer_sync_group_records(&app_state, &group_id, &peer_id(&member))
            .expect("member sync"));
        assert!(!can_peer_sync_group_records(&app_state, &group_id, &peer_id(&outsider))
            .expect("outsider sync"));
    }

    #[test]
    fn admin_leave_is_blocked_until_succession_exists() {
        let app_state = app_state();
        let founder = keypair();
        let group_id = chat_kind::generate_group_chat_id();

        apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "created",
                1,
                GroupRecordBody::GroupCreated {
                    name: "Test".to_string(),
                    settings: None,
                },
            ),
        )
        .expect("created");

        let result = apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "admin-left",
                2,
                GroupRecordBody::MemberLeft {
                    peer_id: peer_id(&founder),
                },
            ),
        );

        assert!(result.is_err());
    }

    #[test]
    fn admin_member_removed_record_removes_active_member() {
        let app_state = app_state();
        let founder = keypair();
        let member = keypair();
        let group_id = chat_kind::generate_group_chat_id();

        apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "created",
                1,
                GroupRecordBody::GroupCreated {
                    name: "Test".to_string(),
                    settings: None,
                },
            ),
        )
        .expect("created");
        apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "invite-member",
                2,
                GroupRecordBody::MemberInvited {
                    peer_id: peer_id(&member),
                    role: "member".to_string(),
                },
            ),
        )
        .expect("invite");
        apply(
            &app_state,
            &signed(
                &member,
                &group_id,
                "member-joined",
                3,
                GroupRecordBody::MemberJoined {
                    peer_id: peer_id(&member),
                },
            ),
        )
        .expect("joined");
        apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "member-removed",
                4,
                GroupRecordBody::MemberRemoved {
                    peer_id: peer_id(&member),
                },
            ),
        )
        .expect("removed");

        let policy = get_group_policy(&app_state, &group_id).expect("policy");
        assert!(!policy.active_members.contains(&peer_id(&member)));
    }

    #[test]
    fn delivered_receipt_does_not_downgrade_read() {
        let app_state = app_state();
        let founder = keypair();
        let member = keypair();
        let group_id = chat_kind::generate_group_chat_id();
        let message_id = "message-1".to_string();

        apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "created",
                1,
                GroupRecordBody::GroupCreated {
                    name: "Test".to_string(),
                    settings: None,
                },
            ),
        )
        .expect("created");
        apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "invite-member",
                2,
                GroupRecordBody::MemberInvited {
                    peer_id: peer_id(&member),
                    role: "member".to_string(),
                },
            ),
        )
        .expect("invite");
        apply(
            &app_state,
            &signed(
                &member,
                &group_id,
                "member-joined",
                3,
                GroupRecordBody::MemberJoined {
                    peer_id: peer_id(&member),
                },
            ),
        )
        .expect("joined");

        apply(
            &app_state,
            &signed(
                &member,
                &group_id,
                "read-receipt",
                4,
                GroupRecordBody::Receipt {
                    message_ids: vec![message_id.clone()],
                    status: GroupReceiptStatus::Read,
                },
            ),
        )
        .expect("read");
        apply(
            &app_state,
            &signed(
                &member,
                &group_id,
                "delivered-receipt",
                5,
                GroupRecordBody::Receipt {
                    message_ids: vec![message_id.clone()],
                    status: GroupReceiptStatus::Delivered,
                },
            ),
        )
        .expect("delivered");

        let conn = app_state.db_conn.lock().expect("db");
        let stored: String = conn
            .query_row(
                "SELECT status FROM group_message_receipts WHERE message_id = ?1 AND peer_id = ?2",
                (&message_id, peer_id(&member)),
                |row| row.get(0),
            )
            .expect("receipt");
        assert_eq!(stored, "read");
    }
}
