use anyhow::{anyhow, Context};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use libp2p::{identity, PeerId};

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
    if !chat_kind::is_group_chat_id(&group_id) {
        return Err(anyhow!("Invalid group id. Expected format group:<uuid>"));
    }
    let resolved_name = name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| chat_kind::default_group_name(&group_id));

    {
        let conn = app_state.db_conn.lock().map_err(|e| anyhow!(e.to_string()))?;
        db::upsert_chat(&conn, &group_id, &resolved_name, true)?;
        db::add_chat_member(&conn, &group_id, "Me", "member")?;
    }

    if let Some(network_state) = network_state {
        send_network_command(
            network_state,
            NetworkCommand::SubscribeGroup {
                group_id: group_id.clone(),
            },
        )
        .await?;
        send_network_command(
            network_state,
            NetworkCommand::SyncGroup {
                group_id: group_id.clone(),
            },
        )
        .await?;
    }

    Ok(GroupChatResult {
        chat_id: group_id,
        name: resolved_name,
    })
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

    for record in invite
        .related_records
        .iter()
        .chain(std::iter::once(&invite.invite_record))
    {
        apply_signed_record(app_state, None, record, true)?;
    }

    let keypair = load_or_create_local_keypair(app_state).await?;
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
    let record = sign_record(
        &keypair,
        group_id.clone(),
        GroupRecordBody::MemberLeft {
            peer_id: PeerId::from_public_key(&keypair.public()).to_string(),
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
    let keypair = load_or_create_local_keypair(app_state).await?;
    let record = sign_record(
        &keypair,
        group_id.clone(),
        GroupRecordBody::Receipt {
            message_ids,
            status: GroupReceiptStatus::Read,
        },
    )?;
    apply_signed_record(app_state, None, &record, true)?;
    send_network_command(network_state, NetworkCommand::PublishGroupRecord { record }).await
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
        if db::group_record_exists(&conn, record.id()) {
            return Ok(false);
        }
        record_applied = db::insert_group_record(&conn, record, verified, !verified)?;
        if !verified {
            return Ok(record_applied);
        }

        match record.body() {
            GroupRecordBody::GroupCreated { name } => {
                db::upsert_chat(&conn, record.group_id(), name, true)?;
                ensure_peer(&conn, record.author_peer_id(), "group")?;
                db::add_chat_member(&conn, record.group_id(), record.author_peer_id(), "admin")?;
                roster_event = Some(GroupRosterUpdatedEvent {
                    group_id: record.group_id().to_string(),
                    peer_id: record.author_peer_id().to_string(),
                    membership_state: "joined".to_string(),
                });
            }
            GroupRecordBody::MemberInvited { peer_id, role } => {
                ensure_peer(&conn, peer_id, "group")?;
                db::add_chat_member(&conn, record.group_id(), peer_id, role)?;
                roster_event = Some(GroupRosterUpdatedEvent {
                    group_id: record.group_id().to_string(),
                    peer_id: peer_id.clone(),
                    membership_state: "invited".to_string(),
                });
            }
            GroupRecordBody::MemberJoined { peer_id } => {
                ensure_peer(&conn, peer_id, "group")?;
                db::add_chat_member(&conn, record.group_id(), peer_id, "member")?;
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
                db::add_chat_member(&conn, record.group_id(), "Me", "member")?;
                db::add_chat_member(&conn, record.group_id(), record.author_peer_id(), "member")?;
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

    #[test]
    fn invite_gated_join_requires_existing_invite_payload() {
        let temp = tempfile::tempdir().expect("temp");
        let app_state = crate::runtime::create_app_state(temp.path().to_path_buf())
            .expect("app state");
        let conn = app_state.db_conn.lock().expect("db");
        let missing = db::get_group_invite_payload(&conn, "missing").expect("query");
        assert!(missing.is_none());
    }
}
