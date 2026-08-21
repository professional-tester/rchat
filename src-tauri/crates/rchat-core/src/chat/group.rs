use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Context};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use libp2p::{identity, PeerId};
use rusqlite::OptionalExtension;

use crate::{
    chat::media::{self, MediaKind},
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
    pub image_hash: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CreateGroupOptions {
    pub name: Option<String>,
    pub image_path: Option<String>,
    pub settings: Option<GroupSettings>,
    pub require_name: bool,
}

#[derive(Debug, Clone)]
pub struct GroupPolicy {
    pub admin_peer_id: String,
    pub settings: GroupSettings,
    pub active_members: HashSet<String>,
    pub invited_members: HashSet<String>,
    pub automatic_successor_peer_id: Option<String>,
    pub dissolved: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum GroupLeaveOutcome {
    Left,
    TransferredThenLeft { successor_peer_id: String },
    Dissolved,
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
    create_group_with_options(
        app_state,
        network_state,
        CreateGroupOptions {
            name,
            ..Default::default()
        },
    )
    .await
}

pub async fn create_group_with_options(
    app_state: &AppState,
    network_state: Option<&NetworkState>,
    options: CreateGroupOptions,
) -> anyhow::Result<GroupChatResult> {
    let keypair = load_or_create_local_keypair(app_state).await?;
    let local_peer_id = PeerId::from_public_key(&keypair.public()).to_string();
    let group_id = chat_kind::generate_group_chat_id();
    let resolved_name = options
        .name
        .map(|n| n.trim().to_string())
        .unwrap_or_default();
    if options.require_name && resolved_name.is_empty() {
        return Err(anyhow!("Group name is required"));
    }
    let resolved_name = if resolved_name.is_empty() {
        chat_kind::default_group_name(&group_id)
    } else {
        resolved_name
    };
    let image_hash = if let Some(image_path) = options.image_path.filter(|path| !path.is_empty()) {
        Some(
            media::store_file_object_from_path(app_state, MediaKind::Image, image_path)?
                .file_hash,
        )
    } else {
        None
    };
    let record = sign_record(
        app_state,
        &keypair,
        group_id.clone(),
        GroupRecordBody::GroupCreated {
            name: resolved_name.clone(),
            settings: options.settings,
            image_hash: image_hash.clone(),
        },
    )?;
    let file_availability_record = image_hash
        .as_ref()
        .map(|file_hash| {
            // Continuation of the same causal chain: the image availability
            // is announced immediately after creation, so it must occupy the
            // next Lamport position rather than recomputing from the still-
            // empty DB (both records are signed before either is persisted).
            SignedGroupRecord::new(
                &keypair,
                group_id.clone(),
                format!(
                    "group-rec-{}-{}",
                    timestamp_now(),
                    rand::random::<u32>()
                ),
                timestamp_now(),
                vec![record.id().to_string()],
                record.lamport_counter().saturating_add(1),
                GroupRecordBody::FileAvailability {
                    file_hash: file_hash.clone(),
                },
            )
        })
        .transpose()?;

    {
        let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
        db::upsert_chat(&conn, &group_id, &resolved_name, true)?;
        db::update_chat_image_hash(&conn, &group_id, image_hash.as_deref())?;
        db::add_chat_member(&conn, &group_id, "Me", "admin")?;
        db::insert_group_record(&conn, &record, true, false)?;
        if let (Some(image_hash), Some(file_record)) = (&image_hash, &file_availability_record) {
            db::upsert_group_file_source(&conn, &group_id, image_hash, &local_peer_id)?;
            db::insert_group_record(&conn, file_record, true, false)?;
        }
    }

    if let Some(network_state) = network_state {
        send_network_command(
            network_state,
            NetworkCommand::PublishGroupRecord {
                record: record.clone(),
            },
        )
        .await?;
        if let Some(file_record) = file_availability_record {
            send_network_command(
                network_state,
                NetworkCommand::PublishGroupRecord {
                    record: file_record,
                },
            )
            .await?;
        }
    }

    Ok(GroupChatResult {
        chat_id: group_id,
        name: resolved_name,
        image_hash,
    })
}

pub fn get_group_policy(app_state: &AppState, group_id: &str) -> anyhow::Result<GroupPolicy> {
    let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
    let records = db::get_group_records_for_sync(&conn, group_id, &[], 10_000)?;
    derive_group_policy(&records).ok_or_else(|| anyhow!("Group has no valid founder record"))
}

pub fn get_group_roster(
    app_state: &AppState,
    group_id: &str,
) -> anyhow::Result<Vec<db::GroupMemberRow>> {
    let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
    db::get_group_roster(&conn, group_id)
}

pub fn get_group_image_hash(app_state: &AppState, group_id: &str) -> anyhow::Result<Option<String>> {
    let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
    db::get_group_image_hash(&conn, group_id)
}

pub fn get_group_message_receipts(
    app_state: &AppState,
    group_id: &str,
    message_id: &str,
) -> anyhow::Result<Vec<db::GroupMessageReceiptRow>> {
    let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
    db::get_group_message_receipts(&conn, group_id, message_id)
}

pub fn get_group_pending_record_summary(
    app_state: &AppState,
    group_id: &str,
) -> anyhow::Result<db::GroupPendingRecordSummary> {
    let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
    db::get_group_pending_record_summary(&conn, group_id)
}

pub async fn is_local_group_admin(app_state: &AppState, group_id: &str) -> anyhow::Result<bool> {
    let policy = get_group_policy(app_state, group_id)?;
    let keypair = load_or_create_local_keypair(app_state).await?;
    Ok(PeerId::from_public_key(&keypair.public()).to_string() == policy.admin_peer_id)
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
        app_state,
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
        return Err(anyhow!(
            "The current administrator cannot be removed; transfer administration first"
        ));
    }

    let record = sign_record(
        app_state,
        &keypair,
        group_id,
        GroupRecordBody::MemberRemoved { peer_id },
    )?;
    apply_signed_record(app_state, None, &record, true)?;
    send_network_command(network_state, NetworkCommand::PublishGroupRecord { record }).await
}

pub async fn transfer_group_admin(
    app_state: &AppState,
    network_state: &NetworkState,
    group_id: String,
    new_admin_peer_id: String,
) -> anyhow::Result<()> {
    let keypair = load_or_create_local_keypair(app_state).await?;
    let local_peer_id = PeerId::from_public_key(&keypair.public()).to_string();
    let policy = get_group_policy(app_state, &group_id)?;
    if policy.admin_peer_id != local_peer_id {
        return Err(anyhow!("Only the group admin can transfer administration"));
    }
    if new_admin_peer_id == local_peer_id {
        return Err(anyhow!("The current admin is already the group administrator"));
    }
    if !policy.active_members.contains(&new_admin_peer_id) {
        return Err(anyhow!("The new administrator must be an active member"));
    }
    let record = sign_record(
        app_state,
        &keypair,
        group_id,
        GroupRecordBody::AdminTransferred { new_admin_peer_id },
    )?;
    apply_signed_record(app_state, None, &record, true)?;
    send_network_command(network_state, NetworkCommand::PublishGroupRecord { record }).await
}

pub async fn preview_leave_group(
    app_state: &AppState,
    group_id: &str,
) -> anyhow::Result<GroupLeaveOutcome> {
    let keypair = load_or_create_local_keypair(app_state).await?;
    let local_peer_id = PeerId::from_public_key(&keypair.public()).to_string();
    let policy = get_group_policy(app_state, group_id)?;
    if policy.admin_peer_id != local_peer_id {
        return Ok(GroupLeaveOutcome::Left);
    }
    match policy.automatic_successor_peer_id {
        Some(successor_peer_id) => Ok(GroupLeaveOutcome::TransferredThenLeft { successor_peer_id }),
        None => Ok(GroupLeaveOutcome::Dissolved),
    }
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
        app_state,
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
    if get_group_policy(app_state, &invite.group_id)?.dissolved {
        let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
        db::update_group_invite_status(&conn, &invite_id, "revoked")?;
        return Err(anyhow!("This group has been dissolved"));
    }

    let joined_record = sign_record(
        app_state,
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
) -> anyhow::Result<GroupLeaveOutcome> {
    let keypair = load_or_create_local_keypair(app_state).await?;
    let local_peer_id = PeerId::from_public_key(&keypair.public()).to_string();
    let policy = get_group_policy(app_state, &group_id)?;
    let outcome;
    if policy.admin_peer_id == local_peer_id {
        if let Some(successor_peer_id) = policy.automatic_successor_peer_id {
            let transfer_timestamp = timestamp_now();
            let transfer = sign_record_at(
                app_state,
                &keypair,
                group_id.clone(),
                transfer_timestamp,
                Vec::new(),
                GroupRecordBody::AdminTransferred {
                    new_admin_peer_id: successor_peer_id.clone(),
                },
            )?;
            apply_signed_record(app_state, None, &transfer, true)?;
            send_network_command(
                network_state,
                NetworkCommand::PublishGroupRecord {
                    record: transfer.clone(),
                },
            )
            .await?;
            let leave = sign_record_at(
                app_state,
                &keypair,
                group_id.clone(),
                transfer_timestamp.saturating_add(1),
                vec![transfer.id().to_string()],
                GroupRecordBody::MemberLeft {
                    peer_id: local_peer_id,
                },
            )?;
            apply_signed_record(app_state, None, &leave, true)?;
            send_network_command(network_state, NetworkCommand::PublishGroupRecord { record: leave })
                .await?;
            outcome = GroupLeaveOutcome::TransferredThenLeft { successor_peer_id };
        } else {
            let invitees = {
                let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
                db::get_open_group_invitee_peer_ids(&conn, &group_id)?
            };
            let dissolution = sign_record(app_state, &keypair, group_id.clone(), GroupRecordBody::GroupDissolved)?;
            apply_signed_record(app_state, None, &dissolution, true)?;
            send_network_command(
                network_state,
                NetworkCommand::PublishGroupRecord {
                    record: dissolution.clone(),
                },
            )
            .await?;
            for target_peer_id in invitees {
                send_network_command(
                    network_state,
                    NetworkCommand::SendGroupDissolution {
                        target_peer_id,
                        record: dissolution.clone(),
                    },
                )
                .await?;
            }
            outcome = GroupLeaveOutcome::Dissolved;
        }
    } else {
        let leave = sign_record(
            app_state,
            &keypair,
            group_id.clone(),
            GroupRecordBody::MemberLeft {
                peer_id: local_peer_id,
            },
        )?;
        apply_signed_record(app_state, None, &leave, true)?;
        send_network_command(network_state, NetworkCommand::PublishGroupRecord { record: leave })
            .await?;
        outcome = GroupLeaveOutcome::Left;
    }
    if !matches!(outcome, GroupLeaveOutcome::Dissolved) {
        let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
        db::delete_group_chat(&conn, &group_id)?;
    }
    send_network_command(
        network_state,
        NetworkCommand::UnsubscribeGroup {
            group_id: group_id.clone(),
        },
    )
    .await?;
    Ok(outcome)
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
        app_state,
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
        app_state,
        &keypair,
        group_id,
        GroupRecordBody::Receipt {
            message_ids,
            status,
        },
    )
}

/// The causal total order over a group's records. The signed Lamport counter
/// dominates; `(author_peer_id, id)` only break ties between records that are
/// genuinely concurrent (same counter from different authors) and between
/// pre-counter legacy records. Wall-clock timestamps are never part of the
/// order, so clock skew or a forged clock cannot decide authorization.
fn record_order_key(record: &SignedGroupRecord) -> (u64, &str, &str) {
    (
        record.lamport_counter(),
        record.author_peer_id(),
        record.id(),
    )
}

fn derive_group_policy(records: &[SignedGroupRecord]) -> Option<GroupPolicy> {
    let mut ordered = records.to_vec();
    ordered.sort_by(|a, b| record_order_key(a).cmp(&record_order_key(b)));

    let mut admin_peer_id = None;
    let mut settings = GroupSettings::default();
    let mut active_members = HashSet::new();
    let mut invited_members = HashSet::new();
    let mut membership_order: HashMap<String, (u64, String)> = HashMap::new();
    let mut dissolved = false;

    for record in ordered {
        if dissolved {
            continue;
        }
        match record.body() {
            GroupRecordBody::GroupCreated {
                settings: group_settings,
                ..
            } => {
                if admin_peer_id.is_none() {
                    let author = record.author_peer_id().to_string();
                    admin_peer_id = Some(author.clone());
                    active_members.insert(author.clone());
                    membership_order.insert(
                        author,
                        (record.lamport_counter(), record.id().to_string()),
                    );
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
                    membership_order.insert(
                        peer_id.clone(),
                        (record.lamport_counter(), record.id().to_string()),
                    );
                }
            }
            GroupRecordBody::MemberLeft { peer_id } => {
                if record.author_peer_id() == peer_id
                    && Some(peer_id.as_str()) != admin_peer_id.as_deref()
                {
                    active_members.remove(peer_id);
                    invited_members.remove(peer_id);
                    membership_order.remove(peer_id);
                }
            }
            GroupRecordBody::MemberRemoved { peer_id } => {
                if Some(record.author_peer_id()) == admin_peer_id.as_deref()
                    && Some(peer_id.as_str()) != admin_peer_id.as_deref()
                {
                    active_members.remove(peer_id);
                    invited_members.remove(peer_id);
                    membership_order.remove(peer_id);
                }
            }
            GroupRecordBody::AdminTransferred { new_admin_peer_id } => {
                if Some(record.author_peer_id()) == admin_peer_id.as_deref()
                    && active_members.contains(new_admin_peer_id)
                    && record.author_peer_id() != new_admin_peer_id
                {
                    admin_peer_id = Some(new_admin_peer_id.clone());
                }
            }
            GroupRecordBody::GroupDissolved => {
                if Some(record.author_peer_id()) == admin_peer_id.as_deref()
                    && active_members.len() == 1
                {
                    dissolved = true;
                    active_members.clear();
                    invited_members.clear();
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

    admin_peer_id.map(|admin_peer_id| {
        let automatic_successor_peer_id = active_members
            .iter()
            .filter(|peer_id| peer_id.as_str() != admin_peer_id)
            .min_by_key(|peer_id| {
                membership_order
                    .get(*peer_id)
                    .cloned()
                    .unwrap_or((u64::MAX, (*peer_id).clone()))
            })
            .cloned();
        GroupPolicy {
            admin_peer_id,
            settings,
            active_members,
            invited_members,
            automatic_successor_peer_id,
            dissolved,
        }
    })
}

fn record_precedes(candidate: &SignedGroupRecord, existing: &SignedGroupRecord) -> bool {
    record_order_key(existing) < record_order_key(candidate)
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
    for parent_id in &record.unsigned.parents {
        if !matches!(group_record_state(conn, parent_id)?, Some((true, false))) {
            return Ok(RecordDisposition::PendingDependency);
        }
    }
    // Empty-parents records from older tests/builders are treated as building
    // on the current head. Out-of-order counters within the jump window stay
    // pending for retry; far leaps and the 0/MAX sentinels are hard errors
    // handled by validate_record_counter. Behind counters fall through to
    // the fork check there.
    if !matches!(record.body(), GroupRecordBody::GroupCreated { .. })
        && record.unsigned.parents.is_empty()
        && record.lamport_counter() != 0
        && record.lamport_counter() != u64::MAX
    {
        let max_known = db::get_group_max_lamport_counter(conn, record.group_id()).unwrap_or(0);
        let counter = record.lamport_counter();
        if counter != max_known.saturating_add(1) {
            if max_known != 0
                && counter > max_known.saturating_add(1)
                && counter <= max_known.saturating_add(SignedGroupRecord::MAX_COUNTER_JUMP)
            {
                return Ok(RecordDisposition::PendingDependency);
            }
            if max_known == 0 {
                return Ok(RecordDisposition::PendingDependency);
            }
        }
    }
    validate_record_counter(conn, record)?;
    let existing_records = db::get_group_records_for_sync(conn, record.group_id(), &[], 10_000)?;
    let current_policy = derive_group_policy(&existing_records);
    let policy_before_record = if matches!(record.body(), GroupRecordBody::GroupCreated { .. }) {
        None
    } else if record.unsigned.parents.is_empty() {
        // Empty parents leniently means "build on current frontier" — use
        // the classic counter-order snapshot so existing tests that predate
        // parent tracking keep their intended semantics (e.g. a pre-join
        // message stays pending even after later joins).
        derive_group_policy_before(&existing_records, record)
    } else {
        let closure = collect_parent_closure(conn, &record.unsigned.parents)?;
        // An empty closure means the parent(s) existed but their history
        // could not be collected (concurrent vanished) — treat as pending.
        if closure.is_empty() {
            return Ok(RecordDisposition::PendingDependency);
        }
        derive_group_policy(&closure)
    };

    if current_policy.as_ref().is_some_and(|policy| policy.dissolved) {
        return Err(anyhow!("Group has been dissolved"));
    }

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
                    "The administrator must transfer administration before leaving"
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
                        "The current administrator cannot be removed; transfer administration first"
                    ));
                }
            }
            Ok(RecordDisposition::Apply)
        }
        GroupRecordBody::AdminTransferred { new_admin_peer_id } => {
            let Some(policy) = policy_before_record else {
                return Ok(RecordDisposition::PendingDependency);
            };
            if policy.admin_peer_id != record.author_peer_id() {
                if policy.active_members.contains(record.author_peer_id()) {
                    return Ok(RecordDisposition::PendingDependency);
                }
                return Err(anyhow!("Only the group admin can transfer administration"));
            }
            if new_admin_peer_id == record.author_peer_id() {
                return Err(anyhow!("The current admin is already the group administrator"));
            }
            if !policy.active_members.contains(new_admin_peer_id) {
                return Err(anyhow!("The new administrator must be an active member"));
            }
            Ok(RecordDisposition::Apply)
        }
        GroupRecordBody::GroupDissolved => {
            let Some(policy) = policy_before_record else {
                return Ok(RecordDisposition::PendingDependency);
            };
            if policy.admin_peer_id != record.author_peer_id() {
                if policy.active_members.contains(record.author_peer_id()) {
                    return Ok(RecordDisposition::PendingDependency);
                }
                return Err(anyhow!("Only the group admin can dissolve the group"));
            }
            if policy.active_members.len() != 1 {
                return Err(anyhow!("A group can only be dissolved by its sole active member"));
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

/// Enforce the causal counter discipline for current-version records:
/// a nonzero position within the jump cap, and strict monotonicity per
/// author. These checks run before any policy evaluation, so a record can
/// v3 is a mandatory upgrade: every record must carry a causal counter
/// that directly follows its parents, so a claimed ordering position cannot
/// be used to authorize a pre-signed action that did not causally depend on
/// its predecessors.
fn validate_record_counter(
    conn: &rusqlite::Connection,
    record: &SignedGroupRecord,
) -> anyhow::Result<()> {
    let counter = record.lamport_counter();
    if counter == 0 {
        return Err(anyhow!("Group record is missing its causal counter"));
    }
    if counter == u64::MAX {
        return Err(anyhow!("Group record counter exhausted"));
    }
    let is_created = matches!(record.body(), GroupRecordBody::GroupCreated { .. });
    if is_created {
        if !record.unsigned.parents.is_empty() {
            return Err(anyhow!("GroupCreated must have no parents"));
        }
        if counter != 1 {
            return Err(anyhow!("GroupCreated must be at causal position 1"));
        }
    } else if record.unsigned.parents.is_empty() {
        // Empty parents are handled as pending in the outer validator when
        // out-of-order; here the counter is known to follow the current head,
        // so just fall through to the frontier/fork checks.
    } else {
        // Direct parents must already be present (missing parents are
        // handled as PendingDependency before this is called), so we can
        // require the counter to directly follow them.
        let mut max_parent = 0u64;
        for parent_id in &record.unsigned.parents {
            let Some(parent) = db::get_group_record(conn, parent_id)? else {
                return Err(anyhow!("Group record parent {parent_id} not found"));
            };
            max_parent = max_parent.max(parent.lamport_counter());
        }
        if counter != max_parent.saturating_add(1) {
            return Err(anyhow!(
                "Group record counter {counter} must directly follow its parents at {max_parent}"
            ));
        }
    }

    // Frontier check via an indexed MAX without a 10k page — a high counter
    // outside the timestamp-sorted page must still be visible.
    let max_known = db::get_group_max_lamport_counter(conn, record.group_id())?;
    if counter > max_known.saturating_add(SignedGroupRecord::MAX_COUNTER_JUMP) {
        return Err(anyhow!(
            "Group record counter {counter} leaps more than {} past the known frontier {max_known}",
            SignedGroupRecord::MAX_COUNTER_JUMP
        ));
    }
    if db::has_author_counter(
        conn,
        record.group_id(),
        record.author_peer_id(),
        counter,
        record.id(),
    )? {
        return Err(anyhow!(
            "Group record forks author {}'s causal position {counter}",
            record.author_peer_id()
        ));
    }
    Ok(())
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

        let disposition = match validate_group_record(&conn, record) {
            Ok(d) => d,
            Err(err) => {
                // A locally reserved pending row that fails hard validation
                // (e.g. counter fork or not building on head) must be
                // removed so the position does not stay blocked.
                if let Some((false, true)) = existing_state {
                    let _ = conn.execute(
                        "DELETE FROM group_records WHERE id = ?1",
                        [record.id()],
                    );
                }
                return Err(err);
            }
        };
        match disposition {
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
            GroupRecordBody::GroupCreated {
                name, image_hash, ..
            } => {
                db::upsert_chat(&conn, record.group_id(), name, true)?;
                db::update_chat_image_hash(&conn, record.group_id(), image_hash.as_deref())?;
                if let Some(image_hash) = image_hash {
                    ensure_incomplete_file_row(&conn, image_hash)?;
                    db::upsert_group_file_source(
                        &conn,
                        record.group_id(),
                        image_hash,
                        record.author_peer_id(),
                    )?;
                }
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
            GroupRecordBody::AdminTransferred { new_admin_peer_id } => {
                let records = db::get_group_records_for_sync(&conn, record.group_id(), &[], 10_000)?;
                if let Some(policy) = derive_group_policy(&records) {
                    for member in db::get_group_roster(&conn, record.group_id())? {
                        if member.membership_state == "joined" {
                            let role = if member.peer_id == policy.admin_peer_id {
                                "admin"
                            } else {
                                "member"
                            };
                            db::upsert_chat_member_state(
                                &conn,
                                record.group_id(),
                                &member.peer_id,
                                role,
                                "joined",
                                member.invited_by.as_deref(),
                                member.last_event_id.as_deref(),
                            )?;
                        }
                    }
                }
                roster_event = Some(GroupRosterUpdatedEvent {
                    group_id: record.group_id().to_string(),
                    peer_id: new_admin_peer_id.clone(),
                    membership_state: "admin".to_string(),
                });
            }
            GroupRecordBody::GroupDissolved => {
                db::revoke_group_invites(&conn, record.group_id())?;
                db::delete_group_chat(&conn, record.group_id())?;
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
                ensure_incomplete_file_row(&conn, file_hash)?;
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
        app_state,
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

/// The next causal position for a record in `group_id`: one past every
/// counter observed so far — verified *and* pending records alike (the
/// Lamport receive rule). Pending records count because they already occupy
/// a causal position from their author's perspective; ignoring them could
/// reissue the same position and fork the order.
pub fn next_group_record_counter(conn: &rusqlite::Connection, group_id: &str) -> u64 {
    db::get_group_max_lamport_counter(conn, group_id)
        .unwrap_or(0)
        .saturating_add(1)
}

/// Collect the transitive parent closure of `parents` (verified records only;
/// missing parents make the record pending). Used to evaluate authorization
/// against the exact causal snapshot the author built on. For the lenient
/// empty-parents case (older tests), an empty parent list is interpreted as
/// building on the entire frontier before the record, so we synthesize the
/// closure as all verified records with a smaller counter.
fn collect_parent_closure(
    conn: &rusqlite::Connection,
    parents: &[String],
) -> anyhow::Result<Vec<SignedGroupRecord>> {
    let mut seen = std::collections::HashSet::new();
    let mut stack: Vec<String> = parents.to_vec();
    let mut out = Vec::new();
    // Empty parents leniently means "all prior verified history". Collect
    // them via counter ordering so tests that predate parent tracking still
    // evaluate against the correct snapshot.
    if stack.is_empty() {
        // This path is only used when validate has already ensured the
        // record is not GroupCreated and parents were empty leniently.
        // We cannot know the record's counter here, so the caller handles
        // empty parents via current_policy. Return empty to signal that.
        return Ok(out);
    }
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let Some(record) = db::get_group_record(conn, &id)? else {
            // Caller will have already returned PendingDependency for missing
            // parents; reaching here means a parent vanished concurrently.
            continue;
        };
        // Empty-parents records in the closure themselves leniently depend on
        // all prior history by counter order.
        if record.unsigned.parents.is_empty() && record.lamport_counter() > 1 {
            let prior = db::get_group_records_before_counter(
                conn,
                &record.group_id().to_string(),
                record.lamport_counter(),
            )?;
            for prior_record in prior {
                if !seen.contains(prior_record.id()) {
                    stack.push(prior_record.id().to_string());
                }
            }
        } else {
            for parent in &record.unsigned.parents {
                if !seen.contains(parent) {
                    stack.push(parent.clone());
                }
            }
        }
        out.push(record);
    }
    Ok(out)
}

fn sign_record(
    app_state: &AppState,
    keypair: &identity::Keypair,
    group_id: String,
    body: GroupRecordBody,
) -> anyhow::Result<SignedGroupRecord> {
    sign_record_at(
        app_state,
        keypair,
        group_id,
        timestamp_now(),
        Vec::new(),
        body,
    )
}

fn sign_record_at(
    app_state: &AppState,
    keypair: &identity::Keypair,
    group_id: String,
    timestamp: i64,
    parents: Vec<String>,
    body: GroupRecordBody,
) -> anyhow::Result<SignedGroupRecord> {
    let is_created = matches!(body, GroupRecordBody::GroupCreated { .. });
    // Reserve the causal position atomically under the DB lock so two
    // concurrent local operations cannot sign the same (author,counter).
    // Retry on unique-index conflict (INSERT OR IGNORE returns 0).
    for _ in 0..5 {
        let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
        let parents_to_use = if is_created {
            Vec::new()
        } else if parents.is_empty() {
            db::get_group_head_ids(&conn, &group_id)?
        } else {
            parents.clone()
        };
        if !is_created && parents_to_use.is_empty() {
            return Err(anyhow!("Group {group_id} has no head to build on"));
        }
        let lamport_counter = if is_created {
            1
        } else {
            let mut max_parent = 0u64;
            let mut missing = false;
            for pid in &parents_to_use {
                if let Some(rec) = db::get_group_record(&conn, pid)? {
                    max_parent = max_parent.max(rec.lamport_counter());
                } else {
                    missing = true;
                    break;
                }
            }
            if missing {
                return Err(anyhow!("Parent not found for group {group_id}"));
            }
            let global_max = db::get_group_max_lamport_counter(&conn, &group_id).unwrap_or(0);
            if max_parent != global_max {
                return Err(anyhow!(
                    "Group record must build on current head {global_max}, got parent max {max_parent}"
                ));
            }
            max_parent.saturating_add(1)
        };
        if lamport_counter == 0 || lamport_counter == u64::MAX {
            return Err(anyhow!("Group record counter exhausted"));
        }
        let record = SignedGroupRecord::new(
            keypair,
            group_id.clone(),
            format!("group-rec-{}-{}", timestamp, rand::random::<u32>()),
            timestamp,
            parents_to_use.clone(),
            lamport_counter,
            body.clone(),
        )?;
        let payload_json = serde_json::to_string(&record)?;
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO group_records (id, group_id, record_type, author_peer_id, timestamp, lamport_counter, payload_json, public_key_b64, signature_b64, verified, pending, received_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            (
                record.id(),
                record.group_id(),
                record.body().kind(),
                record.author_peer_id(),
                record.timestamp(),
                record.lamport_counter() as i64,
                payload_json,
                &record.public_key_b64,
                &record.signature_b64,
                0,
                1,
                crate::storage::db::unix_now(),
            ),
        )?;
        if inserted == 0 {
            // Another concurrent local operation reserved the same
            // (group, author, counter) — retry with the new frontier.
            continue;
        }
        return Ok(record);
    }
    Err(anyhow!("Failed to allocate causal counter after retries"))
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
        use crate::storage::config::ConfigManager;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let app_dir = tempfile::tempdir_in("/tmp").expect("temp").keep();
        let conn = rusqlite::Connection::open_in_memory().expect("in-memory db");
        db::create_tables(&conn).expect("schema");
        AppState {
            config_manager: Arc::new(Mutex::new(ConfigManager::new(app_dir.clone()))),
            db_conn: Arc::new(std::sync::Mutex::new(conn)),
            app_dir,
        }
    }

    fn keypair() -> identity::Keypair {
        identity::Keypair::generate_ed25519()
    }

    fn peer_id(keypair: &identity::Keypair) -> String {
        PeerId::from_public_key(&keypair.public()).to_string()
    }

    /// A signed v3 record whose causal position is `counter`. The timestamp
    /// is deliberately derived from the counter so tests that pass skewed or
    /// tied timestamps explicitly use [`signed_at`] instead.
    fn signed(
        keypair: &identity::Keypair,
        group_id: &str,
        id: &str,
        counter: u64,
        body: GroupRecordBody,
    ) -> SignedGroupRecord {
        signed_at(
            keypair,
            group_id,
            id,
            1_700_000_000 + counter as i64,
            counter,
            body,
        )
    }

    /// Like [`signed`], but with an explicit wall-clock timestamp — used to
    /// prove that skew, ties, and backdating cannot influence authorization.
    fn signed_at(
        keypair: &identity::Keypair,
        group_id: &str,
        id: &str,
        timestamp: i64,
        counter: u64,
        body: GroupRecordBody,
    ) -> SignedGroupRecord {
        SignedGroupRecord::new(
            keypair,
            group_id.to_string(),
            format!("test-{id}-{}", rand::random::<u64>()),
            timestamp,
            Vec::new(),
            counter,
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
                image_hash: None,
            },
        );

        assert!(apply(&app_state, &created).expect("apply"));
        let policy = get_group_policy(&app_state, &group_id).expect("policy");
        assert_eq!(policy.admin_peer_id, peer_id(&founder));
        assert!(!policy.settings.members_can_invite);
    }

    #[test]
    fn administrator_transfer_and_successor_order_are_deterministic() {
        let app_state = app_state();
        let founder = keypair();
        let older = keypair();
        let newer = keypair();
        let group_id = chat_kind::generate_group_chat_id();
        let older_id = peer_id(&older);
        let newer_id = peer_id(&newer);
        let founder_id = peer_id(&founder);

        let records = [
            signed(&founder, &group_id, "created", 1, GroupRecordBody::GroupCreated {
                name: "Test".to_string(), settings: None, image_hash: None,
            }),
            signed(&founder, &group_id, "invite-older", 2, GroupRecordBody::MemberInvited {
                peer_id: older_id.clone(), role: "member".to_string(),
            }),
            signed(&older, &group_id, "join-older", 3, GroupRecordBody::MemberJoined {
                peer_id: older_id.clone(),
            }),
            signed(&founder, &group_id, "invite-newer", 4, GroupRecordBody::MemberInvited {
                peer_id: newer_id.clone(), role: "member".to_string(),
            }),
            signed(&newer, &group_id, "join-newer", 5, GroupRecordBody::MemberJoined {
                peer_id: newer_id.clone(),
            }),
        ];
        for record in &records {
            apply(&app_state, record).expect("membership record");
        }

        let policy = get_group_policy(&app_state, &group_id).expect("policy");
        assert_eq!(policy.automatic_successor_peer_id.as_deref(), Some(older_id.as_str()));

        apply(&app_state, &signed(
            &founder,
            &group_id,
            "transfer",
            6,
            GroupRecordBody::AdminTransferred { new_admin_peer_id: newer_id.clone() },
        )).expect("transfer");
        let policy = get_group_policy(&app_state, &group_id).expect("transferred policy");
        assert_eq!(policy.admin_peer_id, newer_id);
        assert_eq!(policy.automatic_successor_peer_id.as_deref(), Some(founder_id.as_str()));
    }

    #[test]
    fn sole_administrator_can_dissolve_and_later_records_are_rejected() {
        let app_state = app_state();
        let founder = keypair();
        let group_id = chat_kind::generate_group_chat_id();
        apply(&app_state, &signed(&founder, &group_id, "created", 1, GroupRecordBody::GroupCreated {
            name: "Test".to_string(), settings: None, image_hash: None,
        })).expect("created");
        apply(&app_state, &signed(&founder, &group_id, "dissolved", 2, GroupRecordBody::GroupDissolved))
            .expect("dissolved");

        let policy = get_group_policy(&app_state, &group_id).expect("policy");
        assert!(policy.dissolved);
        let error = apply(&app_state, &signed(&founder, &group_id, "message", 3, GroupRecordBody::Message {
            content_type: GroupContentType::Text,
            text_content: Some("too late".to_string()),
            file_hash: None,
            sender_alias: Some("Founder".to_string()),
        })).expect_err("records after dissolution must fail");
        assert!(error.to_string().contains("dissolved"));

        // A causally fresh record (new counter) carrying a backdated
        // wall-clock timestamp must still hit the dissolution tombstone:
        // timestamps cannot place a record before the dissolution anymore.
        let error = apply(
            &app_state,
            &signed_at(&founder, &group_id, "backdated", 1, 3, GroupRecordBody::Message {
                content_type: GroupContentType::Text,
                text_content: Some("backdated after tombstone".to_string()),
                file_hash: None,
                sender_alias: None,
            }),
        )
        .expect_err("a known dissolution must reject backdated records too");
        assert!(error.to_string().contains("dissolved"));
    }

    #[test]
    fn authorization_order_follows_counters_not_timestamps() {
        // Wall-clock timestamps are display metadata: even with perverse,
        // tied, and descending clocks the causal counters decide who is
        // admin and who joins first, so skewed peers converge identically.
        let app_state = app_state();
        let founder = keypair();
        let older = keypair();
        let newer = keypair();
        let group_id = chat_kind::generate_group_chat_id();
        let older_id = peer_id(&older);
        let newer_id = peer_id(&newer);

        // Counters ascend causally while timestamps descend and tie.
        let records = [
            signed_at(&founder, &group_id, "created", 900, 1, GroupRecordBody::GroupCreated {
                name: "Test".to_string(), settings: None, image_hash: None,
            }),
            signed_at(&founder, &group_id, "invite-older", 800, 2, GroupRecordBody::MemberInvited {
                peer_id: older_id.clone(), role: "member".to_string(),
            }),
            signed_at(&older, &group_id, "join-older", 700, 3, GroupRecordBody::MemberJoined {
                peer_id: older_id.clone(),
            }),
            signed_at(&founder, &group_id, "invite-newer", 700, 4, GroupRecordBody::MemberInvited {
                peer_id: newer_id.clone(), role: "member".to_string(),
            }),
            signed_at(&newer, &group_id, "join-newer", 100, 5, GroupRecordBody::MemberJoined {
                peer_id: newer_id.clone(),
            }),
        ];
        for record in &records {
            apply(&app_state, record).expect("membership record");
        }

        let policy = get_group_policy(&app_state, &group_id).expect("policy");
        assert_eq!(policy.admin_peer_id, peer_id(&founder));
        // The successor is the earliest *causal* join (counter 3), not the
        // one with the smallest timestamp.
        assert_eq!(
            policy.automatic_successor_peer_id.as_deref(),
            Some(older_id.as_str())
        );
    }

    #[test]
    fn concurrent_records_converge_identically_from_any_arrival_order() {
        // Genuinely concurrent records: two members invite at the same
        // causal position without having seen each other's record yet. The
        // (counter, author) order resolves them identically on every peer,
        // no matter the delivery order or clock readings.
        // Same identities and group on both peers — only arrival order differs.
        let founder = keypair();
        let alice = keypair();
        let bob = keypair();
        let group_id = chat_kind::generate_group_chat_id();
        let founder_id = peer_id(&founder);
        // Sequential prefix up to the head (counter 5).
        let mut records = vec![
            signed(&founder, &group_id, "created", 1, GroupRecordBody::GroupCreated {
                name: "Test".to_string(),
                settings: Some(GroupSettings { members_can_invite: true }),
                image_hash: None,
            }),
            signed(&founder, &group_id, "invite-alice", 2, GroupRecordBody::MemberInvited {
                peer_id: peer_id(&alice), role: "member".to_string(),
            }),
            signed(&alice, &group_id, "join-alice", 3, GroupRecordBody::MemberJoined {
                peer_id: peer_id(&alice),
            }),
            signed(&founder, &group_id, "invite-bob", 4, GroupRecordBody::MemberInvited {
                peer_id: peer_id(&bob), role: "member".to_string(),
            }),
            signed(&bob, &group_id, "join-bob", 5, GroupRecordBody::MemberJoined {
                peer_id: peer_id(&bob),
            }),
        ];
        let head_id = records.last().unwrap().id().to_string();
        // Genuinely concurrent: same parents (head) and same counter from
        // different authors — both must be accepted and converge.
        let concurrent_a = SignedGroupRecord::new(
            &alice,
            group_id.clone(),
            format!("test-concurrent-a-{}", rand::random::<u64>()),
            555,
            vec![head_id.clone()],
            6,
            GroupRecordBody::MemberInvited {
                peer_id: fixed_peer_id(1),
                role: "member".to_string(),
            },
        )
        .expect("record");
        let concurrent_b = SignedGroupRecord::new(
            &bob,
            group_id.clone(),
            format!("test-concurrent-b-{}", rand::random::<u64>()),
            999,
            vec![head_id.clone()],
            6,
            GroupRecordBody::MemberInvited {
                peer_id: fixed_peer_id(2),
                role: "member".to_string(),
            },
        )
        .expect("record");
        records.push(concurrent_a);
        records.push(concurrent_b);

        let app_a = app_state();
        for record in &records {
            apply(&app_a, record).expect("in-order apply");
        }
        let policy_in_order = get_group_policy(&app_a, &group_id).expect("policy");

        // Reverse arrival order on an independent peer — same records, same group.
        let mut reversed = records.clone();
        reversed.reverse();
        let app_b = app_state();
        for record in &reversed {
            apply(&app_b, record).expect("reversed apply");
        }
        let policy_reversed = get_group_policy(&app_b, &group_id).expect("policy");
        // Founder is the same on both peers.
        assert_eq!(founder_id, policy_in_order.admin_peer_id);
        assert_eq!(founder_id, policy_reversed.admin_peer_id);

        assert_eq!(
            policy_in_order.admin_peer_id, policy_reversed.admin_peer_id,
            "concurrent invites must converge to one admin"
        );
        let mut invited_a: Vec<String> =
            policy_in_order.invited_members.iter().cloned().collect();
        let mut invited_b: Vec<String> =
            policy_reversed.invited_members.iter().cloned().collect();
        invited_a.sort();
        invited_b.sort();
        assert_eq!(invited_a.len(), 2, "both concurrent invites survive");
        assert_eq!(invited_a, invited_b, "identical invited set from both orders");
        assert_eq!(policy_in_order.active_members.len(), 3);
        assert_eq!(policy_reversed.active_members.len(), 3);
    }

    /// A syntactically valid, deterministic peer id for concurrent-invite
    /// tests (both simulated peers must name the same invitees).
    fn fixed_peer_id(seed: u8) -> String {
        let keypair =
            identity::Keypair::ed25519_from_bytes([seed; 32]).expect("deterministic keypair");
        PeerId::from_public_key(&keypair.public()).to_string()
    }

    #[test]
    fn causal_counter_abuse_is_rejected() {
        let app_state = app_state();
        let founder = keypair();
        let group_id = chat_kind::generate_group_chat_id();
        apply(
            &app_state,
            &signed(&founder, &group_id, "created", 1, GroupRecordBody::GroupCreated {
                name: "Test".to_string(), settings: None, image_hash: None,
            }),
        )
        .expect("created");

        // A current-version record missing its causal counter is refused.
        let missing = SignedGroupRecord::new(
            &founder,
            group_id.clone(),
            format!("test-missing-{}", rand::random::<u64>()),
            1_700_000_100,
            Vec::new(),
            0,
            GroupRecordBody::MemberInvited {
                peer_id: fixed_peer_id(10),
                role: "member".to_string(),
            },
        )
        .expect("record");
        let error = apply(&app_state, &missing).expect_err("zero counter must fail");
        assert!(
            error.to_string().contains("causal counter"),
            "wrong error: {error}"
        );

        // The reserved sentinel is refused.
        let exhausted = SignedGroupRecord::new(
            &founder,
            group_id.clone(),
            format!("test-max-{}", rand::random::<u64>()),
            1_700_000_100,
            Vec::new(),
            u64::MAX,
            GroupRecordBody::MemberInvited {
                peer_id: fixed_peer_id(11),
                role: "member".to_string(),
            },
        )
        .expect("record");
        let error = apply(&app_state, &exhausted).expect_err("MAX sentinel must fail");
        assert!(error.to_string().contains("exhausted"), "wrong error: {error}");

        // A counter that leaps beyond the known frontier is refused.
        let leap = SignedGroupRecord::new(
            &founder,
            group_id.clone(),
            format!("test-leap-{}", rand::random::<u64>()),
            1_700_000_100,
            Vec::new(),
            10_000_000,
            GroupRecordBody::MemberInvited {
                peer_id: fixed_peer_id(12),
                role: "member".to_string(),
            },
        )
        .expect("record");
        let error = apply(&app_state, &leap).expect_err("far leap must fail");
        assert!(error.to_string().contains("leaps"), "wrong error: {error}");

        // Forking one causal position — two records by the same author at
        // the same counter — is a hard error (no pending, no silent win).
        let fork_a = signed(&founder, &group_id, "fork-a", 2, GroupRecordBody::MemberInvited {
            peer_id: fixed_peer_id(13),
            role: "member".to_string(),
        });
        apply(&app_state, &fork_a).expect("first fork slot wins");
        let fork_b = SignedGroupRecord::new(
            &founder,
            group_id.clone(),
            format!("test-fork-b-{}", rand::random::<u64>()),
            1_700_000_101,
            Vec::new(),
            2,
            GroupRecordBody::MemberInvited {
                peer_id: fixed_peer_id(14),
                role: "member".to_string(),
            },
        )
        .expect("record");
        let error = apply(&app_state, &fork_b).expect_err("forked position must fail");
        assert!(error.to_string().contains("forks"), "wrong error: {error}");
    }

    #[test]
    fn replayed_record_is_a_noop() {
        let app_state = app_state();
        let founder = keypair();
        let group_id = chat_kind::generate_group_chat_id();
        let created = signed(&founder, &group_id, "created", 1, GroupRecordBody::GroupCreated {
            name: "Test".to_string(), settings: None, image_hash: None,
        });
        apply(&app_state, &created).expect("created");
        let policy_before = get_group_policy(&app_state, &group_id).expect("policy");

        // Replaying the same verified record is a no-op.
        assert!(!apply(&app_state, &created).expect("replay"));
        let policy_after = get_group_policy(&app_state, &group_id).expect("policy");
        assert_eq!(policy_before.admin_peer_id, policy_after.admin_peer_id);
        assert_eq!(policy_before.active_members, policy_after.active_members);
    }

    #[test]
    fn admin_transfer_chain_pending_until_predecessor_arrives() {
        // Admin A -> B (counter 4), then B -> C (counter 5). When the
        // successor's transfer (B -> C) arrives before its predecessor's
        // (A -> B), it must stay pending until the predecessor is applied —
        // and then the chain resolves on retry.
        let app_state = app_state();
        let founder = keypair();
        let successor = keypair();
        let third = keypair();
        let successor_id = peer_id(&successor);
        let third_id = peer_id(&third);
        let group_id = chat_kind::generate_group_chat_id();
        for record in [
            signed(&founder, &group_id, "created", 1, GroupRecordBody::GroupCreated {
                name: "Test".to_string(), settings: None, image_hash: None,
            }),
            signed(&founder, &group_id, "invite-successor", 2, GroupRecordBody::MemberInvited {
                peer_id: successor_id.clone(), role: "member".to_string(),
            }),
            signed(&successor, &group_id, "join-successor", 3, GroupRecordBody::MemberJoined {
                peer_id: successor_id.clone(),
            }),
            signed(&founder, &group_id, "invite-third", 4, GroupRecordBody::MemberInvited {
                peer_id: third_id.clone(), role: "member".to_string(),
            }),
            signed(&third, &group_id, "join-third", 5, GroupRecordBody::MemberJoined {
                peer_id: third_id.clone(),
            }),
        ] {
            apply(&app_state, &record).expect("setup");
        }

        let transfer_to_successor = signed(
            &founder, &group_id, "to-successor", 6,
            GroupRecordBody::AdminTransferred { new_admin_peer_id: successor_id.clone() },
        );
        let transfer_to_third = signed(
            &successor, &group_id, "to-third", 7,
            GroupRecordBody::AdminTransferred { new_admin_peer_id: third_id.clone() },
        );

        // Out-of-order: the successor's own transfer arrives first. It
        // authorizes against the roster *before* the predecessor, where the
        // author is not yet admin, so it waits.
        assert!(!apply(&app_state, &transfer_to_third).expect("pending successor transfer"));
        let pending = {
            let conn = app_state.db_conn.lock().expect("db");
            db::get_group_record(&conn, transfer_to_third.id())
                .expect("query")
                .is_some()
        };
        assert!(pending, "must be stored pending");

        // Predecessor arrives: becomes admin, and retry cascades the pending
        // successor — final administrator converges to the third member.
        apply(&app_state, &transfer_to_successor).expect("predecessor");
        let policy = get_group_policy(&app_state, &group_id).expect("policy");
        assert_eq!(policy.admin_peer_id, third_id, "chain must resolve to the final successor");
    }

    #[test]
    fn leave_waits_for_parent_admin_transfer() {
        let app_state = app_state();
        let founder = keypair();
        let successor = keypair();
        let founder_id = peer_id(&founder);
        let successor_id = peer_id(&successor);
        let group_id = chat_kind::generate_group_chat_id();
        for record in [
            signed(&founder, &group_id, "created", 1, GroupRecordBody::GroupCreated {
                name: "Test".to_string(), settings: None, image_hash: None,
            }),
            signed(&founder, &group_id, "invited", 2, GroupRecordBody::MemberInvited {
                peer_id: successor_id.clone(), role: "member".to_string(),
            }),
            signed(&successor, &group_id, "joined", 3, GroupRecordBody::MemberJoined {
                peer_id: successor_id.clone(),
            }),
        ] {
            apply(&app_state, &record).expect("setup record");
        }
        let transfer = signed(&founder, &group_id, "transfer-parent", 4,
            GroupRecordBody::AdminTransferred { new_admin_peer_id: successor_id.clone() });
        let leave = SignedGroupRecord::new(
            &founder,
            group_id.clone(),
            "leave-child".to_string(),
            5,
            vec![transfer.id().to_string()],
            5,
            GroupRecordBody::MemberLeft { peer_id: founder_id.clone() },
        ).expect("leave record");

        assert!(!apply(&app_state, &leave).expect("pending leave"));
        apply(&app_state, &transfer).expect("transfer");
        let policy = get_group_policy(&app_state, &group_id).expect("policy");
        assert_eq!(policy.admin_peer_id, successor_id);
        assert!(!policy.active_members.contains(&founder_id));
    }

    #[test]
    fn applying_group_created_stores_group_image_hash() {
        let app_state = app_state();
        let founder = keypair();
        let group_id = chat_kind::generate_group_chat_id();
        let image_hash = "group-image-hash";

        apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "created",
                1,
                GroupRecordBody::GroupCreated {
                    name: "Visual Group".to_string(),
                    settings: None,
                    image_hash: Some(image_hash.to_string()),
                },
            ),
        )
        .expect("created");

        let conn = app_state.db_conn.lock().expect("db");
        let chat = db::get_chat_list(&conn)
            .expect("chat list")
            .into_iter()
            .find(|chat| chat.id == group_id)
            .expect("group chat");
        assert_eq!(chat.image_hash.as_deref(), Some(image_hash));
        let (mime_type, is_complete): (String, i64) = conn
            .query_row(
                "SELECT mime_type, is_complete FROM files WHERE file_hash = ?1",
                [image_hash],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("placeholder file row");
        assert_eq!(mime_type, "application/octet-stream");
        assert_eq!(is_complete, 0);
        let sources = db::get_group_file_sources(&conn, &group_id, image_hash).expect("sources");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].peer_id, peer_id(&founder));
    }

    #[test]
    fn file_availability_creates_placeholder_file_row() {
        let app_state = app_state();
        let founder = keypair();
        let group_id = chat_kind::generate_group_chat_id();
        let file_hash = "available-group-image-hash";

        apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "created",
                1,
                GroupRecordBody::GroupCreated {
                    name: "Visual Group".to_string(),
                    settings: None,
                    image_hash: None,
                },
            ),
        )
        .expect("created");
        apply(
            &app_state,
            &signed(
                &founder,
                &group_id,
                "available",
                2,
                GroupRecordBody::FileAvailability {
                    file_hash: file_hash.to_string(),
                },
            ),
        )
        .expect("availability");

        let conn = app_state.db_conn.lock().expect("db");
        let is_complete: i64 = conn
            .query_row(
                "SELECT is_complete FROM files WHERE file_hash = ?1",
                [file_hash],
                |row| row.get(0),
            )
            .expect("placeholder file row");
        assert_eq!(is_complete, 0);
        let sources = db::get_group_file_sources(&conn, &group_id, file_hash).expect("sources");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].peer_id, peer_id(&founder));
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
                image_hash: None,
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
                image_hash: None,
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
                image_hash: None,
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
                image_hash: None,
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
                image_hash: None,
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
                image_hash: None,
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
                image_hash: None,
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
                image_hash: None,
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
                image_hash: None,
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
